//! M1.5 - ConfinedStorage end-to-end: a quota-exceeding torrent is refused, and
//! a path-traversal torrent is rejected. Synthetic torrents, no network.

use std::net::SocketAddr;

use rillio_streaming_server::engine::Pick;
use rillio_streaming_server::{router, Config, Engine};

/// Build a minimal valid single-file `.torrent` (bencode) declaring `length`
/// bytes under `name`, with one piece. Enough for metadata parsing + our
/// storage-layer validation; piece hashes are not checked without data.
fn make_torrent(name: &str, length: u64) -> Vec<u8> {
    // info dict keys must be bencode-sorted: length, name, piece length, pieces.
    let mut info = Vec::new();
    info.extend_from_slice(b"d");
    info.extend_from_slice(format!("6:lengthi{length}e").as_bytes());
    info.extend_from_slice(format!("4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(format!("12:piece lengthi{length}e").as_bytes());
    // one piece => 20-byte pieces string
    info.extend_from_slice(b"6:pieces20:");
    info.extend_from_slice(&[0u8; 20]);
    info.extend_from_slice(b"e");

    let mut torrent = Vec::new();
    torrent.extend_from_slice(b"d4:info");
    torrent.extend_from_slice(&info);
    torrent.extend_from_slice(b"e");
    torrent
}

async fn engine(tag: &str) -> Engine {
    let dir = std::env::temp_dir().join(format!("stremio-confine-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Engine::new(dir).await.unwrap()
}

#[tokio::test]
async fn traversal_path_torrent_is_rejected() {
    // A name carrying a separator/traversal is rejected (librqbit-core at parse;
    // ConfinedStorage would independently reject the resolved path too).
    let engine = engine("traversal").await;
    let torrent = make_torrent("../../Startup/evil.exe", 1000);
    let result = engine.add_blob(torrent, Pick::Files(vec![0])).await;
    assert!(result.is_err(), "a traversal path must never be added");
}

#[tokio::test]
async fn large_torrent_not_rejected_by_size_quota() {
    // Regression for the size-quota bug: a movie far larger than the cache (a
    // 9 GB HDR film under a 2 GB cache) was rejected with "exceeds cache quota",
    // which broke basically every movie. A streaming server plays a window, not
    // the whole file, so there is no size gate. We assert the add is NOT refused
    // for a size/quota reason. (A degenerate synthetic single-8GB-piece torrent
    // may still error inside librqbit's own file handling; that is not our gate.)
    let engine = engine("large").await;
    let torrent = make_torrent("movie-8gb.mkv", 8_000_000_000);
    if let Err(e) = engine.add_blob(torrent, Pick::Files(vec![0])).await {
        let msg = e.to_string();
        assert!(
            !msg.contains("quota") && !msg.contains("exceeds"),
            "add must not be refused by a size quota, got: {msg}"
        );
    }
}

// Confidence check that the router still builds with the confined engine.
#[tokio::test]
async fn router_builds_with_confined_engine() {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let dir = std::env::temp_dir().join("stremio-confine-router");
    let _ = std::fs::create_dir_all(&dir);
    let engine = Engine::new(dir.clone()).await.unwrap();
    let _app = router(Config::local(dir), engine);
}
