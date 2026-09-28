//! M1 torrent routes: `/create`, `/:ih/create`, and file-selection logic.
//! The stream route (`/:ih/:idx`) and lifecycle (`/remove`) land in M1.3/M1.4.

use std::sync::OnceLock;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use regex::Regex;
use serde::Deserialize;

use crate::cache_api::{is_video, main_feature};
use crate::engine::{Engine, Pick};
use crate::types::{self, CreateResponse, PeerSearch};

/// Media extensions GuessFileIdx considers (server.js:62039).
const MEDIA_EXT_RE: &str =
    r"(?i)\.(mkv|avi|mp4|wmv|vp8|mov|mpg|ts|m3u8|webm|flac|mp3|wav|wma|aac|ogg)$";

/// 40 hex chars (server.js:18111). 64-char (BT v2) is "not implemented".
const INFOHASH_RE: &str = r"^[0-9a-fA-F]{40}$";

fn media_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(MEDIA_EXT_RE).expect("media regex"))
}

pub(crate) fn is_valid_infohash(ih: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(INFOHASH_RE).expect("infohash regex"))
        .is_match(ih)
}

/// Body of `POST /:ih/create`. All fields optional; video/core send a subset.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateMagnetBody {
    #[serde(default)]
    peer_search: Option<PeerSearchBody>,
    /// `{}` / `{season,episode}` to guess, or `false` to suppress.
    #[serde(default)]
    guess_file_idx: serde_json::Value,
    #[serde(default)]
    file_must_include: Vec<String>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerSearchBody {
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    min: Option<u64>,
    #[serde(default)]
    max: Option<u64>,
}

/// Body of `POST /create` - raw `.torrent` as hex. `from` is intentionally
/// unsupported (see spec §5: local-read + SSRF).
#[derive(Deserialize)]
pub(crate) struct CreateBlobBody {
    blob: Option<String>,
}

/// Match the blob's `onErr` → 500 empty body. No 4xx from these routes.
fn err500() -> Response {
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// `POST /create` - raw `.torrent` blob only.
pub(crate) async fn create_blob(
    State(engine): State<Engine>,
    body: Json<CreateBlobBody>,
) -> Response {
    let Some(blob) = &body.blob else {
        return err500(); // missing/non-string blob → JS falls to `from` → onErr
    };
    let Ok(bytes) = hex::decode(blob.trim()) else {
        return err500();
    };
    let select = |files: &[types::File]| Ok(browse_selection(files, None));
    let handle = match engine.add_blob(bytes, Pick::Decide(&select)).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("add_blob failed: {e:#}");
            return err500();
        }
    };
    // Freshly added: mark active so the sweeper doesn't evict it before first play.
    engine.touch(&Engine::info_hash_hex(&handle));
    let stats = engine.statistics(&handle, cache_path(&handle), default_peer_search(&handle), None);
    Json(CreateResponse {
        statistics: stats,
        guessed_file_idx: None,
    })
    .into_response()
}

/// `POST /:ih/create` - magnet/infohash + peerSearch + guess/selector.
pub(crate) async fn create_magnet(
    State(engine): State<Engine>,
    Path(info_hash): Path<String>,
    body: Option<Json<CreateMagnetBody>>,
) -> Response {
    if !is_valid_infohash(&info_hash) {
        return err500(); // 64-char BT v2 / garbage → "not implemented"/error
    }
    let info_hash = info_hash.to_lowercase();
    let body = body.map(|Json(b)| b).unwrap_or_default();

    let magnet = build_magnet(&info_hash, body.peer_search.as_ref());
    let select = |files: &[types::File]| Ok(browse_selection(files, resolve_index(files, &body)));
    let handle = match engine.add_magnet(&magnet, Pick::Decide(&select)).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("add_magnet failed: {e:#}");
            return err500();
        }
    };
    engine.touch(&info_hash);

    let files = Engine::files(&handle);
    let guessed = resolve_index(&files, &body);

    let peer_search = body
        .peer_search
        .as_ref()
        .map(|ps| PeerSearch {
            min: ps.min.unwrap_or(40),
            max: ps.max.unwrap_or(200),
            sources: ps.sources.clone(),
        })
        .unwrap_or_else(|| default_peer_search(&handle));

    let stats = engine.statistics(&handle, cache_path(&handle), peer_search, None);
    Json(CreateResponse {
        statistics: stats,
        guessed_file_idx: guessed,
    })
    .into_response()
}

/// `GET /:ih/remove` - stop and forget one torrent. Always `200 {}`.
pub(crate) async fn remove(State(engine): State<Engine>, Path(info_hash): Path<String>) -> Response {
    if is_valid_infohash(&info_hash) {
        engine.remove(&info_hash.to_lowercase()).await;
    }
    Json(serde_json::json!({})).into_response()
}

/// `GET /removeAll` - stop and forget every torrent. Always `200 {}`.
pub(crate) async fn remove_all(State(engine): State<Engine>) -> Response {
    engine.remove_all().await;
    Json(serde_json::json!({})).into_response()
}

/// What a create downloads when it is NEW. Both create routes are the "open
/// this magnet / .torrent" flow: they add the torrent so the app can list its
/// files, and no file has been played yet. So:
/// - a file the request itself asked for (`guessFileIdx` / `fileMustInclude`,
///   the same `guessedFileIdx` the response reports) is selected alone;
/// - otherwise the torrent's unambiguous main feature (the only video, or one
///   that dwarfs the rest, [`main_feature`]) is selected alone: a movie starts
///   while its trailer, extras and .nfo do not;
/// - otherwise (a season pack, or no video at all) NOTHING is selected. The
///   torrent is added and browsable but downloads nothing until a file is
///   actually played (the stream route then selects exactly that file) or
///   fetched from the Cache page. Guessing an episode would be a coin flip,
///   and "everything" is the bug this replaced.
///
/// An already-managed torrent keeps whatever selection it has (see
/// `Engine::add_magnet`); this only decides a fresh add.
fn browse_selection(files: &[types::File], requested: Option<i64>) -> Vec<usize> {
    if let Some(i) = requested.and_then(|i| usize::try_from(i).ok()).filter(|&i| i < files.len()) {
        return vec![i];
    }
    let videos: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| is_video(&f.name))
        .map(|(i, _)| i)
        .collect();
    main_feature(&videos, |i| files[i].length).into_iter().collect()
}

/// Resolve `guessedFileIdx`: fileMustInclude selector wins, else guessFileIdx.
/// Returns None when no guess fires (matching the blob leaving it unset).
fn resolve_index(files: &[types::File], body: &CreateMagnetBody) -> Option<i64> {
    if !body.file_must_include.is_empty() {
        if let Some(i) = file_must_include(files, &body.file_must_include) {
            return Some(i as i64);
        }
    }
    // guessFileIdx: truthy object → guess; `false`/absent → no guess.
    if body.guess_file_idx.is_object() {
        return Some(guess_file_idx(files));
    }
    None
}

/// First file whose name matches any client pattern (server.js:18362-18378).
/// Patterns that fail to compile are skipped (mirrors "ignore evil regex");
/// the `regex` crate is linear-time so no timeout is needed (spec §5).
pub(crate) fn file_must_include(files: &[types::File], patterns: &[String]) -> Option<usize> {
    let compiled: Vec<Regex> = patterns.iter().filter_map(|p| compile_js_pattern(p)).collect();
    files
        .iter()
        .position(|f| compiled.iter().any(|re| re.is_match(&f.name)))
}

/// Compile a `/pat/flags` or bare-string pattern. Returns None on unsupported
/// JS syntax (backreferences/lookaround) - skip, do not fall back.
fn compile_js_pattern(s: &str) -> Option<Regex> {
    static SLASHED: OnceLock<Regex> = OnceLock::new();
    let slashed = SLASHED.get_or_init(|| Regex::new(r"^/(.*)/(.*)$").expect("slashed regex"));
    let (pattern, case_insensitive) = match slashed.captures(s) {
        Some(c) => (
            c.get(1).unwrap().as_str().to_owned(),
            c.get(2).map(|m| m.as_str().contains('i')).unwrap_or(false),
        ),
        None => (s.to_owned(), false),
    };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(case_insensitive)
        .build()
        .ok()
}

/// GuessFileIdx: largest media file, or largest episode-matching media file
/// (server.js:62040-62058). M1 ports the media-largest path; series matching
/// (parseVideoName) is a later refinement that degrades to largest-media.
pub(crate) fn guess_file_idx(files: &[types::File]) -> i64 {
    let media: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| media_re().is_match(&f.path))
        .map(|(i, _)| i)
        .collect();
    if media.is_empty() {
        return -1;
    }
    media
        .into_iter()
        .max_by_key(|&i| files[i].length)
        .map(|i| i as i64)
        .unwrap_or(-1)
}

/// Build a magnet URL, appending `tracker:` peer-search sources as `tr=`.
fn build_magnet(info_hash: &str, peer_search: Option<&PeerSearchBody>) -> String {
    let mut magnet = format!("magnet:?xt=urn:btih:{info_hash}");
    if let Some(ps) = peer_search {
        for src in &ps.sources {
            if let Some(tr) = src.strip_prefix("tracker:") {
                magnet.push_str("&tr=");
                magnet.push_str(&urlencode(tr));
            }
        }
    }
    magnet
}

pub(crate) fn default_peer_search(handle: &crate::engine::Handle) -> PeerSearch {
    PeerSearch {
        min: 40,
        max: 200,
        sources: vec![format!("dht:{}", Engine::info_hash_hex(handle))],
    }
}

pub(crate) fn cache_path(handle: &crate::engine::Handle) -> String {
    // The blob echoes the per-torrent cache path in opts.path. librqbit lays
    // files under the session root; report the infohash subpath as a stand-in.
    Engine::info_hash_hex(handle)
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, len: u64) -> types::File {
        types::File {
            name: name.to_owned(),
            path: name.to_owned(),
            length: len,
            offset: 0,
        }
    }

    #[test]
    fn guess_picks_largest_media_file() {
        // The BBB layout: subtitle, video, poster. Only the .mp4 is media.
        let files = vec![
            file("Big Buck Bunny.en.srt", 140),
            file("Big Buck Bunny.mp4", 276_134_947),
            file("poster.jpg", 310_380),
        ];
        assert_eq!(guess_file_idx(&files), 1);
    }

    #[test]
    fn guess_ignores_larger_nonmedia() {
        // A huge .zip must not beat a smaller real video.
        let files = vec![file("huge.zip", 9_000_000_000), file("movie.mkv", 500)];
        assert_eq!(guess_file_idx(&files), 1);
    }

    #[test]
    fn guess_returns_minus_one_without_media() {
        let files = vec![file("readme.txt", 10), file("cover.png", 20)];
        assert_eq!(guess_file_idx(&files), -1);
    }

    #[test]
    fn file_must_include_bare_and_slashed() {
        let files = vec![file("S01E01.mkv", 1), file("S01E02.mkv", 1)];
        assert_eq!(file_must_include(&files, &["E02".to_owned()]), Some(1));
        assert_eq!(
            file_must_include(&files, &["/e02/i".to_owned()]),
            Some(1),
            "slashed /pat/i should be case-insensitive"
        );
    }

    #[test]
    fn unsupported_regex_is_skipped_not_fatal() {
        // Backreference: valid JS, unsupported by the `regex` crate -> skip.
        assert!(compile_js_pattern(r"(a)\1").is_none());
        // A skipped pattern just yields no match, never a panic.
        let files = vec![file("x.mkv", 1)];
        assert_eq!(file_must_include(&files, &[r"(a)\1".to_owned()]), None);
    }

    #[test]
    fn browse_selection_never_means_everything() {
        let movie = vec![
            file("Movie.mkv", 4_000_000),
            file("Movie.trailer.mp4", 300_000),
            file("Movie.nfo", 2_000),
        ];
        // No request: the unambiguous main feature alone.
        assert_eq!(browse_selection(&movie, None), vec![0]);
        // A file the request asked for (guessFileIdx / fileMustInclude) wins.
        assert_eq!(browse_selection(&movie, Some(1)), vec![1]);
        // A guess that found no media (-1) or an out-of-range one falls back.
        assert_eq!(browse_selection(&movie, Some(-1)), vec![0]);
        assert_eq!(browse_selection(&movie, Some(9)), vec![0]);

        // A season pack is ambiguous: nothing until an episode is played.
        let pack = vec![file("S01E01.mkv", 1_000_000), file("S01E02.mkv", 1_100_000)];
        assert!(browse_selection(&pack, None).is_empty());
        // ...unless the request itself named one.
        assert_eq!(browse_selection(&pack, Some(1)), vec![1]);

        // No video at all: nothing.
        assert!(browse_selection(&[file("readme.txt", 10), file("a.zip", 99)], None).is_empty());
    }

    #[test]
    fn infohash_validation() {
        assert!(is_valid_infohash("dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c"));
        assert!(!is_valid_infohash("tooshort"));
        // 64-char BT v2 is "not implemented".
        assert!(!is_valid_infohash(&"a".repeat(64)));
    }
}
