//! An outgoing dial reaches a peer that IS there, and what arrives is a
//! BitTorrent handshake.
//!
//! The engine's dials do not go through tokio's `TcpStream::connect` any more
//! (vendor/README-RILLIO.md, change 2: on Windows an overlapped `ConnectEx`
//! awaited on the blocking pool; elsewhere tokio's connect with its first poll
//! on the blocking pool). `held_connect.rs` pins what that buys; this pins
//! that the connection it produces still works end to end: connected, usable
//! for writes from the runtime, and closed cleanly.

use std::time::Duration;

use librqbit::{AddTorrent, AddTorrentOptions};
use rillio_streaming_server::Engine;
use tokio::io::AsyncReadExt;

const PIECE: usize = 16_384;

fn make_torrent(name: &str) -> Vec<u8> {
    let file = "E01.mkv";
    let mut info = Vec::new();
    info.extend_from_slice(format!("d5:filesld6:lengthi{PIECE}e4:pathl{}:{file}eee", file.len()).as_bytes());
    info.extend_from_slice(format!("4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(format!("12:piece lengthi{PIECE}e6:pieces20:").as_bytes());
    info.extend_from_slice(&[0u8; 20]);
    info.extend_from_slice(b"e");
    let mut t = Vec::new();
    t.extend_from_slice(b"d4:info");
    t.extend_from_slice(&info);
    t.extend_from_slice(b"e");
    t
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dial_reaches_a_listening_peer_with_a_handshake() {
    let peer = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let peer_addr = peer.local_addr().unwrap();

    let dir = std::env::temp_dir().join(format!("rillio-live-dial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir).await.unwrap();
    let options = AddTorrentOptions {
        only_files: Some(vec![0]),
        overwrite: true,
        initial_peers: Some(vec![peer_addr]),
        disable_trackers: true,
        ..Default::default()
    };
    let handle = engine
        .session()
        .add_torrent(AddTorrent::from_bytes(make_torrent("Live.Dial")), Some(options))
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await.unwrap();

    let (mut stream, from) = tokio::time::timeout(Duration::from_secs(15), peer.accept())
        .await
        .expect("the engine never dialed its one peer")
        .unwrap();
    assert!(from.ip().is_loopback());
    // <19><"BitTorrent protocol"><8 reserved><20 info hash><20 peer id>
    let mut handshake = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut handshake))
        .await
        .expect("the dialed connection carried no handshake within 10 s")
        .expect("reading the handshake");
    assert_eq!(handshake[0], 19);
    assert_eq!(&handshake[1..20], b"BitTorrent protocol");
    assert_eq!(&handshake[28..48], handle.info_hash().0.as_slice(), "the handshake names this torrent");

    engine.session().stop().await;
}
