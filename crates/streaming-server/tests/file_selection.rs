//! Regression: playing one file of a torrent must download THAT file, not the
//! whole torrent.
//!
//! Every add used to leave librqbit's `only_files` at `None` ("every file
//! selected"), and the per-file selection helper was a no-op on `None`, so
//! streaming one episode of a season pack (or a movie beside its extras) pulled
//! the entire pack into the cache. These tests pin the selection contract of
//! every entry point that adds or plays a torrent: the stream route, the
//! browse-time create, `/cache/download` (with and without a file index, the
//! next-episode preload), the explicit `/cache/select`, and a legacy torrent
//! whose persisted selection is still "all".
//!
//! Offline by construction: the torrents are synthetic `.torrent` blobs with
//! REAL piece hashes (so a file written to disk beforehand verifies as
//! complete), each file exactly piece-aligned, and nothing ever connects to a
//! peer. Streams are probed with HEAD, which resolves and selects the file but
//! never opens a body (a GET would park on pieces no peer will ever send).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use librqbit_sha1_wrapper::{ISha1, Sha1};
use rillio_streaming_server::{router, Config, Engine};
use serde_json::Value;

const PIECE: usize = 16_384;

/// One file of a synthetic torrent: its path (slash-separated components),
/// the byte it is filled with, and its length in whole pieces.
struct F<'a> {
    path: &'a str,
    fill: u8,
    pieces: usize,
}

impl F<'_> {
    fn data(&self) -> Vec<u8> {
        vec![self.fill; self.pieces * PIECE]
    }
}

/// A multi-file `.torrent` whose files are each exactly piece-aligned and whose
/// `pieces` are the real SHA-1s of the file contents. Bencode dict keys are in
/// lexicographic order. `path` is a LIST of components, never one string with a
/// slash in it (librqbit rejects that torrent and /create answers an empty 500).
fn make_torrent(name: &str, files: &[F]) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(b"d5:filesl");
    for f in files {
        let components: String =
            f.path.split('/').map(|part| format!("{}:{part}", part.len())).collect();
        info.extend_from_slice(
            format!("d6:lengthi{}e4:pathl{components}ee", f.pieces * PIECE).as_bytes(),
        );
    }
    info.extend_from_slice(b"e");
    info.extend_from_slice(format!("4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(format!("12:piece lengthi{PIECE}e").as_bytes());
    let total_pieces: usize = files.iter().map(|f| f.pieces).sum();
    info.extend_from_slice(format!("6:pieces{}:", total_pieces * 20).as_bytes());
    for f in files {
        for chunk in f.data().chunks(PIECE) {
            let mut h = Sha1::new();
            h.update(chunk);
            info.extend_from_slice(&h.finish());
        }
    }
    info.extend_from_slice(b"e");
    let mut t = Vec::new();
    t.extend_from_slice(b"d4:info");
    t.extend_from_slice(&info);
    t.extend_from_slice(b"e");
    t
}

struct Server {
    base: String,
    c: reqwest::Client,
    dir: PathBuf,
    engine: Engine,
}

async fn spawn(tag: &str) -> Server {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let dir = std::env::temp_dir().join(format!("rillio-file-selection-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone()).await.unwrap();
    let app = router(Config::local(dir.clone()), engine.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { base, c: reqwest::Client::new(), dir, engine }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Server {
    /// `POST /create` with a `.torrent` blob (the "open a torrent file" flow).
    async fn create(&self, blob: &[u8]) -> String {
        let resp = self
            .c
            .post(format!("{}/create", self.base))
            .json(&serde_json::json!({ "blob": hex(blob) }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        assert!(status.is_success(), "/create failed: {status} {body}");
        let created: Value = serde_json::from_str(&body).unwrap();
        created["infoHash"].as_str().expect("create returns an infoHash").to_owned()
    }

    /// The per-file selection flags, in torrent order.
    async fn selected(&self, ih: &str) -> Vec<bool> {
        self.files(ih).await.iter().map(|f| f["selected"] == true).collect()
    }

    async fn files(&self, ih: &str) -> Vec<Value> {
        self.c
            .get(format!("{}/cache/files/{ih}", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn entry(&self, ih: &str) -> Value {
        let list: Vec<Value> =
            self.c.get(format!("{}/cache/list", self.base)).send().await.unwrap().json().await.unwrap();
        list.into_iter()
            .find(|e| e["infoHash"] == ih)
            .unwrap_or_else(|| panic!("{ih} is not in /cache/list"))
    }

    /// librqbit refuses selection changes while a torrent hash-checks ("can't
    /// update initializing torrent"), so every test waits this out first;
    /// otherwise it would only ever exercise that refusal.
    async fn wait_ready(&self, ih: &str) {
        for _ in 0..200 {
            let state = self.entry(ih).await["state"].clone();
            if state != "initializing" {
                assert_ne!(state, "error", "torrent went to error: {:?}", self.entry(ih).await);
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("torrent never left the initializing state");
    }

    /// Probe the stream route the way a player's first request does.
    async fn head_stream(&self, ih: &str, idx: &str) -> reqwest::StatusCode {
        self.c.head(format!("{}/{ih}/{idx}", self.base)).send().await.unwrap().status()
    }

    async fn post(&self, path: &str, body: Value) -> (reqwest::StatusCode, String) {
        let resp = self.c.post(format!("{}/{path}", self.base)).json(&body).send().await.unwrap();
        let status = resp.status();
        (status, resp.text().await.unwrap_or_default())
    }
}

fn season_pack() -> Vec<u8> {
    make_torrent(
        "Some.Show.S01",
        &[
            F { path: "Some.Show.S01E01.mkv", fill: 0x11, pieces: 2 },
            F { path: "Some.Show.S01E02.mkv", fill: 0x22, pieces: 2 },
            F { path: "Some.Show.S01E03.mkv", fill: 0x33, pieces: 2 },
        ],
    )
}

/// THE BUG: streaming episode 2 of a season pack selected the whole pack.
#[tokio::test]
async fn streaming_one_episode_of_a_pack_selects_only_that_episode() {
    let s = spawn("stream-one").await;
    let ih = s.create(&season_pack()).await;
    s.wait_ready(&ih).await;

    assert_eq!(s.head_stream(&ih, "1").await, reqwest::StatusCode::OK);
    assert_eq!(
        s.selected(&ih).await,
        vec![false, true, false],
        "only the streamed episode may download"
    );

    // mpv opens many connections per title; repeats must not widen anything.
    assert_eq!(s.head_stream(&ih, "1").await, reqwest::StatusCode::OK);
    assert_eq!(s.selected(&ih).await, vec![false, true, false]);
}

/// `/{ih}/-1` (no index: the stream route guesses the largest media file) must
/// select the one file it resolves to, never "everything because no index".
#[tokio::test]
async fn streaming_without_an_index_selects_the_guessed_file() {
    let s = spawn("stream-guess").await;
    let ih = s
        .create(&make_torrent(
            "Some.Show.S01",
            &[
                F { path: "E01.mkv", fill: 0x11, pieces: 2 },
                F { path: "E02.mkv", fill: 0x22, pieces: 3 },
                F { path: "E03.mkv", fill: 0x33, pieces: 2 },
            ],
        ))
        .await;
    s.wait_ready(&ih).await;

    assert_eq!(s.head_stream(&ih, "-1").await, reqwest::StatusCode::OK);
    assert_eq!(s.selected(&ih).await, vec![false, true, false]);
}

/// Opening a torrent to browse it (`POST /create`) names no file. It may start
/// the unambiguous main feature, but a season pack is ambiguous and must start
/// NOTHING until a file is actually played.
#[tokio::test]
async fn browsing_a_torrent_selects_the_main_feature_or_nothing() {
    let s = spawn("browse").await;

    let movie = s
        .create(&make_torrent(
            "Some.Movie.2026",
            &[
                F { path: "Some.Movie.2026.mkv", fill: 0x11, pieces: 7 },
                F { path: "extras/behind.the.scenes.mp4", fill: 0x22, pieces: 2 },
                F { path: "Some.Movie.2026.nfo", fill: 0x33, pieces: 1 },
            ],
        ))
        .await;
    s.wait_ready(&movie).await;
    assert_eq!(s.selected(&movie).await, vec![true, false, false], "the feature, not its extras");

    let pack = s.create(&season_pack()).await;
    s.wait_ready(&pack).await;
    assert_eq!(s.selected(&pack).await, vec![false, false, false], "an ambiguous pack starts nothing");
    let entry = s.entry(&pack).await;
    assert_eq!(entry["total"], 0, "nothing selected, nothing to download: {entry}");

    // ...and playing an episode of it afterwards selects exactly that one.
    assert_eq!(s.head_stream(&pack, "2").await, reqwest::StatusCode::OK);
    assert_eq!(s.selected(&pack).await, vec![false, false, true]);
}

/// A torrent added by the old code (or restored from a session.json written by
/// it) carries `only_files: None`. Its next play must narrow it to the played
/// file, keeping every file that is already COMPLETE on disk selected (nothing
/// already downloaded is hidden or discarded) and dropping the rest.
#[tokio::test]
async fn a_legacy_all_selected_torrent_narrows_on_the_next_play() {
    let s = spawn("legacy").await;
    let files = [
        F { path: "E01.mkv", fill: 0x11, pieces: 2 },
        F { path: "E02.mkv", fill: 0x22, pieces: 2 },
        F { path: "E03.mkv", fill: 0x33, pieces: 2 },
    ];
    let blob = make_torrent("Legacy.Pack", &files);

    // Episode 1 is already fully on disk from an earlier session.
    let folder = s.dir.join("Legacy.Pack");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("E01.mkv"), files[0].data()).unwrap();

    // Add it exactly the way the pre-fix code did: no selection at all.
    let handle = s
        .engine
        .session()
        .add_torrent(
            librqbit::AddTorrent::from_bytes(blob),
            Some(librqbit::AddTorrentOptions { overwrite: true, ..Default::default() }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    assert_eq!(handle.only_files(), None, "precondition: a legacy all-files selection");
    let ih = Engine::info_hash_hex(&handle);
    s.wait_ready(&ih).await;
    let before = s.files(&ih).await;
    assert_eq!(before[0]["downloaded"], (2 * PIECE) as u64, "precondition: E01 verified complete");

    assert_eq!(s.head_stream(&ih, "1").await, reqwest::StatusCode::OK);
    assert_eq!(
        s.selected(&ih).await,
        vec![true, true, false],
        "complete E01 stays, played E02 joins, unplayed E03 stops downloading"
    );
    // Nothing on disk was touched.
    assert_eq!(std::fs::read(folder.join("E01.mkv")).unwrap(), files[0].data());
    assert_eq!(s.files(&ih).await[0]["downloaded"], (2 * PIECE) as u64);
}

/// The next-episode preload POSTs `/cache/download {infoHash, fileIdx}` while
/// the current episode plays: the next episode JOINS the selection, the one
/// playing stays, and nothing else is pulled in.
#[tokio::test]
async fn next_episode_preload_adds_to_the_selection() {
    let s = spawn("preload").await;
    let ih = s.create(&season_pack()).await;
    s.wait_ready(&ih).await;

    assert_eq!(s.head_stream(&ih, "0").await, reqwest::StatusCode::OK);
    let (status, body) =
        s.post("cache/download", serde_json::json!({ "infoHash": ih, "fileIdx": 1 })).await;
    assert!(status.is_success(), "cache/download failed: {status} {body}");

    assert_eq!(s.selected(&ih).await, vec![true, true, false]);
    assert_eq!(s.entry(&ih).await["pinned"], true, "download to cache pins");
}

/// `/cache/download` WITHOUT a fileIdx comes from a stream that carries no file
/// index (it plays as `/{ih}/-1`), so it downloads that stream's file: the one
/// the -1 rule picks. Not the whole pack.
#[tokio::test]
async fn cache_download_without_an_index_downloads_the_streams_file() {
    let s = spawn("download-noidx").await;
    let ih = s
        .create(&make_torrent(
            "Some.Show.S01",
            &[
                F { path: "E01.mkv", fill: 0x11, pieces: 2 },
                F { path: "E02.mkv", fill: 0x22, pieces: 3 },
                F { path: "notes.txt", fill: 0x33, pieces: 4 },
            ],
        ))
        .await;
    s.wait_ready(&ih).await;

    let (status, body) = s.post("cache/download", serde_json::json!({ "infoHash": ih })).await;
    assert!(status.is_success(), "cache/download failed: {status} {body}");
    assert_eq!(s.selected(&ih).await, vec![false, true, false]);
    assert_eq!(s.entry(&ih).await["pinned"], true);
}

/// The Cache page's file browser stays the explicit way to fetch more (or
/// less) of a torrent, and a later play only ever ADDS its file to a selection
/// the user made.
#[tokio::test]
async fn explicit_cache_select_still_adds_and_drops_files() {
    let s = spawn("select").await;
    let ih = s.create(&season_pack()).await;
    s.wait_ready(&ih).await;

    assert_eq!(s.head_stream(&ih, "0").await, reqwest::StatusCode::OK);
    assert_eq!(s.selected(&ih).await, vec![true, false, false]);

    let (status, body) = s
        .post("cache/select", serde_json::json!({ "infoHash": ih, "fileIdx": 2, "selected": true }))
        .await;
    assert!(status.is_success(), "select failed: {status} {body}");
    assert_eq!(s.selected(&ih).await, vec![true, false, true]);

    let (status, body) = s
        .post("cache/select", serde_json::json!({ "infoHash": ih, "fileIdx": 0, "selected": false }))
        .await;
    assert!(status.is_success(), "deselect failed: {status} {body}");
    assert_eq!(s.selected(&ih).await, vec![false, false, true]);

    // Playing the already-selected file changes nothing; playing another adds it.
    assert_eq!(s.head_stream(&ih, "2").await, reqwest::StatusCode::OK);
    assert_eq!(s.selected(&ih).await, vec![false, false, true]);
    assert_eq!(s.head_stream(&ih, "1").await, reqwest::StatusCode::OK);
    assert_eq!(s.selected(&ih).await, vec![false, true, true]);
}

/// Selection changes are read-modify-write: read `only_files`, add a file,
/// write the whole list back. Two plays of DIFFERENT files landing at once (two
/// first plays of different episodes, a play beside the next-episode preload)
/// must both survive; an unserialized pair can each write back a list missing
/// the other's file, and that episode then silently never downloads.
///
/// Probabilistic by nature: the unlocked window has no await point, so only
/// true thread parallelism can hit it. Many OS threads are released at once
/// through a barrier, round after round, and every round must end with every
/// file selected.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_plays_of_different_files_all_stay_selected() {
    const FILES: usize = 24;
    const ROUNDS: usize = 40;
    let s = spawn("concurrent-select").await;
    let names: Vec<String> = (0..FILES).map(|i| format!("E{i:02}.mkv")).collect();
    let files: Vec<F> = names
        .iter()
        .enumerate()
        .map(|(i, name)| F { path: name, fill: i as u8 + 1, pieces: 1 })
        .collect();
    // Equal-sized episodes: an ambiguous pack, added with nothing selected.
    let ih = s.create(&make_torrent("Race.Pack", &files)).await;
    s.wait_ready(&ih).await;
    let handle = s.engine.get(&ih).expect("managed");
    let rt = tokio::runtime::Handle::current();

    let mut lost_rounds = Vec::new();
    for round in 0..ROUNDS {
        // Back to an empty selection (the engine API refuses an empty list on
        // purpose; the test resets through librqbit directly).
        s.engine
            .session()
            .update_only_files(&handle, &std::collections::HashSet::new())
            .await
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(FILES));
        let threads: Vec<_> = (0..FILES)
            .map(|idx| {
                let (engine, handle, rt, barrier) =
                    (s.engine.clone(), handle.clone(), rt.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    rt.block_on(engine.ensure_selected(&handle, idx))
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap().expect("ensure_selected failed");
        }
        let mut got = handle.only_files().expect("an explicit selection");
        got.sort_unstable();
        if got.len() != FILES {
            lost_rounds.push((round, FILES - got.len()));
        }
    }
    assert!(
        lost_rounds.is_empty(),
        "concurrent selections lost files in {} of {ROUNDS} rounds, (round, files lost): {lost_rounds:?}",
        lost_rounds.len()
    );
}

/// An index that does not exist fails the stream and leaves the selection
/// alone (it must not fall back to selecting anything).
#[tokio::test]
async fn an_unresolvable_index_changes_nothing() {
    let s = spawn("badidx").await;
    let ih = s.create(&season_pack()).await;
    s.wait_ready(&ih).await;

    assert_eq!(s.head_stream(&ih, "7").await, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(s.head_stream(&ih, "no-such-file.mkv").await, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(s.selected(&ih).await, vec![false, false, false]);
}
