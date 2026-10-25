//! "View location" (Engine::locate) against a real engine and torrent, and the
//! list's `watchedAt`.

use std::net::SocketAddr;

use rillio_streaming_server::location::{Location, LocationError};
use rillio_streaming_server::{router, Config, Engine};
use serde_json::Value;

/// Minimal multi-file `.torrent` (bencode), one piece.
fn make_multi_torrent(name: &str, files: &[(&str, u64)]) -> Vec<u8> {
    let total: u64 = files.iter().map(|(_, len)| len).sum();
    let mut info = Vec::new();
    info.extend_from_slice(b"d5:filesl");
    for (fname, len) in files {
        let components: String = fname.split('/').map(|part| format!("{}:{part}", part.len())).collect();
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

async fn spawn(tag: &str) -> (String, reqwest::Client, std::path::PathBuf, Engine) {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let dir = std::env::temp_dir().join(format!("rillio-location-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone()).await.unwrap();
    let app = router(Config::local(dir.clone()), engine.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, reqwest::Client::new(), dir, engine)
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

#[tokio::test]
async fn locates_files_of_a_cached_torrent_inside_the_cache_only() {
    let (base, c, dir, engine) = spawn("files").await;
    let ih = add(&c, &base, make_multi_torrent("Pack", &[("e01.mkv", 5_000), ("NTb.nfo", 100)])).await;
    let root = std::fs::canonicalize(&dir).unwrap();

    // Put the first file on disk where librqbit lays it out.
    std::fs::create_dir_all(dir.join("Pack")).unwrap();
    std::fs::write(dir.join("Pack").join("e01.mkv"), vec![0u8; 10]).unwrap();

    match engine.locate(&ih, Some(0)).unwrap() {
        Location::Item(path) => assert_eq!(path, root.join("Pack").join("e01.mkv")),
        other => panic!("expected the file itself, got {other:?}"),
    }
    // Not on disk (never downloaded, or removed): its folder instead. The
    // engine may have preallocated it on add; take it away.
    let _ = std::fs::remove_file(dir.join("Pack").join("NTb.nfo"));
    assert!(!dir.join("Pack").join("NTb.nfo").exists());
    match engine.locate(&ih, Some(1)).unwrap() {
        Location::Folder(path) => assert_eq!(path, root.join("Pack")),
        other => panic!("expected the folder, got {other:?}"),
    }
    // The download as a whole: a multi-file torrent is its own folder.
    match engine.locate(&ih, None).unwrap() {
        Location::Item(path) => assert_eq!(path, root.join("Pack")),
        other => panic!("expected the torrent folder, got {other:?}"),
    }
    assert_eq!(engine.locate(&ih, Some(7)), Err(LocationError::NoSuchFile));
    assert_eq!(engine.locate(&"0".repeat(40), None), Err(LocationError::NotInCache));
    assert_eq!(engine.locate("../../etc", None), Err(LocationError::BadInfoHash));
}

#[tokio::test]
async fn the_list_says_when_a_torrent_was_marked_watched() {
    let (base, c, _dir, _engine) = spawn("watched").await;
    let ih = add(&c, &base, make_multi_torrent("Show", &[("a.mkv", 5_000)])).await;
    let list: Vec<Value> = c.get(format!("{base}/cache/list")).send().await.unwrap().json().await.unwrap();
    assert!(list[0].get("watchedAt").is_none(), "absent until marked");

    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    c.post(format!("{base}/cache/watched"))
        .json(&serde_json::json!({ "infoHash": ih, "watched": true }))
        .send()
        .await
        .unwrap();
    let list: Vec<Value> = c.get(format!("{base}/cache/list")).send().await.unwrap().json().await.unwrap();
    let at = list[0]["watchedAt"].as_u64().expect("watchedAt in ms");
    // Stored in whole seconds, so up to a second before `before`.
    assert!(at + 1000 >= before && at <= before + 5_000, "watchedAt {at} vs now {before}");
}
