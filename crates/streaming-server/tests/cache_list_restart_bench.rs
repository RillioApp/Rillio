//! Measurement, not a pass/fail test: how long does `/cache/list` take to
//! answer after an engine RESTART over a large existing cache?
//!
//! Ignored by default (it needs tens of GB of sparse files and a directory to
//! put them in). Run it with:
//!
//! ```text
//! $env:RILLIO_BENCH_DIR = 'E:\ml-tools\rillio-cache-diag\bench'
//! cargo test --release --test cache_list_restart_bench -- --ignored --nocapture
//! ```
//!
//! Knobs (env): `RILLIO_BENCH_PACKS` (multi-file torrents, default 4),
//! `RILLIO_BENCH_PACK_FILES` (files per pack, default 10),
//! `RILLIO_BENCH_FILE_MIB` (size of each pack file, default 4096),
//! `RILLIO_BENCH_SINGLES` (single-file torrents, default 20),
//! `RILLIO_BENCH_SINGLE_MIB` (default 2048), `RILLIO_BENCH_WORKERS` (tokio
//! worker threads, default = CPU count like the shell's runtime).
//!
//! Offline by construction: synthetic `.torrent` blobs with REAL piece hashes
//! over sparse files (a nonzero head and tail so the stale-fastresume guard
//! keeps them), nothing ever needs a peer. The first run seeds the cache (a
//! full hash check, once); every run then restarts the engine over it and
//! times the list from the moment the engine exists until every torrent has
//! left `initializing`.
#![cfg(windows)]

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use librqbit_sha1_wrapper::{ISha1, Sha1};
use rillio_streaming_server::axum::body::Body;
use rillio_streaming_server::axum::http::Request;
use rillio_streaming_server::engine::Pick;
use rillio_streaming_server::tower::ServiceExt;
use rillio_streaming_server::{router, Config, Engine};

const PIECE: usize = 4 * 1024 * 1024;
const EDGE: usize = 64 * 1024;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(data);
    h.finish()
}

/// The three distinct pieces a synthetic file is made of.
struct Pieces {
    head: [u8; 20],
    zero: [u8; 20],
    tail: [u8; 20],
}

fn pieces() -> Pieces {
    let mut head = vec![0u8; PIECE];
    head[..EDGE].fill(0xA5);
    let mut tail = vec![0u8; PIECE];
    tail[PIECE - EDGE..].fill(0x5A);
    Pieces { head: sha1(&head), zero: sha1(&vec![0u8; PIECE]), tail: sha1(&tail) }
}

/// A sparse file of `n_pieces` whole pieces: nonzero first and last 64 KiB,
/// zeros (never allocated) in between.
fn write_sparse(path: &Path, n_pieces: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = std::fs::File::create(path).unwrap();
    let status = std::process::Command::new("fsutil")
        .args(["sparse", "setflag"])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "fsutil sparse setflag failed for {path:?}");
    let len = (n_pieces * PIECE) as u64;
    f.set_len(len).unwrap();
    f.write_all(&vec![0xA5u8; EDGE]).unwrap();
    f.seek(SeekFrom::Start(len - EDGE as u64)).unwrap();
    f.write_all(&vec![0x5Au8; EDGE]).unwrap();
}

/// `.torrent` bytes for files of `n_pieces` each (>= 2). One file makes a
/// single-file torrent (stored at the cache root), several a multi-file one
/// (stored under `name/`).
fn make_torrent(name: &str, files: &[String], n_pieces: usize, p: &Pieces) -> Vec<u8> {
    assert!(n_pieces >= 2);
    let mut info = Vec::new();
    info.extend_from_slice(b"d");
    if files.len() > 1 {
        info.extend_from_slice(b"5:filesl");
        for f in files {
            info.extend_from_slice(
                format!("d6:lengthi{}e4:pathl{}:{f}ee", n_pieces * PIECE, f.len()).as_bytes(),
            );
        }
        info.extend_from_slice(b"e");
    } else {
        info.extend_from_slice(format!("6:lengthi{}e", n_pieces * PIECE).as_bytes());
    }
    info.extend_from_slice(format!("4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(format!("12:piece lengthi{PIECE}e").as_bytes());
    info.extend_from_slice(format!("6:pieces{}:", files.len() * n_pieces * 20).as_bytes());
    for _ in files {
        info.extend_from_slice(&p.head);
        for _ in 0..n_pieces - 2 {
            info.extend_from_slice(&p.zero);
        }
        info.extend_from_slice(&p.tail);
    }
    info.extend_from_slice(b"e");
    let mut t = Vec::new();
    t.extend_from_slice(b"d4:info");
    t.extend_from_slice(&info);
    t.extend_from_slice(b"e");
    t
}

async fn list(app: &rillio_streaming_server::axum::Router) -> Vec<serde_json::Value> {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/cache/list").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(resp.status().is_success(), "/cache/list answered {}", resp.status());
    let bytes = rillio_streaming_server::axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn count_state(entries: &[serde_json::Value], state: &str) -> usize {
    entries.iter().filter(|e| e["state"] == state).count()
}

async fn seed(dir: &Path) {
    let packs = env_usize("RILLIO_BENCH_PACKS", 4);
    let pack_files = env_usize("RILLIO_BENCH_PACK_FILES", 10);
    let file_pieces = (env_usize("RILLIO_BENCH_FILE_MIB", 4096) * 1024 * 1024 / PIECE).max(2);
    let singles = env_usize("RILLIO_BENCH_SINGLES", 20);
    let single_pieces = (env_usize("RILLIO_BENCH_SINGLE_MIB", 2048) * 1024 * 1024 / PIECE).max(2);
    let p = pieces();

    let mut blobs: Vec<(Vec<u8>, usize)> = Vec::new();
    for i in 0..packs {
        let name = format!("Bench.Show.S{:02}.2160p", i + 1);
        let files: Vec<String> =
            (0..pack_files).map(|e| format!("Bench.Show.S{:02}E{:02}.mkv", i + 1, e + 1)).collect();
        for f in &files {
            write_sparse(&dir.join(&name).join(f), file_pieces);
        }
        blobs.push((make_torrent(&name, &files, file_pieces, &p), files.len()));
    }
    for i in 0..singles {
        let name = format!("Bench.Movie.{i:03}.1080p.mkv");
        write_sparse(&dir.join(&name), single_pieces);
        blobs.push((make_torrent(&name, std::slice::from_ref(&name), single_pieces, &p), 1));
    }

    let started = Instant::now();
    let engine = Engine::new(dir.to_path_buf()).await.unwrap();
    for (blob, n_files) in blobs {
        engine.add_blob(blob, Pick::Files((0..n_files).collect())).await.unwrap();
    }
    let app = router(Config::local(dir.to_path_buf()), engine.clone());
    loop {
        let entries = list(&app).await;
        assert_eq!(count_state(&entries, "error"), 0, "a seeded torrent errored: {entries:?}");
        if count_state(&entries, "initializing") == 0 {
            let incomplete: Vec<_> =
                entries.iter().filter(|e| e["downloaded"] != e["total"]).collect();
            assert!(incomplete.is_empty(), "seeded torrents must verify complete: {incomplete:?}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!("seeded in {:?} (full hash check of every file, once)", started.elapsed());
    engine.session().stop().await;
    drop(app);
    drop(engine);
    tokio::time::sleep(Duration::from_secs(2)).await;
    std::fs::write(dir.join("SEEDED"), b"ok").unwrap();
}

async fn restart_and_measure(dir: PathBuf) {
    let t0 = Instant::now();
    let engine = Engine::new(dir.clone()).await.unwrap();
    let engine_new = t0.elapsed();
    let app = router(Config::local(dir.clone()), engine.clone());

    // Runtime responsiveness, independent of the list: how late does a 10 ms
    // sleep wake up while the torrents initialize?
    let max_lag = Arc::new(Mutex::new(Duration::ZERO));
    let lag = max_lag.clone();
    let heartbeat = tokio::spawn(async move {
        loop {
            let before = Instant::now();
            tokio::time::sleep(Duration::from_millis(10)).await;
            let late = before.elapsed().saturating_sub(Duration::from_millis(10));
            let mut m = lag.lock().unwrap();
            if late > *m {
                *m = late;
            }
        }
    });

    // Like the web: a poll on a timer that does NOT wait for the previous one.
    let latencies: Arc<Mutex<Vec<(Duration, Duration, usize, usize)>>> = Arc::default();
    let all_live = Arc::new(tokio::sync::Notify::new());
    let poller = {
        let app = app.clone();
        let latencies = latencies.clone();
        let all_live = all_live.clone();
        tokio::spawn(async move {
            loop {
                let app = app.clone();
                let latencies = latencies.clone();
                let all_live = all_live.clone();
                tokio::spawn(async move {
                    let at = t0.elapsed();
                    let started = Instant::now();
                    let entries = list(&app).await;
                    let took = started.elapsed();
                    let initializing = count_state(&entries, "initializing");
                    latencies.lock().unwrap().push((at, took, initializing, entries.len()));
                    if initializing == 0 {
                        all_live.notify_one();
                    }
                });
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
    };

    let finished = tokio::time::timeout(Duration::from_secs(1800), all_live.notified()).await;
    let ready_after = t0.elapsed();
    poller.abort();
    heartbeat.abort();

    let samples = latencies.lock().unwrap().clone();
    let mut took: Vec<Duration> = samples.iter().map(|s| s.1).collect();
    took.sort();
    let pct = |p: f64| took[((took.len() - 1) as f64 * p) as usize];
    println!("--- restart over {:?}", dir);
    println!("Engine::new (the server is not reachable before this returns): {engine_new:?}");
    println!("every torrent left 'initializing' after: {ready_after:?} (timed out: {})", finished.is_err());
    println!(
        "/cache/list calls answered: {}  p50 {:?}  p95 {:?}  max {:?}",
        took.len(),
        pct(0.5),
        pct(0.95),
        took[took.len() - 1]
    );
    println!("max tokio wake-up lag: {:?}", *max_lag.lock().unwrap());
    for (at, took, initializing, total) in samples.iter().step_by((samples.len() / 12).max(1)) {
        println!("  t={at:>10.3?}  took {took:>10.3?}  initializing {initializing}/{total}");
    }
    engine.session().stop().await;
}

/// The overlap case: a second engine opens the cache while a FIRST one (the
/// previous app instance, still shutting down) holds it.
async fn overlap(dir: PathBuf) {
    let first = Engine::new(dir.clone()).await.unwrap();
    let first_app = router(Config::local(dir.clone()), first.clone());
    while count_state(&list(&first_app).await, "initializing") > 0 {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    println!("--- overlap: first engine is live over {dir:?}; starting a second one beside it");
    let t0 = Instant::now();
    match tokio::time::timeout(Duration::from_secs(120), Engine::new(dir.clone())).await {
        Err(_) => println!("second Engine::new did NOT return within 120 s"),
        Ok(Err(e)) => println!("second Engine::new FAILED after {:?}: {e:#}", t0.elapsed()),
        Ok(Ok(second)) => {
            println!("second Engine::new returned after {:?}", t0.elapsed());
            let app = router(Config::local(dir.clone()), second.clone());
            for _ in 0..40 {
                let started = Instant::now();
                let entries = list(&app).await;
                println!(
                    "  t={:>9.3?} list took {:>9.3?}: {} entries, {} initializing, {} error",
                    t0.elapsed(),
                    started.elapsed(),
                    entries.len(),
                    count_state(&entries, "initializing"),
                    count_state(&entries, "error"),
                );
                if count_state(&entries, "initializing") == 0 {
                    for e in entries.iter().filter(|e| e["state"] == "error").take(3) {
                        println!("    error entry: {}", e["error"]);
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            second.session().stop().await;
        }
    }
    first.session().stop().await;
}

#[test]
#[ignore = "measurement; needs a cache seeded by cache_list_latency_after_restart"]
fn second_engine_beside_a_live_one() {
    let dir = PathBuf::from(std::env::var("RILLIO_BENCH_DIR").expect("set RILLIO_BENCH_DIR"));
    assert!(dir.join("SEEDED").exists(), "seed the cache first");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(overlap(dir));
    rt.shutdown_timeout(Duration::from_secs(5));
}

#[test]
#[ignore = "measurement over tens of GB of sparse files; set RILLIO_BENCH_DIR"]
fn cache_list_latency_after_restart() {
    let dir = PathBuf::from(
        std::env::var("RILLIO_BENCH_DIR").expect("set RILLIO_BENCH_DIR to an empty scratch dir"),
    );
    std::fs::create_dir_all(&dir).unwrap();
    let workers = env_usize(
        "RILLIO_BENCH_WORKERS",
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
    );
    let runtime = || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .enable_all()
            .build()
            .unwrap()
    };
    if !dir.join("SEEDED").exists() {
        let rt = runtime();
        rt.block_on(seed(&dir));
        rt.shutdown_timeout(Duration::from_secs(5));
    }
    // A fresh runtime per restart, like a fresh process.
    let rt = runtime();
    rt.block_on(restart_and_measure(dir));
    rt.shutdown_timeout(Duration::from_secs(5));
}
