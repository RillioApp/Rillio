//! Regression: changing a torrent's file selection while its peer connections
//! are dying must not deadlock the engine.
//!
//! librqbit 8.1.1, `TorrentStateLive::update_only_files`, took the torrent's
//! live-state write lock and then, still holding it, walked the peer table
//! (`reconnect_all_not_needed_peers`, a DashMap `iter_mut`: every shard's write
//! lock in turn). A peer task whose connection had just died did the opposite:
//! `PeerHandler::on_peer_died` holds that peer's table entry (its shard's write
//! lock) and, for a connection that died while still connecting, calls
//! `is_finished_and_no_active_streams`, which takes the live-state lock. Two
//! threads, two locks, opposite orders: each waits for the other forever.
//!
//! Everything that then asks the torrent for its state (the cache list, the
//! stats route, a stream) blocks behind the stuck write lock, a tokio worker at
//! a time, and the engine's own selection lock and mutation gate are held by
//! the stuck selection change. It is reachable in ordinary use: a selection
//! change is "play another episode of the pack" or the next-episode preload,
//! and connections die all the time in a real swarm.
//!
//! It was found when `file_selection`'s concurrent-plays test hung in CI on
//! Linux: that test's synthetic torrent is announced to the public trackers,
//! which hand back a few addresses, and those connections dying against 24
//! threads changing the selection was enough (10 of 40 runs at v0.1.48 on four
//! cores). This test makes the same race on purpose and offline: hundreds of
//! peers on loopback ports nobody listens on, so every connection attempt dies,
//! over and over, while the selection is toggled as fast as it will go.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use librqbit::{AddTorrent, AddTorrentOptions};
use rillio_streaming_server::Engine;

const PIECE: usize = 16_384;
const FILES: usize = 8;
/// Loopback ports with (as good as certainly) nobody behind them.
const DEAD_PEERS: u16 = 600;
/// How long the selection is hammered. Without the fix the deadlock lands
/// well inside this on every platform tried.
const HAMMER: Duration = Duration::from_secs(12);

/// A multi-file `.torrent`, one piece per file. The piece hashes are zeros:
/// nothing is ever downloaded or verified here.
fn make_torrent(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(b"d5:filesl");
    for i in 0..FILES {
        let file = format!("E{i:02}.mkv");
        info.extend_from_slice(format!("d6:lengthi{PIECE}e4:pathl{}:{file}ee", file.len()).as_bytes());
    }
    info.extend_from_slice(b"e");
    info.extend_from_slice(format!("4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(format!("12:piece lengthi{PIECE}e").as_bytes());
    info.extend_from_slice(format!("6:pieces{}:", FILES * 20).as_bytes());
    info.extend_from_slice(&vec![0u8; FILES * 20]);
    info.extend_from_slice(b"e");
    let mut t = Vec::new();
    t.extend_from_slice(b"d4:info");
    t.extend_from_slice(&info);
    t.extend_from_slice(b"e");
    t
}

async fn hammer_the_selection() -> usize {
    let dir = std::env::temp_dir().join("rillio-selection-vs-peer-churn");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir).await.unwrap();

    let dead_peers: Vec<SocketAddr> =
        (0..DEAD_PEERS).map(|i| SocketAddr::from(([127, 0, 0, 1], 20_000 + i))).collect();
    let options = AddTorrentOptions {
        only_files: Some(vec![0]),
        overwrite: true,
        initial_peers: Some(dead_peers),
        // Offline: the only peers are the dead ones above.
        disable_trackers: true,
        ..Default::default()
    };
    let handle = engine
        .session()
        .add_torrent(AddTorrent::from_bytes(make_torrent("Churn.Pack")), Some(options))
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await.unwrap();

    // Every toggle leaves something still to download, which is the branch of
    // librqbit's update that walks the peer table.
    let started = Instant::now();
    let mut changes = 0usize;
    while started.elapsed() < HAMMER {
        let file = 1 + changes % (FILES - 1);
        engine.set_file_selected(&handle, file, changes % 2 == 0).await.unwrap();
        changes += 1;
        if changes % 64 == 0 {
            tokio::task::yield_now().await;
        }
    }
    engine.session().stop().await;
    changes
}

#[test]
fn a_selection_change_does_not_deadlock_against_dying_peer_connections() {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
        let changes = rt.block_on(hammer_the_selection());
        let _ = done.send(changes);
        // The runtime is leaked on purpose when the test has already failed:
        // dropping it would wait on the very threads that are deadlocked.
        std::mem::forget(rt);
    });
    let changes = finished.recv_timeout(HAMMER + Duration::from_secs(45)).expect(
        "DEADLOCK: a selection change (holding the torrent's live-state lock, waiting for the \
         peer table) and a dying peer connection (holding its peer-table entry, waiting for the \
         live-state lock) are each waiting for the other",
    );
    assert!(changes > 0);
    println!("{changes} selection changes against {DEAD_PEERS} dying peers, no deadlock");
}
