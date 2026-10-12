//! Regression: ONE persisted torrent whose saved metadata is unusable must not
//! keep the whole engine from starting, must not be lost, and must come back
//! exactly where it was.
//!
//! librqbit restores a session by re-adding every entry of
//! `session/session.json` and AWAITS all of those adds before the session
//! exists. An entry whose `session/<infohash>.torrent` is missing or empty is
//! restored as a bare magnet, and a magnet add waits for a peer to deliver the
//! metadata with no deadline. With nobody to deliver it `Engine::new` never
//! returned: the streaming server never came up (2026-10-12, a 0-byte
//! `.torrent` of a season pack in a real cache).
//!
//! The shapes here mirror that entry: a multi-file pack in its own folder
//! under the cache root, `only_files = [0]`, a fastresume bitfield, not paused.
//!
//! Offline by construction: nothing here can reach a peer, which is exactly
//! the "nobody delivers the metadata" case.

use std::path::{Path, PathBuf};
use std::time::Duration;

use librqbit_sha1_wrapper::{ISha1, Sha1};
use rillio_streaming_server::axum::body::Body;
use rillio_streaming_server::axum::http::{Request, StatusCode};
use rillio_streaming_server::engine::Pick;
use rillio_streaming_server::tower::ServiceExt;
use rillio_streaming_server::{router, Config, Engine};
use serde_json::Value;

const PIECE: usize = 16_384;
const PACK: &str = "Some Show (2024) (Season 1) (1080p)";
const PACK_FILES: [&str; 3] = ["Some.Show.S01E01.mkv", "Some.Show.S01E02.mkv", "Some.Show.S01E03.mkv"];
const MOVIE: &str = "Healthy.Movie.2021.1080p.mkv";

fn file_data(index: usize) -> Vec<u8> {
    vec![0x11 * (index as u8 + 1); 2 * PIECE]
}

fn bencode_torrent(info: Vec<u8>) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(b"d4:info");
    t.extend_from_slice(&info);
    t.extend_from_slice(b"e");
    t
}

fn piece_hashes(info: &mut Vec<u8>, data: &[u8]) {
    for chunk in data.chunks(PIECE) {
        let mut h = Sha1::new();
        h.update(chunk);
        info.extend_from_slice(&h.finish());
    }
}

/// The season pack: three files of two pieces each, real piece hashes.
fn pack_torrent() -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(b"d5:filesl");
    for name in PACK_FILES {
        info.extend_from_slice(format!("d6:lengthi{}e4:pathl{}:{name}ee", 2 * PIECE, name.len()).as_bytes());
    }
    info.extend_from_slice(b"e");
    info.extend_from_slice(format!("4:name{}:{PACK}", PACK.len()).as_bytes());
    info.extend_from_slice(format!("12:piece lengthi{PIECE}e").as_bytes());
    info.extend_from_slice(format!("6:pieces{}:", PACK_FILES.len() * 2 * 20).as_bytes());
    for index in 0..PACK_FILES.len() {
        piece_hashes(&mut info, &file_data(index));
    }
    info.extend_from_slice(b"e");
    bencode_torrent(info)
}

/// A single-file torrent (its file lives directly in the cache root).
fn movie_torrent() -> (Vec<u8>, Vec<u8>) {
    let data = vec![0xEE; 2 * PIECE];
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi{}e", data.len()).as_bytes());
    info.extend_from_slice(format!("4:name{}:{MOVIE}", MOVIE.len()).as_bytes());
    info.extend_from_slice(format!("12:piece lengthi{PIECE}e").as_bytes());
    info.extend_from_slice(b"6:pieces40:");
    piece_hashes(&mut info, &data);
    info.extend_from_slice(b"e");
    (bencode_torrent(info), data)
}

async fn request(engine: &Engine, dir: &Path, req: Request<Body>) -> (StatusCode, Value) {
    let app = router(Config::local(dir.to_path_buf()), engine.clone());
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = rillio_streaming_server::axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn list(engine: &Engine, dir: &Path) -> Vec<Value> {
    let (status, body) =
        request(engine, dir, Request::builder().uri("/cache/list").body(Body::empty()).unwrap()).await;
    assert!(status.is_success());
    serde_json::from_value(body).unwrap()
}

async fn post(engine: &Engine, dir: &Path, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request(engine, dir, req).await
}

fn entry<'a>(entries: &'a [Value], ih: &str) -> Option<&'a Value> {
    entries.iter().find(|e| e["infoHash"] == ih)
}

/// Poll the list until `ready` holds for it.
async fn wait_for(engine: &Engine, dir: &Path, what: &str, ready: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    for _ in 0..200 {
        let entries = list(engine, dir).await;
        if ready(&entries) {
            return entries;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never saw: {what}. The list: {:?}", list(engine, dir).await);
}

fn settled(entries: &[Value], ih: &str) -> bool {
    entry(entries, ih).is_some_and(|e| e["state"] == "live" || e["state"] == "paused")
}

async fn stop(engine: Engine) {
    engine.session().stop().await;
    drop(engine);
    tokio::time::sleep(Duration::from_millis(300)).await;
}

struct Cache {
    dir: PathBuf,
    pack: String,
    movie: String,
}

impl Cache {
    fn session(&self) -> PathBuf {
        self.dir.join("session")
    }
    fn saved(&self, ih: &str) -> PathBuf {
        self.session().join(format!("{ih}.torrent"))
    }
    fn copy(&self, ih: &str) -> PathBuf {
        self.session().join("metadata").join(format!("{ih}.torrent"))
    }
    fn record(&self, ih: &str) -> PathBuf {
        self.session().join("unrestored").join(format!("{ih}.json"))
    }
    fn session_entry(&self, ih: &str) -> Option<Value> {
        let db: Value = serde_json::from_slice(&std::fs::read(self.session().join("session.json")).unwrap()).unwrap();
        db["torrents"].as_object().unwrap().values().find(|e| e["info_hash"] == ih).cloned()
    }
    /// The state the field entry was in: librqbit's file empty and (as for
    /// anything poisoned before the copies existed) no copy of ours.
    fn poison_without_copy(&self, ih: &str) {
        std::fs::write(self.saved(ih), b"").unwrap();
        let _ = std::fs::remove_file(self.copy(ih));
    }
    /// Restart; the engine must exist within the deadline.
    async fn start(&self) -> Engine {
        tokio::time::timeout(Duration::from_secs(20), Engine::new(self.dir.clone()))
            .await
            .expect(
                "Engine::new did not return within 20 s: one torrent with unusable saved \
                 metadata is holding up the whole engine",
            )
            .unwrap()
    }
}

/// A cache holding the pack (only episode 1 selected and on disk, in its own
/// folder) and a complete single-file movie, engine stopped.
async fn seeded_cache(tag: &str) -> Cache {
    let dir = std::env::temp_dir().join(format!("rillio-restore-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(PACK)).unwrap();
    std::fs::write(dir.join(PACK).join(PACK_FILES[0]), file_data(0)).unwrap();
    let (movie_blob, movie_data) = movie_torrent();
    std::fs::write(dir.join(MOVIE), movie_data).unwrap();

    let engine = Engine::new(dir.clone()).await.unwrap();
    let pack = Engine::info_hash_hex(&engine.add_blob(pack_torrent(), Pick::Files(vec![0])).await.unwrap());
    let movie = Engine::info_hash_hex(&engine.add_blob(movie_blob, Pick::Files(vec![0])).await.unwrap());
    let entries =
        wait_for(&engine, &dir, "both seeded torrents live", |e| settled(e, &pack) && settled(e, &movie)).await;
    assert!(
        entries.iter().all(|e| e["downloaded"] == e["total"] && e.get("issue").is_none()),
        "both seeded torrents verify complete and healthy: {entries:?}"
    );
    stop(engine).await;
    Cache { dir, pack, movie }
}

/// Not a test: writes a small cache in the field entry's poisoned state to
/// `RILLIO_SEED_DIR`, for looking at the Cache window in a dev shell
/// (`RILLIO_STREAMING_CACHE_DIR=<that dir>`). The pack is identified as a
/// series episode so its row has a title, artwork and a streams page.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "seeds a dev cache; set RILLIO_SEED_DIR"]
async fn seed_a_dev_cache_in_the_field_entrys_state() {
    let target = PathBuf::from(std::env::var("RILLIO_SEED_DIR").expect("set RILLIO_SEED_DIR"));
    let cache = seeded_cache("dev-seed").await;
    let engine = cache.start().await;
    engine.set_meta(
        &cache.pack,
        Some(serde_json::json!({
            "metaId": "tt13210838",
            "type": "series",
            "name": "The Gentlemen",
            "poster": "https://images.metahub.space/poster/medium/tt13210838/img",
            "background": "https://images.metahub.space/background/medium/tt13210838/img",
            "logo": "https://images.metahub.space/logo/medium/tt13210838/img",
            "year": "2024",
            "videoId": "tt13210838:1:1",
            "season": 1,
            "episode": 1,
            "fileIdx": 0
        })),
    );
    stop(engine).await;
    cache.poison_without_copy(&cache.pack);

    // The session stores absolute folders: rebase them onto the target.
    let _ = std::fs::remove_dir_all(&target);
    copy_tree(&cache.dir, &target);
    let db_path = target.join("session").join("session.json");
    let (from, to) = (cache.dir.to_string_lossy().into_owned(), target.to_string_lossy().into_owned());
    let mut db: Value = serde_json::from_slice(&std::fs::read(&db_path).unwrap()).unwrap();
    for e in db["torrents"].as_object_mut().unwrap().values_mut() {
        let folder = e["output_folder"].as_str().unwrap().replacen(&from, &to, 1);
        e["output_folder"] = Value::String(folder);
    }
    std::fs::write(&db_path, serde_json::to_vec(&db).unwrap()).unwrap();
    println!("seeded {target:?}: pack {} (poisoned), movie {}", cache.pack, cache.movie);
}

/// Measurement: can an add that is CANCELLED part-way (its future dropped, as
/// when the HTTP client behind it goes away) leave librqbit's `<hash>.torrent`
/// empty? librqbit opens that file with truncate and only then writes it, with
/// an await in between. Prints how many of the cancelled adds left a 0-byte
/// file, and how many of those torrents ended up in the session anyway.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement of how the poisoned state gets created"]
async fn measure_cancelled_adds_leaving_an_empty_saved_torrent() {
    let dir = std::env::temp_dir().join("rillio-restore-cancelled-adds");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.clone()).await.unwrap();
    let runs = 400u64;
    for i in 0..runs {
        let name = format!("Cancelled.Add.{i:04}.mkv");
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi{}e", PIECE).as_bytes());
        info.extend_from_slice(format!("4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(format!("12:piece lengthi{PIECE}e").as_bytes());
        info.extend_from_slice(b"6:pieces20:");
        info.extend_from_slice(&[0u8; 20]);
        info.extend_from_slice(b"e");
        let blob = bencode_torrent(info);
        // Dropped after 0 to ~2 ms: somewhere inside the add.
        let _ = tokio::time::timeout(
            Duration::from_micros(i * 5),
            engine.add_blob(blob, Pick::Files(vec![0])),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let managed: Vec<String> = engine.all().iter().map(Engine::info_hash_hex).collect();
    let mut empty = 0;
    let mut empty_and_managed = 0;
    for entry in std::fs::read_dir(dir.join("session")).unwrap().flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("torrent") && entry.metadata().unwrap().len() == 0 {
            empty += 1;
            let ih = path.file_stem().unwrap().to_string_lossy().into_owned();
            if managed.contains(&ih) {
                empty_and_managed += 1;
            }
        }
    }
    println!(
        "{runs} adds cancelled part-way: {empty} left a 0-byte <hash>.torrent; \
         {empty_and_managed} of those torrents are in the session ({} managed in all)",
        managed.len()
    );
    stop(engine).await;
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// Startup is never held up, and nothing is lost
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_field_entry_does_not_block_startup_and_is_listed_with_its_issue() {
    let cache = seeded_cache("field").await;
    cache.poison_without_copy(&cache.pack);

    let engine = cache.start().await;
    let entries = wait_for(&engine, &cache.dir, "the healthy movie live", |e| settled(e, &cache.movie)).await;
    let row = entry(&entries, &cache.pack).expect("the set-aside pack is a row of the list");
    assert_eq!(row["state"], "waiting");
    assert_eq!(row["issue"]["kind"], "metadata-missing");
    assert!(row["issue"]["reason"].as_str().is_some_and(|r| !r.is_empty()), "a reason a person can read: {row}");
    assert_eq!(row["name"], PACK, "recognisable by its folder name: {row}");
    assert!(entry(&entries, &cache.movie).unwrap().get("issue").is_none());

    // Recorded durably, out of librqbit's index, data untouched.
    let record: Value = serde_json::from_slice(&std::fs::read(cache.record(&cache.pack)).unwrap()).unwrap();
    assert_eq!(record["only_files"], serde_json::json!([0]));
    assert_eq!(record["is_paused"], false);
    assert_eq!(PathBuf::from(record["output_folder"].as_str().unwrap()), cache.dir.join(PACK));
    assert!(cache.session_entry(&cache.pack).is_none());
    assert_eq!(std::fs::read(cache.dir.join(PACK).join(PACK_FILES[0])).unwrap(), file_data(0));
    assert!(cache.session().join(format!("{}.bitv", cache.pack)).exists(), "the fastresume state is kept");
    stop(engine).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_saved_torrent_file_does_not_block_startup() {
    let cache = seeded_cache("missing").await;
    std::fs::remove_file(cache.saved(&cache.movie)).unwrap();
    let _ = std::fs::remove_file(cache.copy(&cache.movie));

    let engine = cache.start().await;
    let entries = wait_for(&engine, &cache.dir, "the pack live", |e| settled(e, &cache.pack)).await;
    assert_eq!(entry(&entries, &cache.movie).unwrap()["issue"]["kind"], "metadata-missing");
    stop(engine).await;
}

/// The other broken-entry shape, for the record: data files deleted behind the
/// engine's back never blocked startup (the fastresume guard drops the stale
/// bitfield and the torrent comes back empty). Pinned so it stays that way.
#[tokio::test(flavor = "multi_thread")]
async fn a_torrent_whose_data_files_are_gone_does_not_block_startup() {
    let cache = seeded_cache("data-gone").await;
    std::fs::remove_file(cache.dir.join(MOVIE)).unwrap();

    let engine = cache.start().await;
    let entries = wait_for(&engine, &cache.dir, "both live", |e| settled(e, &cache.pack) && settled(e, &cache.movie)).await;
    assert_eq!(entry(&entries, &cache.movie).unwrap()["downloaded"], 0, "nothing of it is on disk any more");
    stop(engine).await;
}

// ---------------------------------------------------------------------------
// Our own metadata copy: the poison heals offline
// ---------------------------------------------------------------------------

/// With our copy in hand an empty `<hash>.torrent` is repaired before the
/// session is built: no peer needed, the torrent is never set aside.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_saved_torrent_file_is_healed_offline_from_our_copy() {
    let cache = seeded_cache("heal").await;
    assert!(cache.copy(&cache.pack).exists(), "a copy of the metadata is kept at add time");
    let original = std::fs::read(cache.copy(&cache.pack)).unwrap();
    std::fs::write(cache.saved(&cache.pack), b"").unwrap();

    let engine = cache.start().await;
    let entries = wait_for(&engine, &cache.dir, "both live", |e| settled(e, &cache.pack) && settled(e, &cache.movie)).await;
    let pack = entry(&entries, &cache.pack).unwrap();
    assert!(pack.get("issue").is_none(), "healed, not set aside: {pack}");
    assert_eq!(pack["downloaded"], pack["total"], "its data and fastresume state are reused: {pack}");
    assert!(!cache.record(&cache.pack).exists());
    assert_eq!(std::fs::read(cache.saved(&cache.pack)).unwrap(), original);
    stop(engine).await;
}

/// Torrents added before the copies existed get one at the next start.
#[tokio::test(flavor = "multi_thread")]
async fn a_start_backfills_the_metadata_copies_of_an_existing_cache() {
    let cache = seeded_cache("backfill").await;
    std::fs::remove_dir_all(cache.session().join("metadata")).unwrap();

    let engine = cache.start().await;
    for ih in [&cache.pack, &cache.movie] {
        assert_eq!(std::fs::read(cache.copy(ih)).unwrap(), std::fs::read(cache.saved(ih)).unwrap());
    }
    stop(engine).await;
}

// ---------------------------------------------------------------------------
// Restore: exactly where it was
// ---------------------------------------------------------------------------

/// Set aside, then its metadata is available again (standing in for "a peer
/// delivered it"): it goes back with the same selection and folder, its data
/// and fastresume state reused, nothing downloaded again or truncated.
#[tokio::test(flavor = "multi_thread")]
async fn a_set_aside_pack_is_restored_where_it_was_once_its_metadata_is_back() {
    let cache = seeded_cache("recover").await;
    let saved = std::fs::read(cache.saved(&cache.pack)).unwrap();
    let folder_before = cache.session_entry(&cache.pack).unwrap()["output_folder"].clone();
    cache.poison_without_copy(&cache.pack);
    stop(cache.start().await).await;
    assert!(cache.record(&cache.pack).exists());

    std::fs::write(cache.saved(&cache.pack), &saved).unwrap();
    let engine = cache.start().await;
    let entries = wait_for(&engine, &cache.dir, "the pack restored", |e| settled(e, &cache.pack)).await;
    let pack = entry(&entries, &cache.pack).unwrap();
    assert!(pack.get("issue").is_none(), "{pack}");
    assert_eq!(pack["fileCount"], 1, "the selection is still episode 1 alone: {pack}");
    assert_eq!(pack["downloaded"], pack["total"], "nothing is downloaded again: {pack}");
    assert_eq!(pack["total"], 2 * PIECE);
    assert_eq!(std::fs::read(cache.dir.join(PACK).join(PACK_FILES[0])).unwrap(), file_data(0));
    let persisted = cache.session_entry(&cache.pack).expect("back in librqbit's index");
    assert_eq!(persisted["output_folder"], folder_before);
    assert_eq!(persisted["only_files"], serde_json::json!([0]));
    assert!(!cache.record(&cache.pack).exists(), "a restored torrent leaves no record");
    assert!(cache.copy(&cache.pack).exists(), "and now has a copy of its metadata");
    stop(engine).await;
}

/// The persisted output folder is restored EXACTLY, not re-derived: an older
/// cache can hold folders the default would not produce, and a re-add into
/// the default folder would strand the data and download it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_restore_goes_back_into_the_persisted_folder_not_the_default_one() {
    let cache = seeded_cache("folder").await;
    let saved = std::fs::read(cache.saved(&cache.pack)).unwrap();
    // Give the pack a folder the default derivation would never pick.
    let odd = cache.dir.join("older-layout").join("pack-23");
    std::fs::create_dir_all(odd.parent().unwrap()).unwrap();
    std::fs::rename(cache.dir.join(PACK), &odd).unwrap();
    let db_path = cache.session().join("session.json");
    let mut db: Value = serde_json::from_slice(&std::fs::read(&db_path).unwrap()).unwrap();
    for e in db["torrents"].as_object_mut().unwrap().values_mut() {
        if e["info_hash"] == cache.pack.as_str() {
            e["output_folder"] = Value::String(odd.to_string_lossy().into_owned());
        }
    }
    std::fs::write(&db_path, serde_json::to_vec(&db).unwrap()).unwrap();

    cache.poison_without_copy(&cache.pack);
    stop(cache.start().await).await;
    std::fs::write(cache.saved(&cache.pack), &saved).unwrap();
    let engine = cache.start().await;
    let entries = wait_for(&engine, &cache.dir, "the pack restored", |e| settled(e, &cache.pack)).await;
    let pack = entry(&entries, &cache.pack).unwrap();
    assert_eq!(pack["downloaded"], pack["total"], "its data in the persisted folder is reused: {pack}");
    assert_eq!(
        PathBuf::from(cache.session_entry(&cache.pack).unwrap()["output_folder"].as_str().unwrap()),
        odd
    );
    assert!(!cache.dir.join(PACK).exists(), "nothing was created in the default folder");
    stop(engine).await;
}

/// "Fetch again" restores a set-aside torrent now, without a restart.
#[tokio::test(flavor = "multi_thread")]
async fn fetch_again_restores_a_set_aside_torrent_without_a_restart() {
    let cache = seeded_cache("refetch").await;
    let saved = std::fs::read(cache.saved(&cache.pack)).unwrap();
    cache.poison_without_copy(&cache.pack);
    let engine = cache.start().await;
    wait_for(&engine, &cache.dir, "the pack waiting", |e| {
        entry(e, &cache.pack).is_some_and(|p| p["state"] == "waiting")
    })
    .await;

    std::fs::write(cache.saved(&cache.pack), &saved).unwrap();
    let (status, body) =
        post(&engine, &cache.dir, "/cache/refetch", serde_json::json!({ "infoHash": cache.pack })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entries = wait_for(&engine, &cache.dir, "the pack live", |e| settled(e, &cache.pack)).await;
    let pack = entry(&entries, &cache.pack).unwrap();
    assert!(pack.get("issue").is_none());
    assert_eq!(pack["downloaded"], pack["total"]);
    assert_eq!(entries.iter().filter(|e| e["infoHash"] == cache.pack.as_str()).count(), 1, "one row, not two");
    stop(engine).await;
}

// ---------------------------------------------------------------------------
// The records cannot lose one another, and cannot bring back the dead
// ---------------------------------------------------------------------------

/// Setting a second torrent aside while the first one's record is unreadable
/// must leave that record exactly as it is.
#[tokio::test(flavor = "multi_thread")]
async fn recording_one_torrent_never_touches_another_ones_unreadable_record() {
    let cache = seeded_cache("records").await;
    cache.poison_without_copy(&cache.pack);
    stop(cache.start().await).await;
    // The pack's record is now damaged (a torn write, a scanner's lock...).
    std::fs::write(cache.record(&cache.pack), b"{ not json").unwrap();

    cache.poison_without_copy(&cache.movie);
    let engine = cache.start().await;
    assert_eq!(std::fs::read(cache.record(&cache.pack)).unwrap(), b"{ not json", "left untouched");
    assert!(cache.record(&cache.movie).exists(), "the second torrent is recorded beside it");
    stop(engine).await;
}

/// The state a crash between the two index writes leaves: the entry is in the
/// record AND still in session.json. The next start resolves it, once.
#[tokio::test(flavor = "multi_thread")]
async fn an_entry_left_in_both_indexes_is_resolved_and_listed_once() {
    let cache = seeded_cache("both").await;
    let session_json = std::fs::read(cache.session().join("session.json")).unwrap();
    cache.poison_without_copy(&cache.pack);
    stop(cache.start().await).await;
    // Undo the second write: session.json has the entry again.
    std::fs::write(cache.session().join("session.json"), &session_json).unwrap();

    let engine = cache.start().await;
    let entries = wait_for(&engine, &cache.dir, "the movie live", |e| settled(e, &cache.movie)).await;
    assert_eq!(entries.iter().filter(|e| e["infoHash"] == cache.pack.as_str()).count(), 1);
    assert_eq!(entry(&entries, &cache.pack).unwrap()["state"], "waiting");
    stop(engine).await;
}

/// A record whose torrent is in the session with usable metadata is stale:
/// dropped at start, never re-added on top of the live torrent.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_record_of_a_torrent_in_the_session_is_dropped_at_start() {
    let cache = seeded_cache("stale").await;
    let saved = std::fs::read(cache.saved(&cache.pack)).unwrap();
    cache.poison_without_copy(&cache.pack);
    stop(cache.start().await).await;
    let record = std::fs::read(cache.record(&cache.pack)).unwrap();
    std::fs::write(cache.saved(&cache.pack), &saved).unwrap();
    let engine = cache.start().await;
    wait_for(&engine, &cache.dir, "the pack restored", |e| settled(e, &cache.pack)).await;
    stop(engine).await;
    // A restore that finished but could not clear its record.
    std::fs::write(cache.record(&cache.pack), &record).unwrap();

    let engine = cache.start().await;
    let entries = wait_for(&engine, &cache.dir, "the pack live", |e| settled(e, &cache.pack)).await;
    assert_eq!(entries.iter().filter(|e| e["infoHash"] == cache.pack.as_str()).count(), 1);
    assert!(!cache.record(&cache.pack).exists());

    // And removing the torrent clears everything that could bring it back.
    std::fs::write(cache.record(&cache.pack), &record).unwrap();
    let (status, _) = post(&engine, &cache.dir, "/cache/delete", serde_json::json!({ "infoHash": cache.pack })).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!cache.record(&cache.pack).exists(), "a removal clears the record");
    assert!(!cache.copy(&cache.pack).exists(), "and the metadata copy");
    stop(engine).await;
    let engine = cache.start().await;
    let entries = wait_for(&engine, &cache.dir, "the movie live", |e| settled(e, &cache.movie)).await;
    assert!(entry(&entries, &cache.pack).is_none(), "a removed torrent stays removed");
    stop(engine).await;
}

/// A set-aside torrent is removable from the Cache window like any other row:
/// its record, its session leftovers and its data folder all go.
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_set_aside_torrent_deletes_its_data_and_its_record() {
    let cache = seeded_cache("remove").await;
    cache.poison_without_copy(&cache.pack);
    let engine = cache.start().await;
    wait_for(&engine, &cache.dir, "the pack waiting", |e| entry(e, &cache.pack).is_some()).await;

    let (status, _) = post(&engine, &cache.dir, "/cache/delete", serde_json::json!({ "infoHash": cache.pack })).await;
    assert_eq!(status, StatusCode::OK);
    assert!(entry(&list(&engine, &cache.dir).await, &cache.pack).is_none());
    assert!(!cache.record(&cache.pack).exists());
    assert!(!cache.saved(&cache.pack).exists());
    assert!(!cache.session().join(format!("{}.bitv", cache.pack)).exists());
    assert!(!cache.dir.join(PACK).exists(), "its data folder is deleted");
    assert!(cache.dir.join(MOVIE).exists(), "nobody else's data is touched");
    stop(engine).await;

    let engine = cache.start().await;
    let entries = wait_for(&engine, &cache.dir, "the movie live", |e| settled(e, &cache.movie)).await;
    assert!(entry(&entries, &cache.pack).is_none());
    stop(engine).await;
}

/// Neither sweeper may delete the data of a set-aside torrent just because
/// the session does not hold it.
#[tokio::test(flavor = "multi_thread")]
async fn the_sweepers_leave_a_set_aside_torrents_data_and_marks_alone() {
    let cache = seeded_cache("sweep").await;
    cache.poison_without_copy(&cache.pack);
    let engine = cache.start().await;
    wait_for(&engine, &cache.dir, "the movie live", |e| settled(e, &cache.movie)).await;
    engine.set_watched(&cache.pack, true);

    engine.sweep_watched(Duration::ZERO, Duration::ZERO).await;
    engine.enforce_cache_cap(0, Duration::ZERO).await;

    assert!(engine.is_watched(&cache.pack), "its watched mark waits with it");
    assert_eq!(std::fs::read(cache.dir.join(PACK).join(PACK_FILES[0])).unwrap(), file_data(0));
    assert!(cache.record(&cache.pack).exists());
    assert!(entry(&list(&engine, &cache.dir).await, &cache.pack).is_some());
    stop(engine).await;
}

// ---------------------------------------------------------------------------
// Health of a torrent that IS in the engine
// ---------------------------------------------------------------------------

/// A file deleted under a running torrent that still counts its bytes is an
/// issue on that row, and "fetch again" makes the engine look at the disk.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_deleted_under_a_live_torrent_is_reported_and_fetch_again_rechecks_it() {
    let cache = seeded_cache("data-missing").await;
    let engine = cache.start().await;
    wait_for(&engine, &cache.dir, "the movie live", |e| settled(e, &cache.movie)).await;

    std::fs::remove_file(cache.dir.join(MOVIE)).unwrap();
    let entries = list(&engine, &cache.dir).await;
    let movie = entry(&entries, &cache.movie).unwrap();
    assert_eq!(movie["issue"]["kind"], "data-missing", "{movie}");
    assert!(movie["issue"]["reason"].as_str().unwrap().contains(MOVIE), "{movie}");

    let (status, body) =
        post(&engine, &cache.dir, "/cache/refetch", serde_json::json!({ "infoHash": cache.movie })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entries = wait_for(&engine, &cache.dir, "the movie rechecked", |e| settled(e, &cache.movie)).await;
    let movie = entry(&entries, &cache.movie).unwrap();
    assert!(movie.get("issue").is_none(), "the engine no longer claims bytes it does not have: {movie}");
    assert_eq!(movie["downloaded"], 0, "{movie}");
    stop(engine).await;
}
