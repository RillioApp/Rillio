//! Regression: outgoing dials whose `connect()` call is HELD must not starve the
//! runtime the engine lives on.
//!
//! A non-blocking `connect()` is supposed to return at once. On a machine with a
//! kernel socket filter in the path it may not: measured 2026-10-13 with
//! AdGuard's socket driver, the call was held for the whole TCP handshake, which
//! for an address nobody answers is the full 20 s SYN timeout (20,007 ms, then
//! `WSAEWOULDBLOCK`), against 0.1 ms with the filter off. tokio issues that call
//! from a runtime worker thread, and a torrent engine dials dead addresses all
//! the time (up to 128 peers per live torrent), so every worker ended up parked
//! inside the kernel: `/heartbeat` took up to 10 s or never answered, and in the
//! desktop shell every window command (drag, minimize, maximize, close) waited
//! with it, because they ran on the same runtime.
//!
//! This test models the held call without any filter: librqbit's test hook makes
//! every dial sleep, synchronously, on the thread that issues `connect()`. With
//! the dials on the worker threads the runtime below (two workers, like a small
//! machine) is frozen for seconds at a time; with the fix the dial's system call
//! runs on the blocking pool and the workers never notice.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use librqbit::{AddTorrent, AddTorrentOptions};
use rillio_streaming_server::axum::body::Body;
use rillio_streaming_server::axum::http::Request;
use rillio_streaming_server::tower::ServiceExt;
use rillio_streaming_server::{router, Config, Engine};

const PIECE: usize = 16_384;
/// How long each dial's `connect()` is held. Far above the bound below, far
/// below the 20 s of the real thing so the test stays short.
const HOLD: Duration = Duration::from_millis(1_500);
/// Peers nobody answers (loopback ports with nothing behind them).
const DEAD_PEERS: u16 = 96;
/// How long the runtime is watched while the dials are in flight.
const WATCH: Duration = Duration::from_secs(4);
/// What "the runtime stayed responsive" means: no timer tick and no
/// `/heartbeat` later than this. A healthy runtime is three orders of
/// magnitude under it; a starved one is at `HOLD` or worse.
const BOUND: Duration = Duration::from_millis(400);

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

/// The worst lateness of a 20 ms timer and the slowest `/heartbeat`, both
/// measured from tasks ON the runtime, over `WATCH`.
async fn watch_the_runtime(app: rillio_streaming_server::axum::Router) -> (Duration, Duration) {
    let ticks = tokio::spawn(async move {
        let started = Instant::now();
        let mut worst = Duration::ZERO;
        while started.elapsed() < WATCH {
            let tick = Instant::now();
            tokio::time::sleep(Duration::from_millis(20)).await;
            worst = worst.max(tick.elapsed().saturating_sub(Duration::from_millis(20)));
        }
        worst
    });
    let beats = tokio::spawn(async move {
        let started = Instant::now();
        let mut worst = Duration::ZERO;
        while started.elapsed() < WATCH {
            let beat = Instant::now();
            let resp = app
                .clone()
                .oneshot(Request::builder().uri("/heartbeat").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert!(resp.status().is_success(), "/heartbeat answered {}", resp.status());
            worst = worst.max(beat.elapsed());
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        worst
    });
    (ticks.await.unwrap(), beats.await.unwrap())
}

async fn dial_a_dead_swarm_with_held_connects() -> (Duration, Duration) {
    let dir = std::env::temp_dir().join("rillio-held-connect");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone()).await.unwrap();
    let app = router(Config::local(dir), engine.clone());

    librqbit::rillio_test_hooks::hold_outgoing_connects(HOLD);

    let dead_peers: Vec<SocketAddr> =
        (0..DEAD_PEERS).map(|i| SocketAddr::from(([127, 0, 0, 1], 21_000 + i))).collect();
    let options = AddTorrentOptions {
        // One file, wanted and missing: the torrent keeps dialing its swarm.
        only_files: Some(vec![0]),
        overwrite: true,
        initial_peers: Some(dead_peers),
        // Offline: the only peers are the dead ones above.
        disable_trackers: true,
        ..Default::default()
    };
    let handle = engine
        .session()
        .add_torrent(AddTorrent::from_bytes(make_torrent("Held.Connect")), Some(options))
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await.unwrap();

    let worst = watch_the_runtime(app).await;
    librqbit::rillio_test_hooks::hold_outgoing_connects(Duration::ZERO);
    worst
}

#[test]
fn held_connects_do_not_starve_the_engine_runtime() {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Two workers: every worker is easy to occupy, as on a small machine.
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let worst = rt.block_on(dial_a_dead_swarm_with_held_connects());
        let _ = done.send(worst);
        // Leaked on purpose: dropping it would wait for the held dials.
        std::mem::forget(rt);
    });
    let (tick, beat) = finished
        .recv_timeout(WATCH + Duration::from_secs(120))
        .expect("STARVED: the runtime never got to finish the measurement while dials were held");
    println!("{DEAD_PEERS} dials held {HOLD:?} each: worst timer lateness {tick:?}, slowest /heartbeat {beat:?}");
    assert!(
        tick < BOUND,
        "a 20 ms timer on the engine's runtime fired {tick:?} late while outgoing dials were held: \
         the dials' connect() calls are occupying the runtime's worker threads"
    );
    assert!(
        beat < BOUND,
        "/heartbeat took {beat:?} while outgoing dials were held: \
         the dials' connect() calls are occupying the runtime's worker threads"
    );
}
