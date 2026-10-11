//! Trickplay: seek-bar hover thumbnails from a SHADOW mpv instance.
//!
//! Design after Harbor's thumbs sidecar (github.com/harborstremio/harbor,
//! MIT License, (c) Harbor contributors), reworked for an in-process libmpv:
//! Harbor spawns an `mpv.exe --input-ipc-server` subprocess because that is
//! what they ship; we ship only `libmpv-2.dll`, and libmpv supports multiple
//! independent contexts per process, so the shadow here is simply a second
//! [`Mpv`] with no window (`vo=null`), no audio, and a decode chain scaled to
//! thumbnail width - which makes both decoding and the screenshots cheap.
//!
//! The shadow opens the SAME url the player is streaming (normally the local
//! streaming server), seeks by keyframe to a bucketed time, and screenshots to
//! a temp jpg that returns to the web layer as a data URL. Keyframe seeks land
//! wherever the nearest keyframe is; a hover preview is an approximation by
//! nature, and this is what keeps it fast. For a torrent, a request into a
//! not-yet-downloaded region is just a range request the streaming server can
//! serve later: the seek stalls, the settle wait times out, no thumb is cached,
//! and a later hover retries.
//!
//! Concurrency: one worker thread, "latest wins" - while the user scrubs, only
//! the bucket under the cursor right now is worth generating, so `wanted`
//! holds at most one pending bucket and every newer request overwrites it. The
//! web layer polls (a miss returns `None` and the caller asks again on its next
//! hover tick); the cache fills in behind the cursor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::State;

use crate::mpv::{self, Mpv};

/// Thumbnail granularity. Finer buckets mean more captures while scrubbing but
/// previews that track the cursor closely; 2s matches the keyframe spacing of
/// typical web releases anyway.
const BUCKET_SECONDS: f64 = 2.0;
// The shadow chain's output width (`vf=scale`) is what a screenshot inherits, so
// no post-capture resize is ever needed. For a hover thumbnail that width is
// NOT owned here: the seek bar knows how wide it draws the preview and on what
// pixel density, and sends the device-pixel width with every request (see
// SeekBar.tsx PREVIEW_WIDTH_REM). A fixed width here is how previews went soft:
// 240 px captured, shown 224-288 CSS px wide and up to twice that in device
// pixels.
/// Ceiling on a requested thumbnail width. The preview is 16rem wide and the
/// root font tops out at 18 px, so this covers it up to a device pixel ratio
/// of 4; beyond that a request is clamped rather than refused (a clamped
/// thumbnail is merely less sharp, and the bound exists so a hover can never
/// ask the shadow for full-resolution captures).
const THUMB_WIDTH_MAX: u32 = 1152;
/// Chain width while the scene scan samples. Its frames are only ever reduced
/// to a 16x9 luma grid, so it keeps the small size no matter how large the
/// hover thumbnails are (and `SCENE_CUT_THRESHOLD` was tuned at this size).
const SCAN_WIDTH: u32 = 240;
/// High enough that a frame drawn 1:1 shows no blocking in skies and gradients
/// (72, the old value, was only acceptable while the frame was being blurred
/// by its own upscale). Measured on a 1080p film at 448 px: ~16 KB per frame.
const JPEG_QUALITY: u32 = 90;
/// How long a keyframe seek may take before the capture is abandoned (a live
/// torrent region that is not downloaded yet stalls exactly here).
const SEEK_SETTLE_MS: u64 = 4000;
const SETTLE_POLL: Duration = Duration::from_millis(25);

// Scene scan: the shadow sweeps the whole file at a coarse spacing, comparing
// tiny luma grids of consecutive samples; strong differences are scene cuts.
// The scan runs in the SAME worker, always yielding to hover-thumbnail
// requests, so scrubbing never waits on it. Results feed the seek bar's
// derived chapter segments for files that carry no chapter marks.
/// Sample spacing in seconds; ~20s resolves chapter-level boundaries without
/// turning the scan into a full decode.
const SCAN_SPACING_SECONDS: f64 = 20.0;
/// Cap on samples for very long files (a 4h file scans at coarser spacing).
const SCAN_MAX_SAMPLES: usize = 240;
/// Mean absolute luma difference (0-255) on the 16x9 grid above which two
/// consecutive samples are considered different scenes.
const SCENE_CUT_THRESHOLD: f64 = 28.0;
/// At most this many cuts survive selection (strongest first, min-gap apart).
const SCENE_CUT_MAX: usize = 24;

struct Scan {
    spacing: f64,
    total: usize,
    next: usize,
    prev_luma: Option<Vec<u8>>,
    /// (time_sec, strength) of every raw cut candidate.
    candidates: Vec<(f64, f64)>,
    /// Selected cut times, present once the sweep finishes.
    result: Option<Vec<f64>>,
    /// Minimum distance between two kept cuts (derived from duration).
    min_gap: f64,
}

impl Scan {
    fn new(duration: f64) -> Self {
        let total = ((duration / SCAN_SPACING_SECONDS) as usize).clamp(2, SCAN_MAX_SAMPLES);
        Self {
            spacing: duration / total as f64,
            total,
            next: 0,
            prev_luma: None,
            candidates: Vec::new(),
            result: None,
            min_gap: (duration * 0.03).max(45.0),
        }
    }

    /// Greedy strongest-first selection with a minimum gap, then chronological.
    fn select(&mut self) {
        let mut sorted = self.candidates.clone();
        sorted.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut kept: Vec<f64> = Vec::new();
        for (time, _) in sorted {
            if kept.len() >= SCENE_CUT_MAX {
                break;
            }
            if kept.iter().all(|k| (k - time).abs() >= self.min_gap) {
                kept.push(time);
            }
        }
        kept.sort_by(|a, b| a.total_cmp(b));
        self.result = Some(kept);
    }
}

/// Decode a screenshot jpg down to a 16x9 luma grid for scene comparison.
fn luma_grid(jpeg: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .map_err(|e| format!("thumbs: scan decode: {e}"))?;
    Ok(img
        .resize_exact(16, 9, image::imageops::FilterType::Triangle)
        .to_luma8()
        .into_raw())
}

fn luma_diff(a: &[u8], b: &[u8]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let sum: u64 = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u64).sum();
    sum as f64 / a.len() as f64
}

#[derive(Default)]
pub struct ThumbsState(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    /// The stream url thumbnails are being generated for. Changing it clears
    /// everything: cache keys are only meaningful per url.
    url: Option<String>,
    /// Monotonic generation counter; bumped on every url change so a capture
    /// finishing late cannot cache into the wrong stream's map.
    generation: u64,
    shadow: Option<Shadow>,
    /// The pixel width every cached frame was captured at (0 before the first
    /// request). A request for another width clears the cache: a preview drawn
    /// larger must not be served the small frames captured for the old size.
    width: u32,
    cache: HashMap<u32, String>,
    /// The single pending bucket - latest hover wins.
    wanted: Option<u32>,
    /// The scene sweep for the current url; created by the worker (it needs the
    /// loaded file's duration) once requested, never blocking a thumbnail request.
    scan: Option<Scan>,
    scan_requested: bool,
    worker_running: bool,
    /// After a failed shadow spawn (no dll / broken stream), stop retrying for
    /// this url; every later request would fail the same way.
    disabled: bool,
}

struct Shadow {
    mpv: Mpv,
    /// The width the `vf=scale` chain currently outputs.
    width: u32,
}

/// The shadow's filter chain for an output `width`: scale down to it, keeping
/// the aspect ratio on an even height. Never up: a source narrower than the
/// request is captured as it is. Spelled as a raw libavfilter graph because
/// mpv's own `scale=` shorthand rejects the `min()` expression outright.
fn scale_filter(width: u32) -> String {
    format!("lavfi=[scale=w=min(iw\\,{width}):h=-2]")
}

/// The capture width for a requested one: even (the chain's pixel format
/// subsamples chroma by two) and at most [`THUMB_WIDTH_MAX`].
fn thumb_width(requested: u32) -> Result<u32, String> {
    if requested == 0 {
        return Err("thumbs: requested a zero-width thumbnail".into());
    }
    Ok(requested.min(THUMB_WIDTH_MAX).next_multiple_of(2))
}

impl Shadow {
    /// A windowless, muted mpv over `url` whose decode chain outputs `width`.
    /// Options follow Harbor's shadow flags; `hr-seek=no` + keyframe seeks are
    /// the speed.
    fn spawn(url: &str, width: u32) -> Result<Self, String> {
        let mpv = Mpv::load(&mpv::default_dll_path())?;
        for (name, value) in [
            ("vo", "null"),
            ("aid", "no"),
            ("sid", "no"),
            ("pause", "yes"),
            ("keep-open", "yes"),
            ("idle", "yes"),
            ("config", "no"),
            ("load-scripts", "no"),
            ("ytdl", "no"),
            ("osc", "no"),
            ("hwdec", "no"),
            ("cache", "yes"),
            ("demuxer-max-bytes", "32MiB"),
            ("hr-seek", "no"),
            ("screenshot-format", "jpg"),
            ("screenshot-jpeg-quality", &JPEG_QUALITY.to_string()),
            ("screenshot-high-bit-depth", "no"),
            ("screenshot-tag-colorspace", "no"),
        ] {
            // Options a given libmpv build lacks must not kill the feature.
            if let Err(e) = mpv.set_option(name, value) {
                tracing::debug!("thumbs: option {name}={value}: {e}");
            }
        }
        // The one option that is not optional: without the scale every
        // screenshot is a full-resolution frame (megabytes per hover).
        mpv.set_option("vf", &scale_filter(width))
            .map_err(|e| format!("thumbs: scaling to {width}px failed: {e}"))?;
        mpv.initialize()?;
        mpv.command(&["loadfile", url])?;
        Ok(Self { mpv, width })
    }

    /// Rebuild the filter chain for another output width (a no-op when it
    /// already matches). Hover thumbnails and the scene scan share this one
    /// shadow at different sizes; the frame on screen keeps its old size, so
    /// callers seek before they capture.
    fn set_width(&mut self, width: u32) -> Result<(), String> {
        if self.width == width {
            return Ok(());
        }
        self.mpv
            .set_property("vf", &scale_filter(width))
            .map_err(|e| format!("thumbs: scaling to {width}px failed: {e}"))?;
        self.width = width;
        Ok(())
    }

    /// Keyframe-seek to `target` and wait for the seek to settle. `false` on
    /// timeout (undownloaded torrent region / dead stream) - do not capture.
    fn seek_settled(&self, target: f64) -> bool {
        if self.mpv.command(&["seek", &format!("{target:.3}"), "absolute+keyframes"]).is_err() {
            return false;
        }
        let deadline = Instant::now() + Duration::from_millis(SEEK_SETTLE_MS);
        while Instant::now() < deadline {
            std::thread::sleep(SETTLE_POLL);
            match self.mpv.get_property_string("seeking").as_deref() {
                Some("no") => {
                    // A settled seek with no frame decoded yet reports no
                    // time-pos; require one so the screenshot has a frame.
                    if self.mpv.get_property_string("time-pos").is_some() {
                        return true;
                    }
                }
                // "yes" = still seeking; None = property gone (file replaced).
                Some(_) | None => {}
            }
        }
        false
    }

    /// Screenshot the current (already thumbnail-scaled) frame to a fresh temp
    /// jpg and return its bytes. Synchronous: on Ok the frame was captured.
    fn capture_bytes(&self) -> Result<Vec<u8>, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let temp = std::env::temp_dir().join(format!("rillio-thumb-{nanos}.jpg"));
        let path = temp.to_string_lossy().to_string();
        self.mpv
            .command(&["screenshot-to-file", &path, "video"])
            .map_err(|e| format!("thumbs: screenshot failed: {e}"))?;
        let bytes = std::fs::read(&temp).map_err(|e| format!("thumbs: reading {path}: {e}"))?;
        let _ = std::fs::remove_file(&temp);
        if bytes.is_empty() {
            return Err("thumbs: empty screenshot".into());
        }
        Ok(bytes)
    }

    /// capture_bytes as a data URL, for the web layer's <img src>.
    fn capture(&self) -> Result<String, String> {
        let bytes = self.capture_bytes()?;
        use base64::Engine as _;
        Ok(format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        ))
    }

    fn duration(&self) -> Option<f64> {
        self.mpv
            .get_property_string("duration")
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|d| d.is_finite() && *d > 0.0)
    }
}

/// Same rule as the player bridge's stream validation (shell.rs): the web layer
/// may only point the shadow at http(s), never local paths or mpv's pseudo
/// protocols (`av://`, `edl://`, ...) - those are exfil/execution vectors.
///
/// Deliberately NOT widened to `rillio://` the way shell.rs was. The shadow is a
/// SECOND mpv instance and the protocol is registered per instance, so the
/// scheme would not resolve here at all; and a second FileStream on the same
/// file competes with the player's for piece priority, which is exactly the
/// stall the HTTP path avoids by serving the shadow from the same cached bytes.
/// `resolve_shadow_url` maps the byte-plane url back to HTTP before this runs.
/// Shared with autosync.rs, whose decode shadow has the same constraints.
pub(crate) fn validate_url(url: &str) -> Result<(), String> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        Ok(())
    } else {
        Err(format!("thumbs: BLOCKED non-http(s) url: {url}"))
    }
}

/// The url the SHADOW should open for what the player is playing.
///
/// Normally the identity: the web layer hands over `video.state.stream.url`,
/// which stays the HTTP form even when the player itself loaded `rillio://`.
/// This exists so the shadow survives a caller that passes the byte-plane url
/// anyway: `rillio://<ih>/<idx>` maps back to `<base_url>/<ih>/<idx>` on the
/// port the server really bound. Anything else is returned untouched for
/// [`validate_url`] to judge.
pub(crate) fn resolve_shadow_url(app: &tauri::AppHandle, url: &str) -> Result<String, String> {
    use tauri::Manager;

    if !url.trim().to_ascii_lowercase().starts_with("rillio://") {
        return Ok(url.to_owned());
    }
    let base_url = app.state::<crate::ServerHandle>().get().map(|s| s.base_url.clone());
    shadow_url_for(url, base_url.as_deref())
}

/// The pure half of [`resolve_shadow_url`]: `base_url` is whatever the server
/// bound (`None` while it is still binding).
fn shadow_url_for(url: &str, base_url: Option<&str>) -> Result<String, String> {
    let trimmed = url.trim();
    if !trimmed.to_ascii_lowercase().starts_with("rillio://") {
        return Ok(url.to_owned());
    }
    let (info_hash, file_idx) = crate::stream_cb::parse_url(trimmed)
        .map_err(|e| format!("thumbs: BLOCKED malformed rillio:// url {url}: {e}"))?;
    let base_url =
        base_url.ok_or_else(|| "thumbs: the streaming server has not bound yet".to_string())?;
    Ok(format!("{}/{info_hash}/{file_idx}", base_url.trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    const IH: &str = "0123456789abcdef0123456789abcdef01234567";
    const BASE: &str = "http://127.0.0.1:49312/";

    /// The shadow must open the HTTP form of whatever the player loaded, on the
    /// port the server really got - never `rillio://`, which is registered on
    /// the PLAYER's mpv instance and not on this one.
    #[test]
    fn shadow_maps_the_byte_plane_back_to_http() {
        assert_eq!(
            shadow_url_for(&format!("rillio://{IH}/3"), Some(BASE)),
            Ok(format!("http://127.0.0.1:49312/{IH}/3"))
        );
        // No trailing slash on the base, same answer (no doubled separator).
        assert_eq!(
            shadow_url_for(&format!("rillio://{IH}/0"), Some("http://127.0.0.1:11470")),
            Ok(format!("http://127.0.0.1:11470/{IH}/0"))
        );
        // The mapped url is exactly what validate_url accepts.
        let mapped = shadow_url_for(&format!("rillio://{IH}/1"), Some(BASE)).unwrap();
        validate_url(&mapped).unwrap();
    }

    /// Everything else passes through untouched for validate_url to judge.
    #[test]
    fn shadow_leaves_other_urls_alone() {
        for url in [
            format!("http://127.0.0.1:11470/{IH}/0"),
            "https://cdn.example/a.mkv".to_owned(),
            "av://lavfi:testsrc".to_owned(),
        ] {
            assert_eq!(shadow_url_for(&url, Some(BASE)), Ok(url.clone()), "{url:?}");
        }
        // ...and the pass-through still gets rejected by the scheme rule.
        assert!(validate_url(&shadow_url_for("av://lavfi:testsrc", Some(BASE)).unwrap()).is_err());
    }

    /// A malformed byte-plane url is refused here too (it never becomes a
    /// plausible-looking http url), and an unbound server fails loud.
    #[test]
    fn shadow_refuses_what_it_cannot_map() {
        assert!(shadow_url_for(&format!("rillio://{IH}/0/movie.mkv"), Some(BASE)).is_err());
        assert!(shadow_url_for(&format!("rillio://{IH}/-1"), Some(BASE)).is_err());
        assert!(shadow_url_for("rillio://evil.com/0", Some(BASE)).is_err());
        assert!(shadow_url_for(&format!("rillio://{IH}/0"), None).is_err());
    }

    /// A requested width is captured as asked when it is sane, evened up when
    /// odd, capped when absurd, and refused when it is nothing at all.
    #[test]
    fn thumb_width_bounds_the_request() {
        assert_eq!(thumb_width(448), Ok(448));
        assert_eq!(thumb_width(335), Ok(336));
        assert_eq!(thumb_width(1), Ok(2));
        assert_eq!(thumb_width(THUMB_WIDTH_MAX), Ok(THUMB_WIDTH_MAX));
        assert_eq!(thumb_width(THUMB_WIDTH_MAX + 1), Ok(THUMB_WIDTH_MAX));
        assert_eq!(thumb_width(u32::MAX), Ok(THUMB_WIDTH_MAX));
        assert!(thumb_width(0).is_err());
        // The cap itself must be a width the chain can output.
        assert_eq!(THUMB_WIDTH_MAX % 2, 0);
    }

    /// Measures the real shadow against a local file: pixel size, jpg bytes and
    /// seek + screenshot time per thumbnail at each width, then the same
    /// captures interleaved with scene-scan-sized ones (the filter rebuild a
    /// hover pays when it lands between two sweep samples). Needs
    /// `libmpv-2.dll` (next to the test binary or via `RILLIO_LIBMPV`) and:
    ///   RILLIO_THUMBS_BENCH_FILE    the video to open
    ///   RILLIO_THUMBS_BENCH_WIDTHS  comma separated widths, e.g. `240,448`
    ///   RILLIO_THUMBS_BENCH_OUT     optional directory to keep the jpgs in
    /// Run: `cargo test --lib thumbs::tests::bench -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs libmpv-2.dll and RILLIO_THUMBS_BENCH_FILE"]
    fn bench_capture_sizes() {
        const SAMPLES: u32 = 12;
        const ASPECT_SLACK_PX: u32 = 2;
        let file = std::env::var("RILLIO_THUMBS_BENCH_FILE").expect("RILLIO_THUMBS_BENCH_FILE");
        let widths: Vec<u32> = std::env::var("RILLIO_THUMBS_BENCH_WIDTHS")
            .expect("RILLIO_THUMBS_BENCH_WIDTHS")
            .split(',')
            .map(|w| thumb_width(w.trim().parse().expect("a width")).expect("a usable width"))
            .collect();
        let out = std::env::var("RILLIO_THUMBS_BENCH_OUT").ok().map(std::path::PathBuf::from);

        let mut shadow = Shadow::spawn(&file, widths[0]).expect("shadow spawn");
        // `loadfile` is asynchronous; a seek is refused until the file is open.
        let opened = Instant::now() + Duration::from_millis(SEEK_SETTLE_MS);
        while shadow.duration().is_none() && Instant::now() < opened {
            std::thread::sleep(SETTLE_POLL);
        }
        let duration = shadow.duration().expect("the file never opened");
        let source_width: u32 = shadow
            .mpv
            .get_property_string("width")
            .and_then(|w| w.parse().ok())
            .expect("the source width");
        // Evenly spread interior positions, the same for every width.
        let times: Vec<f64> =
            (1..=SAMPLES).map(|i| duration * f64::from(i) / f64::from(SAMPLES + 1)).collect();

        for interleave_scan in [false, true] {
            for &width in &widths {
                let mut bytes_total = 0usize;
                let mut millis: Vec<f64> = Vec::new();
                let mut scan_millis: Vec<f64> = Vec::new();
                let mut size = (0, 0);
                for (index, &time) in times.iter().enumerate() {
                    if interleave_scan {
                        let started = Instant::now();
                        shadow.set_width(SCAN_WIDTH).expect("scan width");
                        assert!(shadow.seek_settled(time), "scan seek to {time:.0}s");
                        luma_grid(&shadow.capture_bytes().expect("scan capture")).expect("luma");
                        scan_millis.push(started.elapsed().as_secs_f64() * 1000.0);
                    }
                    let started = Instant::now();
                    shadow.set_width(width).expect("thumb width");
                    assert!(shadow.seek_settled(time), "seek to {time:.0}s did not settle");
                    let jpeg = shadow.capture_bytes().expect("capture");
                    millis.push(started.elapsed().as_secs_f64() * 1000.0);
                    bytes_total += jpeg.len();
                    let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
                        .expect("a jpg");
                    size = (img.width(), img.height());
                    // The requested width, or the source's when that is
                    // narrower (never upscaled). Not exact: the even height
                    // bends the aspect ratio a hair and the screenshot squares
                    // the pixels back up.
                    assert!(
                        img.width().abs_diff(width.min(source_width)) <= ASPECT_SLACK_PX,
                        "asked for {width}px of a {source_width}px source, captured {}px",
                        img.width()
                    );
                    if let (Some(dir), false) = (out.as_deref(), interleave_scan) {
                        std::fs::write(dir.join(format!("w{width}-{index:02}.jpg")), &jpeg)
                            .expect("writing the jpg");
                    }
                }
                let n = times.len();
                // The median: one slow keyframe seek would own the mean.
                millis.sort_by(|a, b| a.total_cmp(b));
                println!(
                    "width {width} scan-interleaved={interleave_scan}: {}x{} px, {} bytes/thumb \
                     ({} as a data URL), median {:.0} ms, slowest {:.0} ms over {n} samples",
                    size.0,
                    size.1,
                    bytes_total / n,
                    (bytes_total / n).div_ceil(3) * 4,
                    millis[n / 2],
                    millis[n - 1],
                );
                if interleave_scan {
                    scan_millis.sort_by(|a, b| a.total_cmp(b));
                    println!(
                        "  scan sample between them (rebuild to {SCAN_WIDTH}px + seek + capture + \
                         luma grid): median {:.0} ms",
                        scan_millis[n / 2],
                    );
                }
            }
        }
    }
}

/// One thumbnail request: `Some(dataUrl)` on a cache hit, `None` while the
/// worker generates (the web layer polls again on its next hover tick).
/// `width_px` is the width the preview is drawn at, in device pixels.
#[tauri::command]
pub async fn player_thumb(
    app: tauri::AppHandle,
    state: State<'_, ThumbsState>,
    url: String,
    time_sec: f64,
    width_px: u32,
) -> Result<Option<String>, String> {
    if !time_sec.is_finite() || time_sec < 0.0 {
        return Ok(None);
    }
    let url = resolve_shadow_url(&app, &url)?;
    validate_url(&url)?;
    let width = thumb_width(width_px)?;
    let bucket = (time_sec / BUCKET_SECONDS).round() as u32;

    let inner_arc = state.0.clone();
    let mut inner = inner_arc.lock().map_err(|_| "thumbs: poisoned")?;
    if inner.url.as_deref() != Some(url.as_str()) {
        // New stream: drop the old shadow (its decoder pipeline is for the old
        // url) and every cached frame with it.
        inner.url = Some(url);
        inner.generation += 1;
        inner.shadow = None;
        inner.cache.clear();
        inner.wanted = None;
        inner.scan = None;
        inner.scan_requested = false;
        inner.disabled = false;
    }
    if inner.width != width {
        // The preview changed size (window moved to a denser screen, zoom):
        // the shadow stays, the frames captured for the old size go.
        inner.width = width;
        inner.cache.clear();
        inner.wanted = None;
    }
    if let Some(hit) = inner.cache.get(&bucket) {
        return Ok(Some(hit.clone()));
    }
    if inner.disabled {
        return Ok(None);
    }
    inner.wanted = Some(bucket);
    if !inner.worker_running {
        inner.worker_running = true;
        let arc = inner_arc.clone();
        // A dedicated OS thread: every step (mpv FFI, settle polling, file IO)
        // is blocking, and exactly one capture runs at a time.
        std::thread::spawn(move || worker(arc));
    }
    Ok(None)
}

/// Tear the shadow down (player unmount / playback stop). Frees the decoder
/// and closes the shadow's connection to the streaming server.
#[tauri::command]
pub async fn player_thumb_stop(state: State<'_, ThumbsState>) -> Result<(), String> {
    let mut inner = state.0.lock().map_err(|_| "thumbs: poisoned")?;
    inner.url = None;
    inner.generation += 1;
    inner.shadow = None;
    inner.cache.clear();
    inner.wanted = None;
    inner.scan = None;
    inner.scan_requested = false;
    inner.disabled = false;
    Ok(())
}

/// Scene-cut times (seconds) for `url`, from the shadow's background sweep:
/// `None` while the sweep runs (poll again), `Some(times)` when done - empty
/// when the shadow cannot run (no dll / broken stream) so the caller stops
/// polling.
#[tauri::command]
pub async fn player_scene_cuts(
    app: tauri::AppHandle,
    state: State<'_, ThumbsState>,
    url: String,
) -> Result<Option<Vec<f64>>, String> {
    let url = resolve_shadow_url(&app, &url)?;
    validate_url(&url)?;
    let inner_arc = state.0.clone();
    let mut inner = inner_arc.lock().map_err(|_| "thumbs: poisoned")?;
    if inner.url.as_deref() != Some(url.as_str()) {
        inner.url = Some(url);
        inner.generation += 1;
        inner.shadow = None;
        inner.cache.clear();
        inner.wanted = None;
        inner.scan = None;
        inner.scan_requested = false;
        inner.disabled = false;
    }
    if inner.disabled {
        return Ok(Some(Vec::new()));
    }
    if let Some(result) = inner.scan.as_ref().and_then(|scan| scan.result.clone()) {
        return Ok(Some(result));
    }
    inner.scan_requested = true;
    if !inner.worker_running {
        inner.worker_running = true;
        let arc = inner_arc.clone();
        std::thread::spawn(move || worker(arc));
    }
    Ok(None)
}

enum Job {
    /// A hover-thumbnail request (always wins over the scan): the bucket and
    /// the width to capture it at.
    Thumb(u32, u32),
    /// One scene-sweep step (create the Scan first if needed).
    ScanStep,
}

fn worker(inner_arc: Arc<Mutex<Inner>>) {
    loop {
        // Take the next job under the lock; NEVER hold the lock while driving
        // mpv (captures take up to seconds and player_thumb must stay instant).
        let (job, url, generation, shadow_missing) = {
            let mut inner = match inner_arc.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            let job = if let Some(bucket) = inner.wanted.take() {
                if inner.cache.contains_key(&bucket) {
                    continue;
                }
                Job::Thumb(bucket, inner.width)
            } else if inner.scan_requested &&
                inner.scan.as_ref().map_or(true, |scan| scan.result.is_none()) {
                Job::ScanStep
            } else {
                inner.worker_running = false;
                return;
            };
            let Some(url) = inner.url.clone() else {
                inner.worker_running = false;
                return;
            };
            (job, url, inner.generation, inner.shadow.is_none())
        };

        if shadow_missing {
            // Born at the size its first job needs, so a spawn is never
            // followed straight away by a filter rebuild.
            let width = match job {
                Job::Thumb(_, width) => width,
                Job::ScanStep => SCAN_WIDTH,
            };
            match Shadow::spawn(&url, width) {
                Ok(shadow) => {
                    let mut inner = match inner_arc.lock() {
                        Ok(guard) => guard,
                        Err(_) => return,
                    };
                    if inner.generation != generation {
                        // Url changed while spawning; the shadow is for the old
                        // stream. Drop it and loop for the new state.
                        continue;
                    }
                    inner.shadow = Some(shadow);
                }
                Err(e) => {
                    tracing::warn!("thumbs: shadow spawn failed: {e}");
                    let mut inner = match inner_arc.lock() {
                        Ok(guard) => guard,
                        Err(_) => return,
                    };
                    if inner.generation == generation {
                        inner.disabled = true;
                        inner.wanted = None;
                    }
                    inner.worker_running = false;
                    return;
                }
            }
        }

        // Drive the capture OUTSIDE the lock (it takes up to seconds, and
        // player_thumb must stay instant): take the shadow out under the lock,
        // capture, put it back. Ownership moves to this thread for the
        // duration, so a concurrent url change can only make the RESULT stale
        // (the generation check below discards it) - it never races the mpv.
        let taken = {
            let mut inner = match inner_arc.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            if inner.generation != generation {
                continue;
            }
            inner.shadow.take()
        };
        let Some(mut shadow) = taken else { continue };
        match job {
            Job::Thumb(bucket, width) => {
                let capture = shadow.set_width(width).and_then(|()| {
                    if shadow.seek_settled(bucket as f64 * BUCKET_SECONDS) {
                        shadow.capture()
                    } else {
                        Err("thumbs: seek did not settle (region not downloaded yet?)".into())
                    }
                });

                let mut inner = match inner_arc.lock() {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
                if inner.generation != generation {
                    // Url changed mid-capture: the shadow and the frame belong
                    // to the old stream; drop both.
                    continue;
                }
                inner.shadow = Some(shadow);
                match capture {
                    // The preview changed size mid-capture: this frame is the
                    // old size, and the cache now holds the new one only.
                    Ok(_) if inner.width != width => {}
                    Ok(data_url) => {
                        inner.cache.insert(bucket, data_url);
                    }
                    Err(e) => tracing::debug!("thumbs: bucket {bucket}: {e}"),
                }
            }
            Job::ScanStep => {
                // Snapshot the scan cursor under the lock, creating the Scan
                // from the loaded file's duration on the first step.
                let step = {
                    let mut inner = match inner_arc.lock() {
                        Ok(guard) => guard,
                        Err(_) => return,
                    };
                    if inner.generation != generation {
                        continue;
                    }
                    if inner.scan.is_none() {
                        match shadow.duration() {
                            Some(duration) => inner.scan = Some(Scan::new(duration)),
                            None => {
                                // File header not parsed yet: put the shadow
                                // back and let the loop retry (a thumbnail
                                // request may run in between).
                                inner.shadow = Some(shadow);
                                std::thread::sleep(Duration::from_millis(200));
                                continue;
                            }
                        }
                    }
                    let scan = inner.scan.as_ref().expect("just ensured");
                    (scan.next, scan.spacing, scan.prev_luma.clone())
                };
                let (index, spacing, prev_luma) = step;

                let target = index as f64 * spacing;
                // An unsettled seek (undownloaded torrent region) skips the
                // sample rather than stalling the sweep; prev resets so no
                // false cut spans the hole. The sweep samples at its own small
                // width whatever size the hover thumbnails are being cut at.
                let luma = match shadow.set_width(SCAN_WIDTH) {
                    Ok(()) if shadow.seek_settled(target) => {
                        shadow.capture_bytes().ok().and_then(|bytes| luma_grid(&bytes).ok())
                    }
                    Ok(()) => None,
                    Err(e) => {
                        tracing::warn!("thumbs: scene sweep sample {index}: {e}");
                        None
                    }
                };

                let mut inner = match inner_arc.lock() {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
                if inner.generation != generation {
                    continue;
                }
                inner.shadow = Some(shadow);
                if let Some(scan) = inner.scan.as_mut() {
                    if let (Some(prev), Some(current)) = (prev_luma.as_deref(), luma.as_deref()) {
                        let diff = luma_diff(prev, current);
                        if diff > SCENE_CUT_THRESHOLD {
                            // The cut happened somewhere between the two
                            // samples; the midpoint is the best guess.
                            scan.candidates.push((target - spacing / 2.0, diff));
                        }
                    }
                    scan.prev_luma = luma;
                    scan.next = index + 1;
                    if scan.next >= scan.total {
                        scan.select();
                        tracing::debug!(
                            "thumbs: scene sweep done: {} cuts from {} candidates",
                            scan.result.as_ref().map(|r| r.len()).unwrap_or(0),
                            scan.candidates.len(),
                        );
                    }
                }
            }
        }
    }
}
