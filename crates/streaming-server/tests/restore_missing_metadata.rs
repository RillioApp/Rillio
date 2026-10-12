//! Regression: ONE persisted torrent whose saved metadata is unusable must not
//! keep the whole engine from starting.
//!
//! librqbit restores a session by re-adding every entry of
//! `session/session.json` and AWAITS all of those adds before the session
//! exists. An entry whose `session/<infohash>.torrent` is missing or empty is
//! restored as a bare magnet, and a magnet add waits for a peer to deliver the
//! metadata with no deadline. With nobody to deliver it (a dead torrent, a
//! blocked network) `Engine::new` never returned: the streaming server never
//! came up, so the Cache window sat on "Loading..." and then said the service
//! was not reachable, the cached cards never showed, and nothing played, on
//! every launch, with nothing logged.
//!
//! librqbit rewrites every `.torrent` at every start (open, truncate, write),
//! so a process killed at the wrong moment (an installer, a crash, a power
//! cut) is enough to leave one empty.
//!
//! Offline by construction: nothing here can reach a peer, which is exactly
//! the "nobody delivers the metadata" case.

use std::path::{Path, PathBuf};
use std::time::Duration;

use librqbit_sha1_wrapper::{ISha1, Sha1};
use rillio_streaming_server::axum::body::Body;
use rillio_streaming_server::axum::http::Request;
use rillio_streaming_server::engine::Pick;
use rillio_streaming_server::tower::ServiceExt;
use rillio_streaming_server::{router, Config, Engine};
use serde_json::Value;

const PIECE: usize = 16_384;

/// A single-file `.torrent` of two pieces filled with `fill`, with real piece
/// hashes, so the file written beside it verifies as complete.
fn make_torrent(name: &str, fill: u8) -> (Vec<u8>, Vec<u8>) {
    let data = vec![fill; 2 * PIECE];
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi{}e", data.len()).as_bytes());
    info.extend_from_slice(format!("4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(format!("12:piece lengthi{PIECE}e").as_bytes());
    info.extend_from_slice(b"6:pieces40:");
    for chunk in data.chunks(PIECE) {
        let mut h = Sha1::new();
        h.update(chunk);
        info.extend_from_slice(&h.finish());
    }
    info.extend_from_slice(b"e");
    let mut t = Vec::new();
    t.extend_from_slice(b"d4:info");
    t.extend_from_slice(&info);
    t.extend_from_slice(b"e");
    (t, data)
}

async fn list(engine: &Engine, dir: &Path) -> Vec<Value> {
    let app = router(Config::local(dir.to_path_buf()), engine.clone());
    let resp = app
        .oneshot(Request::builder().uri("/cache/list").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let bytes = rillio_streaming_server::axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn wait_all_ready(engine: &Engine, dir: &Path, expected: usize) -> Vec<Value> {
    for _ in 0..200 {
        let entries = list(engine, dir).await;
        if entries.len() == expected && entries.iter().all(|e| e["state"] != "initializing") {
            return entries;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the cache never settled at {expected} ready torrents: {:?}", list(engine, dir).await);
}

/// A cache holding two complete torrents, engine stopped. Returns the cache
/// dir and the two infohashes (broken-to-be, healthy).
async fn seeded_cache(tag: &str) -> (PathBuf, String, String) {
    let dir = std::env::temp_dir().join(format!("rillio-restore-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (blob_a, data_a) = make_torrent("Broken.Movie.2019.1080p.mkv", 0x11);
    let (blob_b, data_b) = make_torrent("Healthy.Movie.2021.1080p.mkv", 0x22);
    std::fs::write(dir.join("Broken.Movie.2019.1080p.mkv"), data_a).unwrap();
    std::fs::write(dir.join("Healthy.Movie.2021.1080p.mkv"), data_b).unwrap();

    let engine = Engine::new(dir.clone()).await.unwrap();
    let a = Engine::info_hash_hex(&engine.add_blob(blob_a, Pick::Files(vec![0])).await.unwrap());
    let b = Engine::info_hash_hex(&engine.add_blob(blob_b, Pick::Files(vec![0])).await.unwrap());
    let entries = wait_all_ready(&engine, &dir, 2).await;
    assert!(
        entries.iter().all(|e| e["downloaded"] == e["total"]),
        "both seeded torrents verify complete: {entries:?}"
    );
    engine.session().stop().await;
    drop(engine);
    tokio::time::sleep(Duration::from_millis(300)).await;
    (dir, a, b)
}

/// Restart over the cache; the engine must exist within the deadline and the
/// healthy torrent must be listed and ready.
async fn restart_must_come_up(dir: &Path, healthy: &str) -> Engine {
    let engine = tokio::time::timeout(Duration::from_secs(20), Engine::new(dir.to_path_buf()))
        .await
        .expect(
            "Engine::new did not return within 20 s: one torrent with unusable saved \
             metadata is holding up the whole engine",
        )
        .unwrap();
    for _ in 0..200 {
        let entries = list(&engine, dir).await;
        if entries.iter().any(|e| e["infoHash"] == healthy && e["state"] != "initializing") {
            return engine;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the healthy torrent never came back: {:?}", list(&engine, dir).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_saved_torrent_file_does_not_block_startup() {
    let (dir, broken, healthy) = seeded_cache("empty").await;
    // What a kill in the middle of librqbit's truncate-then-write leaves.
    std::fs::write(dir.join("session").join(format!("{broken}.torrent")), b"").unwrap();

    let engine = restart_must_come_up(&dir, &healthy).await;
    engine.session().stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_saved_torrent_file_does_not_block_startup() {
    let (dir, broken, healthy) = seeded_cache("missing").await;
    std::fs::remove_file(dir.join("session").join(format!("{broken}.torrent"))).unwrap();

    let engine = restart_must_come_up(&dir, &healthy).await;
    engine.session().stop().await;
}

/// The torrent set aside is not forgotten: it is recorded for a later start to
/// try again, and it is restored (same files, no re-download) the moment its
/// metadata is available again.
#[tokio::test(flavor = "multi_thread")]
async fn a_torrent_set_aside_is_recorded_and_restored_once_its_metadata_is_back() {
    let (dir, broken, healthy) = seeded_cache("recover").await;
    let torrent_file = dir.join("session").join(format!("{broken}.torrent"));
    let saved = std::fs::read(&torrent_file).unwrap();
    std::fs::write(&torrent_file, b"").unwrap();

    let engine = restart_must_come_up(&dir, &healthy).await;
    let recorded: Value = serde_json::from_slice(
        &std::fs::read(dir.join("session").join("unrestored.json"))
            .expect("the torrent set aside is recorded in session/unrestored.json"),
    )
    .unwrap();
    assert!(
        recorded.as_array().is_some_and(|list| list.iter().any(|e| e["info_hash"] == broken)),
        "unrestored.json names the torrent: {recorded}"
    );
    engine.session().stop().await;
    drop(engine);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The metadata is back (standing in for "a peer delivered it").
    std::fs::write(&torrent_file, saved).unwrap();
    let engine = restart_must_come_up(&dir, &healthy).await;
    let entries = wait_all_ready(&engine, &dir, 2).await;
    let restored = entries.iter().find(|e| e["infoHash"] == broken).unwrap();
    assert_eq!(restored["downloaded"], restored["total"], "its data on disk was reused: {restored}");
    let recorded: Value =
        serde_json::from_slice(&std::fs::read(dir.join("session").join("unrestored.json")).unwrap())
            .unwrap();
    assert_eq!(recorded, serde_json::json!([]), "a restored torrent leaves the record");
    engine.session().stop().await;
}
