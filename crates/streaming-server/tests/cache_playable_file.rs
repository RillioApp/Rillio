//! Regression: the Cache page's "playable file" resolution.
//!
//! `/cache/list` reports `fileIdx` so the Cached page can offer Play. It used to
//! mean "exactly one file is selected", which broke on real scene releases: a
//! movie shipping an .nfo beside the .mkv reported two files, so no fileIdx, so
//! a finished 4 GB movie rendered with no Play button. The rule is now "exactly
//! one VIDEO among the selected files".

use std::net::SocketAddr;

use rillio_streaming_server::{router, Config, Engine};
use serde_json::Value;

/// Minimal multi-file `.torrent` (bencode), one piece. Dict keys are emitted in
/// lexicographic order, as bencode requires.
fn make_multi_torrent(name: &str, files: &[(&str, u64)]) -> Vec<u8> {
    let total: u64 = files.iter().map(|(_, len)| len).sum();
    let mut info = Vec::new();
    info.extend_from_slice(b"d5:filesl");
    for (fname, len) in files {
        // `path` is a LIST of components: a nested file is ["extras", "clip.mp4"],
        // never one string with a slash in it (librqbit rejects that torrent).
        let components: String = fname
            .split('/')
            .map(|part| format!("{}:{part}", part.len()))
            .collect();
        info.extend_from_slice(format!("d6:lengthi{len}e4:pathl{components}ee").as_bytes());
    }
    info.extend_from_slice(b"e");
    info.extend_from_slice(format!("4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(format!("12:piece lengthi{total}e").as_bytes());
    info.extend_from_slice(b"6:pieces20:");
    info.extend_from_slice(&[0u8; 20]);
    info.extend_from_slice(b"e");
    let mut t = Vec::new();
    t.extend_from_slice(b"d4:info");
    t.extend_from_slice(&info);
    t.extend_from_slice(b"e");
    t
}

async fn spawn(tag: &str) -> (String, reqwest::Client) {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let dir = std::env::temp_dir().join(format!("rillio-playable-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    let engine = Engine::new(dir.clone()).await.unwrap();
    let app = router(Config::local(dir), engine);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, reqwest::Client::new())
}

async fn add(c: &reqwest::Client, base: &str, blob: Vec<u8>) -> String {
    let hex: String = blob.iter().map(|b| format!("{b:02x}")).collect();
    let created: Value = c
        .post(format!("{base}/create"))
        .json(&serde_json::json!({ "blob": hex }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    created["infoHash"].as_str().unwrap_or_else(|| panic!("create failed: {created}")).to_owned()
}

/// A fresh add selects at most the main feature (tests/file_selection.rs), so
/// these tests select every file explicitly, the Cache page's file-browser
/// path, to exercise the playable-file rule over a multi-file SELECTION.
/// Waits out the hash check first: librqbit refuses selection changes on an
/// initializing torrent.
async fn select_all(c: &reqwest::Client, base: &str, ih: &str, count: usize) {
    for _ in 0..100 {
        if only_entry(c, base).await["state"] != "initializing" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    for idx in 0..count {
        let resp = c
            .post(format!("{base}/cache/select"))
            .json(&serde_json::json!({ "infoHash": ih, "fileIdx": idx, "selected": true }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "select {idx} failed: {status} {body}");
    }
}

async fn only_entry(c: &reqwest::Client, base: &str) -> Value {
    let list: Vec<Value> = c
        .get(format!("{base}/cache/list"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1, "expected exactly one cache entry, got {list:?}");
    list.into_iter().next().unwrap()
}

#[tokio::test]
async fn movie_with_an_nfo_beside_it_is_still_playable() {
    let (base, c) = spawn("nfo").await;
    let ih = add(
        &c,
        &base,
        make_multi_torrent("Some.Movie.2026.1080p", &[("Some.Movie.2026.1080p.mkv", 4_000_000), ("Some.Movie.2026.1080p.nfo", 2_689)]),
    )
    .await;
    select_all(&c, &base, &ih, 2).await;

    let entry = only_entry(&c, &base).await;
    // Both files are selected (honest count), but the lone VIDEO is playable.
    assert_eq!(entry["fileCount"], 2);
    assert_eq!(entry["fileIdx"], 0, "the .mkv must be the playable file");
    // The row names the movie, not the torrent/folder.
    assert_eq!(entry["name"], "Some.Movie.2026.1080p.mkv");
}

#[tokio::test]
async fn a_movie_beside_its_extras_still_plays_the_feature() {
    let (base, c) = spawn("extras").await;
    let ih = add(
        &c,
        &base,
        make_multi_torrent(
            "Some.Movie.2026",
            &[("Some.Movie.2026.mkv", 4_000_000), ("extras/behind.the.scenes.mp4", 900_000)],
        ),
    )
    .await;
    select_all(&c, &base, &ih, 2).await;

    let entry = only_entry(&c, &base).await;
    assert_eq!(entry["fileCount"], 2);
    // Two videos, but not a coin flip: the feature dwarfs the extra.
    assert_eq!(entry["fileIdx"], 0);
    assert_eq!(entry["name"], "Some.Movie.2026.mkv");
}

#[tokio::test]
async fn season_pack_stays_ambiguous() {
    let (base, c) = spawn("pack").await;
    let ih = add(
        &c,
        &base,
        make_multi_torrent("Some.Show.S01", &[("S01E01.mkv", 1_000_000), ("S01E02.mkv", 1_000_000)]),
    )
    .await;
    select_all(&c, &base, &ih, 2).await;

    let entry = only_entry(&c, &base).await;
    // Two videos: which one would Play mean? Correctly offers none.
    assert_eq!(entry["fileCount"], 2);
    assert!(entry.get("fileIdx").is_none(), "a season pack must not claim a playable file");
    assert_eq!(entry["name"], "Some.Show.S01");
}

#[tokio::test]
async fn selection_with_no_video_is_not_playable() {
    let (base, c) = spawn("novideo").await;
    let ih = add(&c, &base, make_multi_torrent("Just.Extras", &[("readme.nfo", 100), ("cover.jpg", 2_000)])).await;
    select_all(&c, &base, &ih, 2).await;

    let entry = only_entry(&c, &base).await;
    assert_eq!(entry["fileCount"], 2);
    assert!(entry.get("fileIdx").is_none());
    assert_eq!(entry["name"], "Just.Extras");
}
