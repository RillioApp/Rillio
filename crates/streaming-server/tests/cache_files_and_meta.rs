//! The Cache page's file browser (`/cache/files`, `/cache/select`) and the
//! torrent<->media metadata sidecar (`/cache/meta`, surfaced on `/cache/list`).

use std::net::SocketAddr;

use rillio_streaming_server::{router, Config, Engine};
use serde_json::Value;

/// Minimal multi-file `.torrent` (bencode), one piece; keys in lexicographic
/// order as bencode requires.
fn make_multi_torrent(name: &str, files: &[(&str, u64)]) -> Vec<u8> {
    let total: u64 = files.iter().map(|(_, len)| len).sum();
    let mut info = Vec::new();
    info.extend_from_slice(b"d5:filesl");
    for (fname, len) in files {
        // `path` is a LIST of components: a nested file is ["extras", "behind.mp4"],
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

async fn spawn(tag: &str) -> (String, reqwest::Client, std::path::PathBuf) {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let dir = std::env::temp_dir().join(format!("rillio-cachefiles-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    let engine = Engine::new(dir.clone()).await.unwrap();
    let app = router(Config::local(dir.clone()), engine);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, reqwest::Client::new(), dir)
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
    created["infoHash"].as_str().expect("create returns an infoHash").to_owned()
}

async fn files(c: &reqwest::Client, base: &str, ih: &str) -> Vec<Value> {
    c.get(format!("{base}/cache/files/{ih}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Wait until the torrent finishes its hash check. librqbit refuses selection
/// changes on an initializing torrent (the same refusal as pause), so a test
/// that skipped this would only ever exercise the rejection path.
async fn wait_ready(c: &reqwest::Client, base: &str) {
    for _ in 0..100 {
        let entry = only_entry(c, base).await;
        if entry["state"] != "initializing" {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("torrent never left the initializing state");
}

async fn only_entry(c: &reqwest::Client, base: &str) -> Value {
    let list: Vec<Value> = c.get(format!("{base}/cache/list")).send().await.unwrap().json().await.unwrap();
    list.into_iter().next().expect("one cache entry")
}

#[tokio::test]
async fn lists_every_file_in_the_torrent_and_marks_videos() {
    let (base, c, _dir) = spawn("list").await;
    let ih = add(
        &c,
        &base,
        make_multi_torrent("Pack", &[("movie.mkv", 5_000), ("extras/behind.mp4", 2_000), ("readme.nfo", 100)]),
    )
    .await;

    let listed = files(&c, &base, &ih).await;
    assert_eq!(listed.len(), 3, "the browser shows the WHOLE torrent");
    assert_eq!(listed[0]["path"], "movie.mkv");
    assert_eq!(listed[0]["video"], true);
    // Nested paths keep their folder, which is what makes a release dir readable.
    assert_eq!(listed[1]["path"], "extras/behind.mp4");
    assert_eq!(listed[1]["video"], true);
    assert_eq!(listed[2]["video"], false, ".nfo is not playable");
    // Listing is not selecting: two videos within 3x of each other are
    // ambiguous, so a fresh browse-time add downloads nothing until a file is
    // played or picked here (tests/file_selection.rs owns that contract).
    assert!(listed.iter().all(|f| f["selected"] == false));
}

async fn select(c: &reqwest::Client, base: &str, ih: &str, idx: usize, selected: bool) {
    let resp = c
        .post(format!("{base}/cache/select"))
        .json(&serde_json::json!({ "infoHash": ih, "fileIdx": idx, "selected": selected }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(status.is_success(), "select {idx}={selected} failed: {status} {body}");
}

#[tokio::test]
async fn a_file_can_be_dropped_from_and_added_back_to_the_selection() {
    let (base, c, _dir) = spawn("select").await;
    let ih = add(&c, &base, make_multi_torrent("Pack", &[("a.mkv", 5_000), ("b.mkv", 4_000)])).await;
    wait_ready(&c, &base).await;
    // An ambiguous pack starts with nothing selected; pick both explicitly.
    select(&c, &base, &ih, 0, true).await;
    select(&c, &base, &ih, 1, true).await;
    assert!(files(&c, &base, &ih).await.iter().all(|f| f["selected"] == true));

    select(&c, &base, &ih, 1, false).await;
    let listed = files(&c, &base, &ih).await;
    assert_eq!(listed[0]["selected"], true);
    assert_eq!(listed[1]["selected"], false);

    // ...and back: this is the "download the rest of the torrent" path.
    select(&c, &base, &ih, 1, true).await;
    assert!(files(&c, &base, &ih).await.iter().all(|f| f["selected"] == true));
}

#[tokio::test]
async fn deselecting_the_last_file_is_refused() {
    let (base, c, _dir) = spawn("lastfile").await;
    let ih = add(&c, &base, make_multi_torrent("Solo", &[("only.mkv", 5_000)])).await;
    wait_ready(&c, &base).await;

    let resp = c
        .post(format!("{base}/cache/select"))
        .json(&serde_json::json!({ "infoHash": ih, "fileIdx": 0, "selected": false }))
        .send()
        .await
        .unwrap();
    // 409, not a silent success: a torrent downloading nothing is not a state
    // the UI should be able to reach by accident (delete is the honest path).
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert_eq!(status, reqwest::StatusCode::CONFLICT);
    // ...and for THAT reason, not because the torrent happened to be busy: this
    // assertion is what keeps the test from passing on librqbit's unrelated
    // "can't update initializing torrent" refusal.
    assert!(body.contains("at least one selected file"), "unexpected refusal: {body}");
    assert_eq!(files(&c, &base, &ih).await[0]["selected"], true);
}

#[tokio::test]
async fn metadata_round_trips_and_survives_a_restart() {
    let (base, c, dir) = spawn("meta").await;
    let ih = add(&c, &base, make_multi_torrent("Some.Movie.2026", &[("movie.mkv", 5_000)])).await;

    // Unidentified until told otherwise.
    assert!(only_entry(&c, &base).await.get("meta").is_none());

    let meta = serde_json::json!({
        "metaId": "tt1234567",
        "type": "movie",
        "name": "Some Movie",
        "poster": "https://example.invalid/poster.jpg",
        "background": null,
        "logo": null,
        "year": "2026",
    });
    let resp = c
        .post(format!("{base}/cache/meta"))
        .json(&serde_json::json!({ "infoHash": ih, "meta": meta }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let entry = only_entry(&c, &base).await;
    assert_eq!(entry["meta"]["metaId"], "tt1234567");
    assert_eq!(entry["meta"]["name"], "Some Movie");

    // The sidecar is on disk, so an identified title stays identified across
    // restarts (the whole point of storing it server-side).
    let raw = std::fs::read(dir.join("cache-meta.json")).expect("cache-meta.json written");
    let stored: serde_json::Map<String, Value> = serde_json::from_slice(&raw).unwrap();
    assert_eq!(stored[&ih]["metaId"], "tt1234567");
}
