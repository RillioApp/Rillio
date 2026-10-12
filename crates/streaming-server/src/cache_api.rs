//! Cache management API (Rillio-specific, not in the stremio server oracle):
//! list what's cached, keep ("download to cache" = add + pin), pin/unpin, and
//! delete. Backs the web app's Cached page and per-stream Download buttons.
//!
//! Trust model matches the rest of the server (see `security`): reads are GET,
//! every mutation is POST-only so a foreign page's `<img src>`/navigation (a
//! GET with no Origin) can never trigger one, and the Origin allowlist runs in
//! front of all of it.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use librqbit::TorrentStatsState;
use serde::{Deserialize, Serialize};

use crate::engine::{Engine, FileRef};
use crate::torrent::{guess_file_idx, is_valid_infohash};

/// One cached torrent, as the Cached page renders it. Everything is scoped to
/// the SELECTED files (what we actually download), not the whole torrent, so a
/// season pack where one episode was picked shows that episode's name and size.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CacheEntry {
    pub info_hash: String,
    /// The playable file's name when the selection resolves to exactly one
    /// video (e.g. the episode filename), otherwise the torrent name.
    pub name: String,
    /// Bytes actually downloaded of the selected files (≈ on-disk weight).
    pub downloaded: u64,
    /// Total size of the selected files.
    pub total: u64,
    /// "initializing" | "live" | "paused" | "error", or "waiting" for a torrent
    /// that is in the cache but not in the engine (set aside until its
    /// metadata is back; see `issue`).
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The ONE health value of the entry: absent when it is healthy, otherwise
    /// what is wrong with it. Derived here (see [`derive_issue`]) so every
    /// client reads the same verdict instead of re-deriving it from `state`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<Issue>,
    pub pinned: bool,
    /// Whether the player marked this stream watched (streaming mode): an
    /// un-pinned watched entry is scheduled for automatic cleanup.
    pub watched: bool,
    /// The addon metadata this torrent was matched to (title, artwork ids,
    /// the meta/video ids for a deep link), or absent when it has not been
    /// identified yet. Opaque to the server; see engine::CacheMeta.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<crate::engine::CacheMeta>,
    /// Number of selected files.
    pub file_count: usize,
    /// The playable file's index in the torrent: the selection's only video,
    /// or its dominant one (see [`main_feature`]). Omitted for a season pack,
    /// where "Play" has no single meaning, and for a selection with no video.
    /// Lets the Cached page build a player deep link for the entry.
    ///
    /// Deliberately NOT "exactly one selected file": scene releases routinely
    /// ship an .nfo/.txt/.jpg next to the video, and requiring a lone file made
    /// every one of those look unplayable (no Play button on a finished movie).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_idx: Option<usize>,
    /// When this torrent entered the cache, unix epoch milliseconds. Stamped on
    /// add; pre-existing caches are backfilled once from on-disk file times
    /// (see engine::added_at_stamped). Drives the Cache page's date sort.
    pub added_at: u64,
}

/// What is wrong with a cache entry. `kind` is for code (which actions make
/// sense, whether it may be offered as playable: it may not), `reason` is for
/// a person.
#[derive(Serialize, Debug, PartialEq)]
pub(crate) struct Issue {
    /// "metadata-missing" | "engine-error" | "data-missing".
    pub kind: &'static str,
    pub reason: String,
}

/// What the engine knows about one torrent, reduced to what its health
/// depends on.
pub(crate) enum Observed<'a> {
    /// Not in the engine: set aside at startup because its saved metadata was
    /// unusable, waiting to be restored. Carries the last reason.
    SetAside(&'a str),
    /// In the engine. `error` is librqbit's message when it is in its error
    /// state; `missing` names the selected files the engine counts bytes for
    /// that are not on disk.
    Managed { error: Option<&'a str>, missing: &'a [String] },
}

/// The health of a cache entry, from what was observed. One place, so the
/// list, the tests and every client agree on what "has an issue" means.
pub(crate) fn derive_issue(observed: Observed<'_>) -> Option<Issue> {
    match observed {
        Observed::SetAside(reason) => Some(Issue {
            kind: "metadata-missing",
            reason: if reason.is_empty() {
                "Its saved details were lost. Waiting to get them again.".to_owned()
            } else {
                reason.to_owned()
            },
        }),
        Observed::Managed { error: Some(error), .. } => {
            Some(Issue { kind: "engine-error", reason: error.to_owned() })
        }
        Observed::Managed { error: None, missing } if !missing.is_empty() => Some(Issue {
            kind: "data-missing",
            reason: match missing {
                [one] => format!("{one} is no longer on disk"),
                [first, rest @ ..] => format!("{first} and {} more files are no longer on disk", rest.len()),
                [] => unreachable!("guarded above"),
            },
        }),
        Observed::Managed { .. } => None,
    }
}

#[cfg(test)]
mod issue_tests {
    use super::*;

    #[test]
    fn a_healthy_torrent_has_no_issue() {
        assert_eq!(derive_issue(Observed::Managed { error: None, missing: &[] }), None);
    }

    #[test]
    fn a_set_aside_torrent_is_metadata_missing_with_its_reason() {
        let issue = derive_issue(Observed::SetAside("no peer delivered the metadata")).unwrap();
        assert_eq!(issue.kind, "metadata-missing");
        assert_eq!(issue.reason, "no peer delivered the metadata");
        // A record with no reason yet still says something a person can read.
        assert!(!derive_issue(Observed::SetAside("")).unwrap().reason.is_empty());
    }

    #[test]
    fn the_engine_error_state_wins_and_carries_the_message() {
        let missing = vec!["a.mkv".to_owned()];
        let issue =
            derive_issue(Observed::Managed { error: Some("disk full"), missing: &missing }).unwrap();
        assert_eq!(issue, Issue { kind: "engine-error", reason: "disk full".into() });
    }

    #[test]
    fn files_gone_from_disk_are_data_missing_and_named() {
        let one = vec!["S01E01.mkv".to_owned()];
        let issue = derive_issue(Observed::Managed { error: None, missing: &one }).unwrap();
        assert_eq!(issue.kind, "data-missing");
        assert_eq!(issue.reason, "S01E01.mkv is no longer on disk");
        let three = vec!["a.mkv".to_owned(), "b.mkv".to_owned(), "c.mkv".to_owned()];
        let issue = derive_issue(Observed::Managed { error: None, missing: &three }).unwrap();
        assert_eq!(issue.reason, "a.mkv and 2 more files are no longer on disk");
    }
}

/// Extensions we treat as playable video. Matches the container formats the
/// player (libmpv) handles; anything else in a torrent (.nfo, .txt, .jpg,
/// .srt, sample scripts) is packaging, not the thing the user wants to watch.
const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "mov", "m4v", "webm", "ts", "m2ts", "mts", "wmv", "flv", "mpg", "mpeg",
    "ogv", "ogm", "divx", "vob", "rmvb", "3gp",
];

pub(crate) fn is_video(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| VIDEO_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

/// How much bigger the main feature must be than the next video for us to call
/// it unambiguous. A movie beside a trailer/sample/behind-the-scenes clip is a
/// landslide; two episodes of a season pack are within a factor of two of each
/// other, which is exactly the case we must NOT guess at.
const MAIN_FEATURE_RATIO: u64 = 3;

/// Which video (if any) "the Play button" should mean, given the video file
/// indices and their sizes.
pub(crate) fn main_feature(videos: &[usize], length: impl Fn(usize) -> u64) -> Option<usize> {
    match videos.len() {
        0 => None,
        1 => Some(videos[0]),
        _ => {
            let mut by_size: Vec<(usize, u64)> = videos.iter().map(|&i| (i, length(i))).collect();
            by_size.sort_by(|a, b| b.1.cmp(&a.1));
            let (biggest, biggest_len) = by_size[0];
            let runner_up = by_size[1].1;
            // Guard the multiply against a pathological 0-length runner-up.
            if runner_up == 0 || biggest_len / runner_up.max(1) >= MAIN_FEATURE_RATIO {
                Some(biggest)
            } else {
                None
            }
        }
    }
}

/// `GET /cache/list` - every torrent the engine manages (the session persists
/// across restarts, so this is the whole cache), biggest first.
pub(crate) async fn list(State(engine): State<Engine>) -> Json<Vec<CacheEntry>> {
    let mut entries: Vec<CacheEntry> = engine
        .all()
        .iter()
        .map(|handle| {
            let stats = handle.stats();
            let info_hash = Engine::info_hash_hex(handle);
            let files = Engine::files(handle);
            // The download selection: `only_files() == None` means every file.
            // Indices are bounds-checked so a stale selection can't panic.
            let selected: Vec<usize> = match handle.only_files() {
                Some(only) => only.into_iter().filter(|&i| i < files.len()).collect(),
                None => (0..files.len()).collect(),
            };
            let total: u64 = selected.iter().map(|&i| files[i].length).sum();
            // Per-file have-bytes are exact (never exceed the file's length),
            // unlike `progress_bytes` which is piece-aligned.
            //
            // `progress_bytes` MEANS DIFFERENT THINGS PER STATE, and conflating
            // them is a bug we shipped: for Live/Paused it is have-bytes, but for
            // Initializing librqbit sets it to `checked_bytes` - how far the HASH
            // CHECK has scanned, which is not data we hold. Since file_progress is
            // also empty while initializing (no chunk tracker yet), the old
            // `file_progress.is_empty() -> progress_bytes` fallback reported the
            // check's scan as downloaded: on a freshly preallocated file the row
            // raced to ~total in seconds (checking zeros is fast) and then
            // COLLAPSED to 0 the moment it went live and the truth took over.
            // Every pause/resume re-runs the check, so it climbed and collapsed
            // again. There is no have-byte count during Initializing, so the
            // honest answer is 0; the row already says "Preparing" there.
            let downloaded: u64 = match stats.state {
                TorrentStatsState::Initializing | TorrentStatsState::Error => 0,
                TorrentStatsState::Live | TorrentStatsState::Paused => {
                    if stats.file_progress.is_empty() {
                        // Live can still yield an empty vec if the chunk-tracker read
                        // fails; there progress_bytes IS have-bytes, so it stands in.
                        // Clamped so `downloaded <= total` always holds.
                        stats.progress_bytes.min(total)
                    } else {
                        selected
                            .iter()
                            .map(|&i| stats.file_progress.get(i).copied().unwrap_or(0))
                            .sum()
                    }
                }
            };
            // The playable file among the selected ones (see `file_idx`).
            let videos: Vec<usize> = selected
                .iter()
                .copied()
                .filter(|&i| is_video(&files[i].name))
                .collect();
            let playable = main_feature(&videos, |i| files[i].length);
            let name = match playable {
                Some(idx) => files[idx].name.clone(),
                None => handle.name().unwrap_or_default(),
            };
            // Selected files the engine counts bytes for that are not on disk
            // (deleted behind its back while it runs). Only checked where a
            // have-byte count exists, and only for files that claim some.
            let missing: Vec<String> = match (&stats.state, engine.output_folder(&info_hash)) {
                (TorrentStatsState::Live | TorrentStatsState::Paused, Some(folder)) => selected
                    .iter()
                    .filter(|&&i| stats.file_progress.get(i).copied().unwrap_or(0) > 0)
                    .filter(|&&i| !folder.join(&files[i].path).exists())
                    .map(|&i| files[i].name.clone())
                    .collect(),
                _ => Vec::new(),
            };
            CacheEntry {
                issue: derive_issue(Observed::Managed {
                    error: match stats.state {
                        TorrentStatsState::Error => {
                            Some(stats.error.as_deref().unwrap_or("the torrent engine reported an error"))
                        }
                        _ => None,
                    },
                    missing: &missing,
                }),
                pinned: engine.is_pinned(&info_hash),
                watched: engine.is_watched(&info_hash),
                meta: engine.meta(&info_hash),
                added_at: engine.added_at_stamped(&info_hash, handle),
                info_hash,
                name,
                downloaded,
                total,
                state: format!("{:?}", stats.state).to_lowercase(),
                error: stats.error.clone(),
                file_count: selected.len(),
                file_idx: playable,
            }
        })
        .collect();
    // Torrents that are in the cache but not in the engine (set aside until
    // their metadata is back) are rows like any other, with their issue. The
    // sidecars are keyed by infohash, so a title identified before it was set
    // aside keeps its name and artwork; without one the folder name stands in.
    entries.extend(engine.unrestored().into_iter().map(|record| {
        let info_hash = record.info_hash.clone();
        CacheEntry {
            name: record
                .output_folder
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| info_hash.clone()),
            downloaded: 0,
            total: 0,
            state: "waiting".to_owned(),
            error: None,
            issue: derive_issue(Observed::SetAside(&record.reason)),
            pinned: engine.is_pinned(&info_hash),
            watched: engine.is_watched(&info_hash),
            meta: engine.meta(&info_hash),
            file_count: 0,
            file_idx: None,
            added_at: engine.added_at(&info_hash).unwrap_or(0),
            info_hash,
        }
    }));
    // Newest first as the wire order; the web re-sorts to whatever the user
    // picked, this is just a sensible default for any other consumer.
    entries.sort_by(|a, b| b.added_at.cmp(&a.added_at));
    Json(entries)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DownloadBody {
    info_hash: String,
    /// The stream's file. Absent when the stream carries no index, which is
    /// the same stream the player opens as `/{ih}/-1`.
    file_idx: Option<usize>,
}

/// `POST /cache/download` - "download to cache": add-or-get the torrent, make
/// sure it is running and the stream's file is selected, and PIN it so the
/// cache sweeper never evicts it.
///
/// It downloads ONE file, the stream's, never the whole torrent: both callers
/// (the player's "download" action and the next-episode preload) hand it a
/// stream. With `fileIdx` that file joins the selection (a preload of the next
/// episode of the pack being watched adds that episode and keeps the current
/// one). Without it, the file is the one the player would open for that stream
/// (`/{ih}/-1`, the largest media file). Fetching more of a torrent is the
/// Cache page's explicit per-file `/cache/select`.
pub(crate) async fn download(
    State(engine): State<Engine>,
    Json(body): Json<DownloadBody>,
) -> Response {
    if !is_valid_infohash(&body.info_hash) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let info_hash = body.info_hash.to_lowercase();
    let stream_default = |files: &[crate::types::File]| usize::try_from(guess_file_idx(files)).ok();
    let file = match body.file_idx {
        Some(idx) => FileRef::Index(idx),
        None => FileRef::Resolve(&stream_default),
    };
    let handle = match engine.get_or_create_for_file(&info_hash, file).await {
        Ok((h, _)) => h,
        Err(e) => {
            tracing::error!("cache/download {info_hash} fileIdx={:?}: {e:#}", body.file_idx);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("{e:#}") })),
            )
                .into_response();
        }
    };
    // A fresh add is already unpaused and still initializing, which librqbit
    // reports as "not paused" - so this is a no-op there and only does work when
    // re-downloading something the user had paused. Not fatal either way: the
    // torrent is added and pinned, which is what the caller asked for.
    if let Err(e) = engine.unpause(&handle).await {
        tracing::warn!("cache/download {info_hash}: {e:#}");
    }
    engine.touch(&info_hash);
    engine.set_pinned(&info_hash, true);
    Json(serde_json::json!({ "success": true })).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PinBody {
    info_hash: String,
    pinned: bool,
}

/// `POST /cache/pin` - toggle eviction protection.
pub(crate) async fn pin(State(engine): State<Engine>, Json(body): Json<PinBody>) -> Response {
    if !is_valid_infohash(&body.info_hash) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    engine.set_pinned(&body.info_hash.to_lowercase(), body.pinned);
    Json(serde_json::json!({ "success": true })).into_response()
}

/// One file inside a cached torrent, as the Cache page's file browser renders
/// it. Covers EVERY file in the torrent, not just the downloading selection:
/// the point of the browser is to see (and fetch) what else is in there.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TorrentFile {
    /// Index in the torrent, which is what selection and streaming address.
    pub index: usize,
    /// Full relative path (forward slashes), so nested release folders read
    /// correctly in a list.
    pub path: String,
    pub length: u64,
    /// Bytes held on disk for this file. 0 for a file nobody selected.
    pub downloaded: u64,
    /// Whether this file is part of the download selection.
    pub selected: bool,
    /// Whether the player can open it (drives the per-file Play button).
    pub video: bool,
}

/// `GET /cache/files/{info_hash}` - the whole torrent's contents.
pub(crate) async fn files(
    State(engine): State<Engine>,
    axum::extract::Path(info_hash): axum::extract::Path<String>,
) -> Response {
    if !is_valid_infohash(&info_hash) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(handle) = engine.get(&info_hash.to_lowercase()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let all = Engine::files(&handle);
    let stats = handle.stats();
    let selected: Vec<usize> = match handle.only_files() {
        Some(only) => only.into_iter().filter(|&i| i < all.len()).collect(),
        None => (0..all.len()).collect(),
    };
    let out: Vec<TorrentFile> = all
        .iter()
        .enumerate()
        .map(|(index, file)| TorrentFile {
            index,
            // Per-file have-bytes only exist once the chunk tracker does; an
            // initializing torrent honestly reports 0 (see the note in `list`).
            downloaded: stats.file_progress.get(index).copied().unwrap_or(0),
            selected: selected.contains(&index),
            video: is_video(&file.name),
            path: file.path.clone(),
            length: file.length,
        })
        .collect();
    Json(out).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MetaBody {
    info_hash: String,
    /// The matched metadata, or null to forget a wrong match.
    meta: Option<crate::engine::CacheMeta>,
}

/// `POST /cache/meta` - record what media a torrent actually is.
///
/// The web posts this when playback starts from a known title (it already has
/// the metadata) and after matching an orphan torrent's release name against
/// the installed addons. Stored verbatim and handed back on `/cache/list`, so
/// the Cache page can show real titles and artwork instead of scene filenames.
pub(crate) async fn meta(State(engine): State<Engine>, Json(body): Json<MetaBody>) -> Response {
    if !is_valid_infohash(&body.info_hash) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    engine.set_meta(&body.info_hash.to_lowercase(), body.meta);
    Json(serde_json::json!({ "success": true })).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SelectBody {
    info_hash: String,
    file_idx: usize,
    selected: bool,
}

/// `POST /cache/select` - add a file to the download selection, or drop it.
///
/// Adding is how the Cache page's file browser fetches "the other stuff" in a
/// torrent (extras, a bundled subtitle, the second episode of a double bill).
pub(crate) async fn select(State(engine): State<Engine>, Json(body): Json<SelectBody>) -> Response {
    if !is_valid_infohash(&body.info_hash) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let info_hash = body.info_hash.to_lowercase();
    let Some(handle) = engine.get(&info_hash) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if body.file_idx >= Engine::files(&handle).len() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // The read-modify-write of the selection happens inside the engine, under
    // its selection lock, so a concurrent play cannot lose this toggle (or
    // this toggle the play's file).
    match engine.set_file_selected(&handle, body.file_idx, body.selected).await {
        Ok(()) => {
            engine.touch(&info_hash);
            Json(serde_json::json!({ "success": true })).into_response()
        }
        // librqbit refuses a selection change while the torrent hash-checks
        // ("can't update initializing torrent"): the request was fine, the
        // torrent's state would not allow it. 409, same as a refused pause.
        Err(e) => {
            tracing::warn!("cache/select {info_hash} idx={}: {e:#}", body.file_idx);
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": format!("{e:#}") })),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WatchedBody {
    info_hash: String,
    watched: bool,
}

/// `POST /cache/watched` - the player reporting a stream watched (>= ~90%
/// through). In streaming mode the ephemeral sweeper deletes un-pinned watched
/// torrents after a grace period; a pinned ("kept") torrent keeps the mark but
/// is never deleted.
pub(crate) async fn watched(State(engine): State<Engine>, Json(body): Json<WatchedBody>) -> Response {
    if !is_valid_infohash(&body.info_hash) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    engine.set_watched(&body.info_hash.to_lowercase(), body.watched);
    Json(serde_json::json!({ "success": true })).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PauseBody {
    info_hash: String,
    paused: bool,
}

/// `POST /cache/pause` - pause or resume a cached torrent's download.
pub(crate) async fn pause(State(engine): State<Engine>, Json(body): Json<PauseBody>) -> Response {
    if !is_valid_infohash(&body.info_hash) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let info_hash = body.info_hash.to_lowercase();
    let Some(handle) = engine.get(&info_hash) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let result = if body.paused {
        engine.pause(&handle).await
    } else {
        engine.unpause(&handle).await
    };
    // A refused pause must not answer {"success": true}. librqbit cannot pause a
    // torrent mid-hash-check, and swallowing that told the UI the row was paused
    // while the download ran on. 409: the request was fine, the torrent's state
    // would not allow it.
    match result {
        Ok(()) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => {
            tracing::warn!("cache/pause {} paused={}: {e:#}", info_hash, body.paused);
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": format!("{e:#}") })),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RefetchBody {
    info_hash: String,
}

/// `POST /cache/refetch` - "fetch again": try now to get what a torrent with
/// an issue is missing (see [`Engine::refetch`]). Answers once the attempt is
/// over, which for lost metadata can take as long as the bounded wait for a
/// peer; `409` with the reason when it did not work.
pub(crate) async fn refetch(State(engine): State<Engine>, Json(body): Json<RefetchBody>) -> Response {
    if !is_valid_infohash(&body.info_hash) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let info_hash = body.info_hash.to_lowercase();
    match engine.refetch(&info_hash).await {
        Ok(()) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(e) => {
            tracing::warn!("cache/refetch {info_hash}: {e:#}");
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": format!("{e:#}") })),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeleteBody {
    info_hash: String,
}

/// `POST /cache/delete` - stop the torrent and delete its cached files.
pub(crate) async fn delete(State(engine): State<Engine>, Json(body): Json<DeleteBody>) -> Response {
    if !is_valid_infohash(&body.info_hash) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    engine.remove(&body.info_hash.to_lowercase()).await;
    Json(serde_json::json!({ "success": true })).into_response()
}
