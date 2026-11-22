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
//!
//! Colour: a screenshot is only as right as the conversion behind it. The
//! `vo=null` shadow converts the scaled frame to RGB in software, which shows
//! SDR BT.709-class video correctly and nothing else: a Dolby Vision profile 5
//! frame is IPTPQc2 that only its RPU turns into a picture (purple and green
//! without it), and PQ/HLG frames need tone mapping. Playback is right because
//! gpu-next renders through libplacebo, which applies the RPU and tone maps. So
//! once a stream is open, [`conversion_for`] reads what mpv reports about it,
//! and a stream that needs more than the software path gets a shadow whose
//! screenshots libplacebo renders, on a window nobody sees (see [`Render`]).
//!
//! Measured 2026-10-25 against the alternatives, with the shipped libmpv:
//! FFmpeg's `libplacebo` filter inside this same lavfi chain converts just as
//! correctly, but mpv rebuilds a lavfi graph on every seek and the filter
//! creates a Vulkan device each time (libplacebo logs ~1.1 s for it), so a
//! thumbnail took 0.9-1.9 s against 0.05-0.33 s for the gpu-next screenshot;
//! FFmpeg's zscale + tonemap cannot apply an RPU at all.

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
// SeekBar.tsx PREVIEW_WIDTH_REM, usePlayerThumb.ts OVERSAMPLE). A fixed width
// here is how previews went soft: 240 px captured, shown 224-288 CSS px wide
// and up to twice that in device pixels.
//
// The shell only bounds the request, and the bound is the largest width the
// web layer's own derivation (rem x root font px x pixel ratio x oversampling)
// can produce on a screen we mean to serve at full sharpness:
/// The preview's width in rem. Mirrors SeekBar.tsx PREVIEW_WIDTH_REM.
const PREVIEW_WIDTH_REM: u32 = 16;
/// The largest root font size the web app sets (tailwind.css, >= 2800 px wide).
const ROOT_FONT_PX_MAX: u32 = 18;
/// The densest screen served at full sharpness.
const DEVICE_PIXEL_RATIO_MAX: u32 = 4;
/// Captured pixels per drawn device pixel. Mirrors usePlayerThumb.ts OVERSAMPLE.
const OVERSAMPLE: u32 = 2;
/// Ceiling on a requested thumbnail width. A request beyond it is clamped
/// rather than refused: a clamped thumbnail is merely less sharp. The chain
/// never scales above the source width either way, so on real files the source
/// is usually the tighter bound.
const THUMB_WIDTH_MAX: u32 =
    PREVIEW_WIDTH_REM * ROOT_FONT_PX_MAX * DEVICE_PIXEL_RATIO_MAX * OVERSAMPLE;
/// Chain width while the scene scan samples. Its frames are only ever reduced
/// to a 16x9 luma grid, so it keeps the small size no matter how large the
/// hover thumbnails are (and `SCENE_CUT_THRESHOLD` was tuned at this size).
const SCAN_WIDTH: u32 = 240;
/// High enough that a frame drawn 1:1 shows no blocking in skies and gradients
/// (72, the old value, was only acceptable while the frame was being blurred
/// by its own upscale). Measured on a 1080p film at 448 px: ~16 KB per frame.
const JPEG_QUALITY: u32 = 90;
/// How many thumbnails the cache holds; once full, each new one overwrites the
/// least recently used. A frame is ~39 KB as a data URL at the 640 px a
/// 125%-scaled 1440p screen asks for (measured, 1080p film), so this is ~10 MB
/// there, and ~63 MB at the very worst (every frame a full 1920 px capture,
/// ~246 KB). Latest-wins scrubbing lands about ten captures a second, so the
/// cap is some 25 s of continuous scrubbing before the oldest ground is
/// recaptured. Mirrors thumbCache.js THUMB_CACHE_MAX.
const CACHE_MAX_THUMBS: usize = 256;
/// How long a keyframe seek may take before the capture is abandoned (a live
/// torrent region that is not downloaded yet stalls exactly here).
const SEEK_SETTLE_MS: u64 = 4000;
/// How long a shadow may stay in the FIRST-OPEN phase (the file never opened,
/// `Settle::Opening`) across seek budgets before it is retired as unreadable.
/// Wall-clock from spawn (`Shadow::born`, set fresh per spawn, so a respawn
/// gets its own full deadline), SEPARATE from the per-seek budget.
///
/// A real first open completes within ONE seek budget: the double-open bench
/// measured a cold DV/HDR open at ~1.3-1.8 s, well under SEEK_SETTLE_MS (4 s),
/// so 3x is headroom for a loaded machine, not a guess. Retiring past it is
/// SELF-CORRECTING, not data loss: the shadow is dropped (Lost, paced) and the
/// next hover opens a fresh one; it never fails the stream. The bound only
/// stops a dead/hung source (whose mpv error is a 600 s network-timeout away)
/// from leaking a shadow for as long as the user keeps hovering.
const OPENING_DEADLINE_MS: u64 = 3 * SEEK_SETTLE_MS;
/// How long a shadow may stay KEPT without showing a frame because its seeks
/// are `Settle::Busy` (the target's bytes in hand, the decode or the VO still
/// at work) or `Settle::Unknown`, counted from the first such verdict and not
/// restarted by later accepted seeks; only a captured frame clears it
/// ([`kept_overdue`]). Cached bytes do not guarantee a stalled decoder or VO
/// ever ends the load in error, so this is the bound.
///
/// The same allowance a whole first open gets ([`OPENING_DEADLINE_MS`]),
/// counted from when the bytes were in hand, so at least one seek budget more
/// in all: measured 2026-11-22, four cold 4K DV shadows opened together showed
/// their first frames within 1-16 s at 29-41% GPU. Under heavier GPU load
/// (minutes to a first frame) a shadow is retired here, and the loss paces
/// the next one by the retired one's lifetime ([`Inner::may_respawn`]): slow
/// retries, never the rapid respawn loop dropping on every timeout made.
const BUSY_DEADLINE_MS: u64 = OPENING_DEADLINE_MS;
const SETTLE_POLL: Duration = Duration::from_millis(25);
/// How many of mpv's error lines a libplacebo shadow keeps for its refusal: a
/// gpu-next start failure ends in four that say why (measured with a window
/// that cannot be used: no window, no GPU context, no VO, nothing to play).
const MPV_ERRORS_KEPT: usize = 4;

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

/// The captured thumbnails of one stream at one width, by bucket: a rolling
/// set of at most `max` frames. Reading a frame marks it used; adding one to a
/// full cache overwrites the frame that has gone unused the longest.
struct ThumbCache {
    max: usize,
    /// bucket -> (last use, data URL). `clock` only ever counts up, so the
    /// smallest stamp is the least recently used frame.
    frames: HashMap<u32, (u64, String)>,
    clock: u64,
}

impl Default for ThumbCache {
    fn default() -> Self {
        Self::with_max(CACHE_MAX_THUMBS)
    }
}

impl ThumbCache {
    fn with_max(max: usize) -> Self {
        Self { max, frames: HashMap::new(), clock: 0 }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn contains(&self, bucket: u32) -> bool {
        self.frames.contains_key(&bucket)
    }

    fn get(&mut self, bucket: u32) -> Option<&String> {
        let now = self.tick();
        let (used, data_url) = self.frames.get_mut(&bucket)?;
        *used = now;
        Some(data_url)
    }

    fn insert(&mut self, bucket: u32, data_url: String) {
        if !self.frames.contains_key(&bucket) && self.frames.len() >= self.max {
            // A scan of at most `max` stamps, once per capture (~100 ms each).
            let oldest = self.frames.iter().min_by_key(|(_, (used, _))| *used).map(|(b, _)| *b);
            if let Some(oldest) = oldest {
                self.frames.remove(&oldest);
            }
        }
        let now = self.tick();
        self.frames.insert(bucket, (now, data_url));
    }

    fn clear(&mut self) {
        self.frames.clear();
    }
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
    cache: ThumbCache,
    /// The single pending bucket - latest hover wins.
    wanted: Option<u32>,
    /// The scene sweep for the current url; created by the worker (it needs the
    /// loaded file's duration) once requested, never blocking a thumbnail request.
    scan: Option<Scan>,
    scan_requested: bool,
    worker_running: bool,
    /// Why this stream gets no previews at all ([`Inner::refuse`]): its frames
    /// cannot be converted, or no shadow can exist here. Every later request
    /// would end the same way, so none is made until the stream changes.
    refused: Option<String>,
    /// The last shadow this stream lost ([`Inner::lose_shadow`]): the stream
    /// could not be read then, which is no verdict on its frames.
    lost: Option<LostShadow>,
    /// What the stream's last checked frame needed ([`Inner::note_conversion`]):
    /// a frame that needs something else invalidates the cached previews.
    conversion: Option<Conversion>,
    /// The renderer the player's own video-params call for, carried so the
    /// FIRST shadow of a stream opens as that renderer instead of always
    /// starting Software and respawning on the first HDR/DV frame (the double
    /// open). Seeded from what the player already decoded; `None` until the
    /// player reports a frame, and then the Software default starts and
    /// [`checked_capture`] still moves it per frame. Never the decision owner:
    /// [`conversion_for`] is, here and per frame.
    hint_render: Option<Render>,
}

/// A shadow that was lost, kept to pace the next one ([`Inner::may_respawn`]).
#[derive(Debug, Clone)]
struct LostShadow {
    at: Instant,
    /// How long it lived before its load ended in error.
    lived: Duration,
    reason: String,
}

struct Shadow {
    /// When it was spawned (a lost shadow's lifetime paces the next one).
    born: Instant,
    mpv: Mpv,
    /// The width the `vf=scale` chain currently outputs.
    width: u32,
    /// How this shadow's screenshots are rendered. Nothing is captured from a
    /// frame whose conversion this does not serve ([`checked_capture`]).
    render: Render,
    /// The decoder asked of mpv ([`hwdec_for`]).
    hwdec: &'static str,
    /// Whether [`Shadow::note_decoder`] has reported a decoder fall-back.
    fallback_reported: std::cell::Cell<bool>,
    /// Since when this shadow has been kept without a frame by Busy/Unknown
    /// seeks ([`kept_overdue`]); `None` while it delivers frames.
    kept_since: std::cell::Cell<Option<Instant>>,
    /// mpv's latest error lines (a libplacebo shadow asks for them), for the
    /// refusal that says why gpu-next could not show the stream.
    mpv_errors: std::cell::RefCell<Vec<String>>,
    /// The never-shown window a [`Render::Libplacebo`] shadow renders into.
    /// Declared after `mpv` because fields drop in order: mpv's own output
    /// window is a child of this one and must go first.
    _window: Option<HiddenWindow>,
}

/// How the shadow turns a decoded, scaled frame into a screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Render {
    /// `vo=null`: mpv converts the frame to RGB in software. Right for SDR
    /// BT.709-class video only.
    Software,
    /// `vo=gpu-next` on a never-shown window, `screenshot-sw=no`: libplacebo
    /// renders the frame to SDR BT.709 with the player's own tone mapping
    /// ([`crate::shell::TONE_MAPPING_OPTIONS`]), applying a Dolby Vision RPU
    /// first. The `vf` chain still scales before it, so the GPU only ever sees
    /// a thumbnail-sized frame and the width rule is the same for both.
    Libplacebo,
}

/// Why a stream needs libplacebo.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Need {
    /// The frames carry a Dolby Vision RPU (mpv reports
    /// `colormatrix=dolbyvision`): applied, then tone mapped. `profile` is the
    /// track's, for the log: 5 is IPTPQc2 with no fallback picture, 8.x and 7
    /// put the RPU on an HDR10 / HLG base layer.
    DolbyVision { profile: Option<u8> },
    /// An HDR transfer (PQ, HLG, ...): tone mapped to SDR.
    Hdr { transfer: String },
    /// A colour system the software path does not decode (ICtCp, XYZ,
    /// constant-luminance BT.2020).
    ColourSystem { colormatrix: String },
    /// SDR on primaries wider than BT.709: gamut mapped, not shown desaturated.
    WideGamut { primaries: String },
}

/// What a stream's frames need to become a correct SDR BT.709 thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Conversion {
    /// SDR BT.709-class video: scaled and screenshot in software, as always.
    Plain,
    /// Rendered by libplacebo, for this reason.
    Libplacebo(Need),
}

impl Conversion {
    fn render(&self) -> Render {
        match self {
            Conversion::Plain => Render::Software,
            Conversion::Libplacebo(_) => Render::Libplacebo,
        }
    }
}

impl std::fmt::Display for Conversion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Conversion::Plain => write!(f, "SDR, plain software screenshot"),
            Conversion::Libplacebo(Need::DolbyVision { profile: Some(profile) }) => {
                write!(f, "Dolby Vision profile {profile}, RPU applied and tone mapped by libplacebo")
            }
            Conversion::Libplacebo(Need::DolbyVision { profile: None }) => {
                write!(f, "Dolby Vision, RPU applied and tone mapped by libplacebo")
            }
            Conversion::Libplacebo(Need::Hdr { transfer }) => {
                write!(f, "HDR ({transfer}), tone mapped by libplacebo")
            }
            Conversion::Libplacebo(Need::ColourSystem { colormatrix }) => {
                write!(f, "colour system {colormatrix}, converted by libplacebo")
            }
            Conversion::Libplacebo(Need::WideGamut { primaries }) => {
                write!(f, "wide gamut ({primaries}), gamut mapped by libplacebo")
            }
        }
    }
}

// The names below are mpv's (`video-params`), and every one mpv can report is
// listed on exactly one side: a name missing from both is refused by
// [`conversion_for`] rather than guessed at, so a name a later mpv adds fails
// loud instead of slipping through as SDR.

/// The colour matrix mpv reports for frames that carry a Dolby Vision RPU.
const DOLBY_VISION_MATRIX: &str = "dolbyvision";
/// The Dolby Vision profile with no backward-compatible base layer (IPTPQc2,
/// signal compatibility id 0). Profiles 7 and 8.x carry a real HDR10 / HLG
/// base layer that is a correct picture even when the RPU is missing; profile
/// 5 frames without their RPU are no picture at all.
const DV_PROFILE_WITHOUT_BASE_LAYER: u8 = 5;
/// Colour systems the software path decodes: plain YCbCr and RGB.
const SOFTWARE_MATRICES: &[&str] =
    &["bt.601", "bt.709", "smpte-240m", "bt.2020-ncl", "ycgco", "ycgco-re", "ycgco-ro", "rgb"];
/// Colour systems only libplacebo decodes.
const LIBPLACEBO_MATRICES: &[&str] =
    &[DOLBY_VISION_MATRIX, "bt.2020-cl", "bt.2100-pq", "bt.2100-hlg", "xyz"];
/// SDR transfer curves: the software path shows them as they are.
const SDR_TRANSFERS: &[&str] = &[
    "bt.1886", "srgb", "linear", "gamma1.8", "gamma2.0", "gamma2.2", "gamma2.4", "gamma2.6",
    "gamma2.8", "prophoto", "st428",
];
/// HDR transfer curves: only libplacebo tone maps them to SDR.
const HDR_TRANSFERS: &[&str] = &["pq", "hlg", "v-log", "s-log1", "s-log2", "scrgb"];
/// The BT.709 family of primaries, close enough that the software path has
/// always shown them without gamut mapping.
const NARROW_PRIMARIES: &[&str] = &["bt.709", "bt.601-525", "bt.601-625", "bt.470m", "ebu3213"];
/// Primaries wider than BT.709.
const WIDE_PRIMARIES: &[&str] = &[
    "bt.2020", "apple", "adobe", "prophoto", "cie1931", "dci-p3", "display-p3", "v-gamut",
    "s-gamut", "film-c", "aces-ap0", "aces-ap1",
];

/// The colour description mpv reports for the open video stream.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VideoParams {
    colormatrix: String,
    primaries: String,
    /// mpv's name for the transfer function.
    gamma: String,
    dolby_vision_profile: Option<u8>,
}

impl VideoParams {
    /// `video_params` is mpv's JSON for the `video-params` property (the
    /// decoded frames after mpv's own guessing, before the shadow's scale);
    /// `dolby_vision_profile` is `current-tracks/video/dolby-vision-profile`,
    /// which mpv only has for a Dolby Vision track.
    ///
    /// `Ok(None)` until the colour is reported: mpv fills `video-params` in two
    /// steps, the size from the container first (`{"w":3840,"h":1920}`), the
    /// full description with the first decoded frame.
    fn parse(
        video_params: &str,
        dolby_vision_profile: Option<&str>,
    ) -> Result<Option<Self>, String> {
        let json: serde_json::Value = serde_json::from_str(video_params)
            .map_err(|e| format!("thumbs: video-params is not JSON ({e}): {video_params}"))?;
        if json.get("colormatrix").is_none() {
            return Ok(None);
        }
        let field = |name: &str| {
            json.get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("thumbs: video-params has no {name}: {video_params}"))
        };
        let dolby_vision_profile = dolby_vision_profile
            .map(|profile| {
                profile
                    .trim()
                    .parse::<u8>()
                    .map_err(|e| format!("thumbs: dolby-vision-profile {profile:?}: {e}"))
            })
            .transpose()?;
        Ok(Some(Self {
            colormatrix: field("colormatrix")?,
            primaries: field("primaries")?,
            gamma: field("gamma")?,
            dolby_vision_profile,
        }))
    }
}

/// THE decision: what a stream with these parameters needs to become a
/// correct SDR BT.709 thumbnail, or why no conversion is known for it.
///
/// Decided from the frames' own description, never the file name. Light levels
/// (MaxCLL, mastering peak, DV L1, HDR10+) choose no route: libplacebo reads
/// them, or measures the frame, exactly as playback does. Order matters only
/// where a stream qualifies twice, and then the stronger need names it: an RPU
/// outranks the PQ base layer it rides on, an HDR curve outranks its gamut.
fn conversion_for(params: &VideoParams) -> Result<Conversion, String> {
    let VideoParams { colormatrix, primaries, gamma, dolby_vision_profile } = params;
    for (what, value, sides) in [
        ("colour matrix", colormatrix, [SOFTWARE_MATRICES, LIBPLACEBO_MATRICES]),
        ("transfer", gamma, [SDR_TRANSFERS, HDR_TRANSFERS]),
        ("primaries", primaries, [NARROW_PRIMARIES, WIDE_PRIMARIES]),
    ] {
        if !sides.iter().any(|side| side.contains(&value.as_str())) {
            return Err(format!(
                "thumbs: mpv reports {what} {value:?}, for which no conversion to SDR is known"
            ));
        }
    }
    if colormatrix != DOLBY_VISION_MATRIX
        && *dolby_vision_profile == Some(DV_PROFILE_WITHOUT_BASE_LAYER)
    {
        // Without the RPU these frames are IPTPQc2 under whatever tags they
        // carry; there is no HDR10 picture underneath to tone map instead.
        return Err(format!(
            "thumbs: a Dolby Vision profile {DV_PROFILE_WITHOUT_BASE_LAYER} track whose frames \
             arrive without their RPU (reported as {colormatrix}/{gamma}): profile \
             {DV_PROFILE_WITHOUT_BASE_LAYER} has no HDR10 base layer, so only the RPU makes a \
             picture of it"
        ));
    }
    Ok(if colormatrix == DOLBY_VISION_MATRIX {
        Conversion::Libplacebo(Need::DolbyVision { profile: *dolby_vision_profile })
    } else if HDR_TRANSFERS.contains(&gamma.as_str()) {
        Conversion::Libplacebo(Need::Hdr { transfer: gamma.clone() })
    } else if LIBPLACEBO_MATRICES.contains(&colormatrix.as_str()) {
        Conversion::Libplacebo(Need::ColourSystem { colormatrix: colormatrix.clone() })
    } else if WIDE_PRIMARIES.contains(&primaries.as_str()) {
        Conversion::Libplacebo(Need::WideGamut { primaries: primaries.clone() })
    } else {
        Conversion::Plain
    })
}

/// The renderer the player's own `video-params` call for: the same verdict
/// [`conversion_for`] reaches per frame, moved earlier so the first shadow for a
/// stream opens as the right renderer instead of starting Software and
/// respawning when the first HDR/DV frame lands (the double open, measured
/// 1.3-1.8 s). The player decoded this file already; its colour tags are the
/// stream's, not the renderer's, so they match what the shadow will see.
///
/// `None` keeps today's behaviour exactly: the player reported no frame yet
/// (just the container size), or its frames cannot be converted. Either way the
/// Software default starts and [`checked_capture`] decides on the real frame -
/// including the refusal, which is why the profile is not needed here (a hint
/// never refuses; the frame does).
fn initial_render(video_params: Option<&str>, profile: Option<&str>) -> Option<Render> {
    let params = VideoParams::parse(video_params?, profile).ok()??;
    conversion_for(&params).ok().map(|conversion| conversion.render())
}

/// What to do with a settled frame, given what it needs (`frame`, from
/// [`Shadow::frame_conversion`]) and how the shadow holding it renders.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// The shadow renders this frame's conversion: capture it.
    Capture(Conversion),
    /// No verdict yet (no frame, VO still configuring): skip the job.
    NotYet,
    /// The frame needs the other renderer: replace the shadow, skip the job.
    Respawn(Conversion),
    /// No correct thumbnail can be made of this stream.
    Refuse(String),
}

/// The pure half of [`checked_capture`].
fn step_for(frame: Result<Option<Conversion>, String>, render: Render) -> Step {
    match frame {
        Err(reason) => Step::Refuse(reason),
        Ok(None) => Step::NotYet,
        Ok(Some(conversion)) if conversion.render() == render => Step::Capture(conversion),
        Ok(Some(conversion)) => Step::Respawn(conversion),
    }
}

/// How a shadow's seek ended ([`Shadow::seek_settled`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settle {
    Settled,
    /// The seek was ACCEPTED (the file is open) but did not settle within the
    /// budget, and the demuxer is starved or holds no cached range with the
    /// target ([`unsettled`]): a region not downloaded yet, or a dead shadow.
    /// mpv does not abandon that read for a later seek, so this shadow is
    /// dropped.
    TimedOut,
    /// The seek was accepted and did not settle within the budget, but the
    /// bytes are in hand ([`unsettled`]): the decoder or the VO is still at work
    /// (a gpu-next shadow configuring its VO under GPU load). The shadow is KEPT
    /// and the job skipped: dropping it would throw that work away, block the
    /// worker while its mpv tears down, and make the replacement pay the whole
    /// open again.
    Busy,
    /// The seek was accepted and did not settle within the budget, and mpv's
    /// state proves neither starvation nor that the target is cached (the
    /// property unreadable, malformed, or without the fields that decide, as
    /// an older mpv reports it). Neither verdict is evidenced, so the shadow is
    /// kept like `Busy`, under the same cumulative deadline.
    Unknown,
    /// The file was not open yet when the budget ran out (the seek stayed
    /// refused the whole time): a slow first open of a valid file, still
    /// progressing. The shadow is KEPT and given another budget, never dropped
    /// and respawned (which would restart the open from the dll load each
    /// time, and never finish for a file whose open exceeds one budget).
    Opening,
    /// Abandoned for a hover request.
    Yielded,
}

/// THE decision on a seek to `target` (seconds) that mpv accepted and that did
/// not settle in its budget, from mpv's `demuxer-cache-state` (JSON) at that
/// moment:
/// - `TimedOut` (drop the shadow) when the demuxer is starved (`underrun`), or
///   when its cached ranges (`seekable-ranges`, the spans mpv can seek inside
///   without reading) are known and none holds the target: the read may be
///   blocked on bytes that never come (a backward seek before the cache, a
///   gap, a region not downloaded, a state still describing the old position).
/// - `Busy` (keep it) when it is not starved and a cached range holds the
///   target: the bytes are there, only the decode or the VO is left. A
///   keyframe seek lands on the keyframe at or before the target, and mpv
///   seeks inside the cache only when the target lies in a range, so "the
///   target inside a range" is the membership test, not `cache-end`.
/// - `Unknown` when the state proves neither (unreadable, malformed, or
///   missing `underrun` / `seekable-ranges`, as an older mpv reports it).
///
/// Measured 2026-11-22 in the running shell under GPU load: every not-settled
/// DV seek had `vo-configured=no`, `underrun: false` and the target inside
/// `[0, 12.554]`; the one starved state was a first open with 0 bytes read
/// (`underrun: true`). [`checked_capture`] keeps a `Busy` or `Unknown` shadow.
fn unsettled(demuxer_cache_state: Option<&str>, target: f64) -> Settle {
    let Some(state) = demuxer_cache_state.and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()) else {
        return Settle::Unknown;
    };
    let starved = state.get("underrun").and_then(serde_json::Value::as_bool);
    // Every range must read as two numbers; one that does not makes the
    // coverage unknown rather than quietly skipped.
    let ranges: Option<Vec<(f64, f64)>> = state.get("seekable-ranges").and_then(|ranges| {
        ranges
            .as_array()?
            .iter()
            .map(|range| Some((range.get("start")?.as_f64()?, range.get("end")?.as_f64()?)))
            .collect()
    });
    let covered = ranges.map(|ranges| ranges.iter().any(|&(start, end)| start <= target && target <= end));
    match (starved, covered) {
        (Some(true), _) | (_, Some(false)) => Settle::TimedOut,
        (Some(false), Some(true)) => Settle::Busy,
        (None, _) | (_, None) => Settle::Unknown,
    }
}

/// How many retired values may wait behind the one being torn down. A shadow
/// is a whole mpv with a decoder and, for libplacebo, a GPU context: letting
/// teardowns pile up would hold their memory and VRAM on a machine whose GPU
/// is shared. The worker holds one shadow at a time and retires it when it
/// replaces it, so one in teardown plus one waiting covers a replacement whose
/// predecessor is still stuck; a further retirement waits for room
/// (backpressure), which is the old synchronous behaviour as the worst case.
const RETIREMENTS_QUEUED: usize = 1;

/// Tears values down on a thread of its own, so a caller never waits for a
/// slow drop: a shadow's mpv tearing down waits for its blocked read to be
/// cancelled and, under GPU load, for its VO (measured 2026-11-22: hover jobs
/// of 46 s spent in a dropped shadow's teardown). A `Shadow` drops there whole,
/// in its own field order (its mpv before the hidden window that mpv renders
/// into), exactly as it would inline.
struct Retirer {
    queue: std::sync::mpsc::SyncSender<Box<dyn Send>>,
}

impl Retirer {
    fn new() -> Self {
        let (queue, retired) = std::sync::mpsc::sync_channel::<Box<dyn Send>>(RETIREMENTS_QUEUED);
        std::thread::Builder::new()
            .name("thumbs-retire".into())
            .spawn(move || {
                for value in retired {
                    drop(value);
                }
            })
            .expect("thumbs: the retirement thread could not start");
        Self { queue }
    }

    /// Hand `value` over for teardown; waits only while the queue is full.
    fn retire<T: Send + 'static>(&self, value: T) {
        if let Err(std::sync::mpsc::SendError(value)) = self.queue.send(Box::new(value)) {
            // The thread is gone (a teardown panicked): tear down here, loudly.
            tracing::error!("thumbs: the retirement thread is gone; tearing a shadow down inline");
            drop(value);
        }
    }
}

/// Retire a shadow off the calling thread ([`Retirer`]).
fn retire(shadow: Shadow) {
    static RETIRER: std::sync::OnceLock<Retirer> = std::sync::OnceLock::new();
    RETIRER.get_or_init(Retirer::new).retire(shadow);
}

/// The warning for a shadow that asked for hardware decoder `asked` while mpv
/// decodes with `current` (`hwdec-current`, `None` when mpv reports none):
/// `Some` the first time they differ, then never again for this shadow
/// (`reported` latches only once a fall-back has been said, so one that
/// happens after hardware-decoded captures is still caught). Nothing when
/// software decode was asked for.
fn decoder_fallback(asked: &str, current: Option<&str>, reported: &std::cell::Cell<bool>) -> Option<String> {
    if asked == "no" || reported.get() || current == Some(asked) {
        return None;
    }
    reported.set(true);
    Some(format!(
        "thumbs: asked for hwdec={asked} but mpv decodes with {:?}; previews stay correct \
         (software decode) but slower",
        current.unwrap_or("nothing"),
    ))
}

/// What one job showed of a shadow's progress, for its busy allowance
/// ([`kept_overdue`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Progress {
    /// The seek was `Busy` or `Unknown`: the shadow is kept (starts the clock).
    Kept,
    /// The job ended without a frame for some other reason: a settled seek
    /// with no capturable frame yet (`Step::NotYet`), or a capture that ran
    /// and failed. Checks a running clock, never starts or resets one.
    NoFrame,
    /// A frame was captured: the clock stops.
    Frame,
}

/// Whether a shadow's busy allowance has run out after a job that showed
/// `progress` at `now`. `kept` is when the shadow was first kept by a `Busy`
/// / `Unknown` seek without a frame since; only a delivered frame clears it,
/// so neither later seeks nor jobs that end with no frame escape
/// [`BUSY_DEADLINE_MS`].
fn kept_overdue(kept: &std::cell::Cell<Option<Instant>>, progress: Progress, now: Instant) -> bool {
    let limit = Duration::from_millis(BUSY_DEADLINE_MS);
    match progress {
        Progress::Frame => {
            kept.set(None);
            false
        }
        Progress::Kept => {
            let since = *kept.get().get_or_insert(now);
            kept.set(Some(since));
            now.saturating_duration_since(since) >= limit
        }
        Progress::NoFrame => kept.get().is_some_and(|since| now.saturating_duration_since(since) >= limit),
    }
}

/// Whether what a capture closure returned is a delivered frame (so the
/// shadow's busy allowance resets), or a capture that ran and failed.
trait Delivered {
    fn delivered(&self) -> bool;
}

impl<T, E> Delivered for Result<T, E> {
    fn delivered(&self) -> bool {
        self.is_ok()
    }
}

impl<T> Delivered for Option<T> {
    fn delivered(&self) -> bool {
        self.is_some()
    }
}

/// A closure that only wants the seek (a bench, the sweep's open wait).
impl Delivered for () {
    fn delivered(&self) -> bool {
        true
    }
}

/// How a shadow's load ended in error ([`Shadow::check_load`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoadEnd {
    /// mpv could not read the stream: unreachable, or not being served yet
    /// (a torrent still being added). A new shadow may well succeed later.
    Lost(String),
    /// mpv read the stream and still could not show it: no video to play, or
    /// a renderer that cannot start. A new shadow would end the same way.
    Never(String),
}

/// THE decision on a load mpv ended in error, from mpv's error code. Only
/// `MPV_ERROR_LOADING_FAILED` says the bytes could not be had; every other
/// code is mpv judging what it did read (measured: a gpu-next that cannot
/// start ends a load as "no audio or video data played", not as loading
/// failed).
fn load_end(error: std::ffi::c_int, error_text: &str, render: Render, mpv_said: &str) -> LoadEnd {
    let what = match render {
        Render::Software => "the shadow could not show the stream",
        Render::Libplacebo => "gpu-next could not show the stream",
    };
    let reason = format!("thumbs: {what} ({error_text}); mpv said: {mpv_said}");
    if error == mpv::MPV_ERROR_LOADING_FAILED {
        LoadEnd::Lost(reason)
    } else {
        LoadEnd::Never(reason)
    }
}

/// Whether a stream's frames now need something other than they did when
/// last captured (`last`, `None` before the first capture).
fn conversion_changed(last: Option<&Conversion>, now: &Conversion) -> bool {
    last.is_some_and(|last| last != now)
}

/// A shadow after [`checked_capture`], with what it captured.
enum Captured<T> {
    /// The frame at the target, captured by a shadow that renders what it
    /// needs, and what it needs.
    Frame(Shadow, Conversion, T),
    /// Nothing captured (the seek did not settle, no verdict yet): the job is
    /// skipped and a later one asks again.
    Skipped(Shadow),
    /// The frame needs the other renderer: here is a new shadow with it (same
    /// url and width), the frame's conversion, and nothing captured. The next
    /// job seeks again, on this shadow.
    Replaced(Shadow, Conversion),
    /// No correct thumbnail can be made of this stream, and why. The caller
    /// stops previews for it and logs the reason once.
    Refused(String),
    /// The shadow is gone (mpv could not read the stream NOW: unreachable,
    /// not served yet), and why. Nothing is wrong with the stream's frames:
    /// the caller drops the shadow and a later request opens a new one.
    /// `lived` is how long that shadow lasted (it paces the next one).
    Lost { reason: String, lived: Duration },
    /// A hover request arrived while this (sweep) seek was waiting: nothing
    /// captured, the shadow unchanged, the same target is sampled again later.
    Yielded(Shadow),
    /// The seek did not settle in its budget: the shadow is parked on bytes
    /// the swarm has not delivered, and was dropped (see [`checked_capture`]).
    /// Nothing captured; the next job opens a new shadow.
    Parked,
}

impl<T> Captured<T> {
    /// The shadow this outcome still holds, if any (to [`retire`] it when the
    /// outcome is discarded).
    fn into_shadow(self) -> Option<Shadow> {
        match self {
            Captured::Frame(shadow, ..)
            | Captured::Skipped(shadow)
            | Captured::Replaced(shadow, _)
            | Captured::Yielded(shadow) => Some(shadow),
            Captured::Refused(_) | Captured::Lost { .. } | Captured::Parked => None,
        }
    }
}

/// THE one path to a capture, hover thumbnail or sweep sample alike: seek to
/// `target`, decide from what mpv reports for the frame landed on, and run
/// `capture` only when this shadow renders that conversion. Decided per frame,
/// not once per stream: a stream can change colour along the way (an SDR
/// segment spliced before an HDR or Dolby Vision one), and a name that must
/// be refused can appear anywhere. `yield_now` is asked while the seek waits:
/// true abandons it (a sweep sample giving way to a hover).
fn checked_capture<T: Delivered>(
    shadow: Shadow,
    url: &str,
    target: f64,
    yield_now: &dyn Fn() -> bool,
    capture: impl FnOnce(&Shadow) -> T,
) -> Captured<T> {
    // A shadow whose busy allowance ran out ([`kept_overdue`]) is retired as
    // lost: paced by its lifetime, never respawned at once.
    let overdue = |shadow: Shadow, why: &str| -> Captured<T> {
        let lived = shadow.born.elapsed();
        retire(shadow);
        Captured::Lost {
            reason: format!(
                "thumbs: no frame within {BUSY_DEADLINE_MS} ms of first being kept ({why}); retiring the shadow"
            ),
            lived,
        }
    };
    let settled = shadow.seek_settled(target, yield_now);
    // mpv's own verdict on the load comes first, settled or not: a load mpv
    // ended in error never settles a seek, and must not time out in silence.
    // Every exit that ends this shadow retires it ([`retire`]): its teardown
    // can take tens of seconds and must not hold up the next hover.
    match shadow.check_load() {
        Ok(()) => {}
        Err(LoadEnd::Lost(reason)) => {
            let lived = shadow.born.elapsed();
            retire(shadow);
            return Captured::Lost { reason, lived };
        }
        Err(LoadEnd::Never(reason)) => {
            retire(shadow);
            return Captured::Refused(reason);
        }
    }
    match settled {
        Settle::Settled => {}
        Settle::Opening => {
            // Still opening the file. Keep a shadow that is progressing toward
            // open (valid, just slow) across seek budgets rather than dropping
            // and respawning it (which restarts the open and never finishes for
            // an open longer than one budget). But a stream that NEVER opens and
            // never errors (a dead/hung source; mpv's own error is a 600 s
            // network-timeout away) must not be held for the whole time the
            // user keeps hovering, so a cumulative opening deadline from spawn
            // retires it as unreadable (paced like any lost shadow).
            let lived = shadow.born.elapsed();
            if lived >= Duration::from_millis(OPENING_DEADLINE_MS) {
                retire(shadow);
                return Captured::Lost {
                    reason: format!(
                        "thumbs: the stream did not open within {OPENING_DEADLINE_MS} ms; retiring the shadow"
                    ),
                    lived,
                };
            }
            return Captured::Skipped(shadow);
        }
        Settle::TimedOut => {
            // A seek that did not settle left the shadow's stream thread
            // blocked in a read of bytes not downloaded yet, and mpv does not
            // abandon that read for a later seek: every following job, even
            // into a region long downloaded, waited behind it (measured
            // 2026-10-28: one position failed 7 times in a row right after a
            // hover far ahead). Dropping the shadow cancels the read; the next
            // job starts clean. Tearing an mpv down waits for that cancel, so
            // the teardown is retired, never waited for here.
            retire(shadow);
            return Captured::Parked;
        }
        Settle::Yielded => return Captured::Yielded(shadow),
        // The bytes are in hand; the frame is still being decoded or shown.
        // Kept, and the next job asks again on this same shadow.
        Settle::Busy | Settle::Unknown => {
            if kept_overdue(&shadow.kept_since, Progress::Kept, Instant::now()) {
                return overdue(shadow, &format!("{settled:?} seek"));
            }
            return Captured::Skipped(shadow);
        }
    }
    match step_for(shadow.frame_conversion(), shadow.render) {
        Step::Capture(conversion) => {
            shadow.note_decoder();
            let captured = capture(&shadow);
            // Only a delivered frame is progress; a capture that ran and
            // failed leaves a running clock running.
            let progress = if captured.delivered() { Progress::Frame } else { Progress::NoFrame };
            if kept_overdue(&shadow.kept_since, progress, Instant::now()) {
                return overdue(shadow, "the capture failed");
            }
            Captured::Frame(shadow, conversion, captured)
        }
        Step::NotYet => {
            if kept_overdue(&shadow.kept_since, Progress::NoFrame, Instant::now()) {
                return overdue(shadow, "settled with no frame to capture");
            }
            Captured::Skipped(shadow)
        }
        Step::Refuse(reason) => {
            retire(shadow);
            Captured::Refused(reason)
        }
        Step::Respawn(conversion) => {
            let (width, render) = (shadow.width, conversion.render());
            // The old shadow is retired before the new one opens; its teardown
            // runs on the retirement thread (bounded, [`RETIREMENTS_QUEUED`])
            // instead of holding up the replacement.
            retire(shadow);
            match Shadow::spawn(url, width, render) {
                Ok(replacement) => Captured::Replaced(replacement, conversion),
                Err(e) => Captured::Refused(format!(
                    "thumbs: {conversion} needs the {render:?} renderer, which could not \
                     start: {e}"
                )),
            }
        }
    }
}

/// The shadow's filter chain for an output `width`: scale down to it, keeping
/// the aspect ratio on an even height. Never up: a source narrower than the
/// request is captured as it is. Spelled as a raw libavfilter graph because
/// mpv's own `scale=` shorthand rejects the `min()` expression outright.
///
/// The scaler stays libswscale's default (bicubic), on measurement: against a
/// reference downscale of the full frame at the drawn size, `flags=lanczos`
/// scored slightly lower (PSNR 42.6 vs 42.9 dB) and cost ~10 ms more per
/// capture, and mpv's `sws-scaler` option does not reach this graph at all
/// (byte-identical screenshots).
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

/// The decoder a shadow uses, decided by its renderer (never by the file).
///
/// `d3d11va-copy` for the libplacebo shadow: the GPU decodes and the frame is
/// copied back to RAM (p010) BEFORE the software scale, so the chain, the
/// output size and the picture are exactly the software decode's. Measured
/// 2026-11-22 (bench_hwdec, interleaved per time point under the same load):
/// bit-identical screenshots (mean-RGB diff 0.000, same sizes) on DV p5, DV
/// p8.1, HDR10 and HLG, at 0.40-0.43x the software time per capture for 4K DV
/// (debug and release alike: the decode is libmpv's, not ours) and ~0.8x for
/// 1080p HDR; open-to-first-frame medians no slower (the hardware decoder's
/// ~250 ms init is paid back by the faster first 4K decode). Native `d3d11va`
/// is NOT usable here: its frames stay D3D11 surfaces, which the software
/// scale cannot touch, so mpv drops the filter and shows the full 4K frame
/// (and FFmpeg's `scale_d3d11` fails to configure on them in the shipped
/// libmpv).
///
/// `no` for the software shadow: with `vo=null` there is no GPU interop (mpv
/// silently decodes in software anyway for `d3d11va`), and `d3d11va-copy`
/// there was no faster for SDR (1.02x) and moved colour (nv12 path, 0.42
/// mean-RGB), so SDR keeps the decode it always had.
///
/// If mpv cannot start the hardware decoder it decodes in software, which is
/// the colour reference above: only slower, never wrong. [`Shadow::note_decoder`]
/// says so loudly.
fn hwdec_for(render: Render) -> &'static str {
    match render {
        Render::Software => "no",
        Render::Libplacebo => "d3d11va-copy",
    }
}

impl Shadow {
    /// A muted mpv over `url` whose decode chain outputs `width`, rendering its
    /// screenshots as `render` says, decoding as [`hwdec_for`] says. Options
    /// follow Harbor's shadow flags; `hr-seek=no` + keyframe seeks are the
    /// speed. [`checked_capture`] confirms each frame wants this `render`
    /// before it is captured.
    fn spawn(url: &str, width: u32, render: Render) -> Result<Self, String> {
        Self::spawn_decoding(url, width, render, hwdec_for(render))
    }

    /// [`Shadow::spawn`] with the decoder named (the benches compare them).
    fn spawn_decoding(url: &str, width: u32, render: Render, hwdec: &'static str) -> Result<Self, String> {
        let window = match render {
            Render::Software => None,
            Render::Libplacebo => Some(HiddenWindow::create()?),
        };
        let mpv = Mpv::load(&mpv::default_dll_path())?;
        match &window {
            None => {
                // Optional like the rest below: a build without the null VO
                // fails at the first screenshot, loudly enough.
                if let Err(e) = mpv.set_option("vo", "null") {
                    tracing::debug!("thumbs: option vo=null: {e}");
                }
            }
            Some(window) => {
                // Every one of these decides whether the screenshot is the
                // libplacebo render of an SDR target, so none is optional: no
                // fallback VO (a software screenshot is exactly the wrong
                // picture), the hidden window to render into, the screenshot
                // taken from the VO, and untagged, which makes its target SDR.
                let wid = window.wid().to_string();
                for (name, value) in [
                    ("vo", "gpu-next"),
                    ("wid", wid.as_str()),
                    ("force-window", "no"),
                    ("screenshot-sw", "no"),
                    ("screenshot-tag-colorspace", "no"),
                ] {
                    mpv.set_option(name, value).map_err(|e| {
                        format!("thumbs: libplacebo screenshots need {name}={value}: {e}")
                    })?;
                }
                // The player's own tone mapping; best-effort exactly as the
                // player applies it (shell.rs).
                for (name, value) in crate::shell::TONE_MAPPING_OPTIONS {
                    if let Err(e) = mpv.set_option(name, value) {
                        tracing::warn!("thumbs: option {name}={value} not applied: {e}");
                    }
                }
            }
        }
        for (name, value) in [
            ("aid", "no"),
            ("sid", "no"),
            ("pause", "yes"),
            ("keep-open", "yes"),
            ("idle", "yes"),
            ("config", "no"),
            ("load-scripts", "no"),
            ("ytdl", "no"),
            ("osc", "no"),
            ("hwdec", hwdec),
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
        // Benches only: options a bench arm varies (read path, cache), applied
        // over the ones above. Loud, since a bench must not measure a typo.
        #[cfg(test)]
        tests::BENCH_EXTRA_OPTIONS.with(|extra| {
            for (name, value) in extra.borrow().iter() {
                mpv.set_option(name, value)
                    .unwrap_or_else(|e| panic!("bench option {name}={value}: {e}"));
            }
        });
        // The one option that is not optional: without the scale every
        // screenshot is a full-resolution frame (megabytes per hover).
        mpv.set_option("vf", &scale_filter(width))
            .map_err(|e| format!("thumbs: scaling to {width}px failed: {e}"))?;
        // mpv's own error lines, so a lost or refused shadow can say why
        // ([`Shadow::check_load`]). Diagnostics only.
        if let Err(e) = mpv.request_log_messages("error") {
            tracing::debug!("thumbs: mpv error log unavailable: {e}");
        }
        mpv.initialize()?;
        mpv.command(&["loadfile", url])?;
        Ok(Self {
            born: Instant::now(),
            mpv,
            width,
            render,
            hwdec,
            fallback_reported: std::cell::Cell::new(false),
            kept_since: std::cell::Cell::new(None),
            mpv_errors: std::cell::RefCell::new(Vec::new()),
            _window: window,
        })
    }

    /// On every captured frame: say loudly, once per shadow, when mpv is not
    /// (or no longer) decoding with the hardware decoder asked for
    /// ([`decoder_fallback`]). The frame is still right (mpv's fallback is the
    /// software decode, the colour reference of [`hwdec_for`]); only its time
    /// is the software one.
    fn note_decoder(&self) {
        if self.hwdec == "no" || self.fallback_reported.get() {
            return;
        }
        let current = self.mpv.get_property_string("hwdec-current");
        if let Some(warning) = decoder_fallback(self.hwdec, current.as_deref(), &self.fallback_reported) {
            tracing::warn!("{warning}");
        }
    }

    /// What the frame mpv holds right now needs ([`conversion_for`] over what
    /// mpv reports for it), read without waiting: callers ask once a seek has
    /// settled, when the frame and its parameters are already there.
    /// `Ok(None)` while there is no decoded frame yet, or (libplacebo) while
    /// the VO is still configuring: slow, not broken (seen with the player
    /// decoding 4K and the subtitle model loading at once), so the job is
    /// skipped and the next one asks again. `Err` when the frame cannot be
    /// converted. (How the LOAD ended is [`Shadow::check_load`]'s to say, and
    /// [`checked_capture`] asks it first.)
    fn frame_conversion(&self) -> Result<Option<Conversion>, String> {
        let profile = self.mpv.get_property_string("current-tracks/video/dolby-vision-profile");
        let Some(params) = (match self.mpv.get_property_string("video-params") {
            Some(json) => VideoParams::parse(&json, profile.as_deref())?,
            None => None,
        }) else {
            return Ok(None);
        };
        let renders = match self.render {
            Render::Software => true,
            Render::Libplacebo => {
                self.mpv.get_property_string("vo-configured").as_deref() == Some("yes")
            }
        };
        if renders {
            conversion_for(&params).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Drain the shadow's events (nothing else reads them) and say how its
    /// load ended, if it did: `Ok` while it is alive. A load mpv ended in error
    /// is final for THIS shadow either way (an idle mpv never settles a seek,
    /// so without this every job would time out in silence, for the rest of
    /// the video: measured 2026-10-28 on a shadow whose stream could not be
    /// reached); [`load_end`] decides whether a new shadow may do better.
    fn check_load(&self) -> Result<(), LoadEnd> {
        loop {
            match self.mpv.wait_event(0.0) {
                mpv::MpvEvent::Empty => return Ok(()),
                mpv::MpvEvent::LogMessage { prefix, text, .. } => {
                    let mut said = self.mpv_errors.borrow_mut();
                    if said.len() == MPV_ERRORS_KEPT {
                        said.remove(0);
                    }
                    said.push(format!("[{prefix}] {text}"));
                }
                mpv::MpvEvent::EndFile { reason, error, .. } if reason == mpv::MPV_END_FILE_REASON_ERROR => {
                    return Err(load_end(
                        error,
                        &self.mpv.error_string(error),
                        self.render,
                        &self.mpv_errors.borrow().join("; "),
                    ));
                }
                _ => {}
            }
        }
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

    /// Keyframe-seek to `target` and wait for the seek to settle. `TimedOut`
    /// (undownloaded torrent region / dead stream), `Busy` (bytes in hand, the
    /// frame not shown yet: [`unsettled`] tells the two apart) and `Yielded`
    /// (`yield_now` said a hover is waiting) mean do not capture. mpv refuses a seek until
    /// the file is open, which is every first job on a fresh shadow (and every
    /// first job after a renderer change): the seek is asked again within the
    /// same budget instead of failing at once.
    fn seek_settled(&self, target: f64, yield_now: &dyn Fn() -> bool) -> Settle {
        let deadline = Instant::now() + Duration::from_millis(SEEK_SETTLE_MS);
        // mpv refuses a seek until the file is open. Staying here to the budget
        // means the file is STILL OPENING (a fresh shadow, a slow first decode)
        // - progress, not a stuck read - so it returns `Opening` (keep the
        // shadow), distinct from a seek that WAS accepted and then did not
        // settle (`TimedOut`, below: a stuck/undownloaded read, drop it).
        while self.mpv.command(&["seek", &format!("{target:.3}"), "absolute+keyframes"]).is_err() {
            // No retry starts after the budget: the sleep is capped to what
            // is left, and the deadline is checked again before the next ask.
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Settle::Opening;
            }
            std::thread::sleep(SETTLE_POLL.min(left));
            if yield_now() {
                return Settle::Yielded;
            }
            if Instant::now() >= deadline {
                return Settle::Opening;
            }
        }
        while Instant::now() < deadline {
            std::thread::sleep(SETTLE_POLL);
            match self.mpv.get_property_string("seeking").as_deref() {
                Some("no") => {
                    // A settled seek with no frame decoded yet reports no
                    // time-pos; require one so the screenshot has a frame.
                    if self.mpv.get_property_string("time-pos").is_some() {
                        return Settle::Settled;
                    }
                }
                // "yes" = still seeking; None = property gone (file replaced).
                Some(_) | None => {}
            }
            if yield_now() {
                return Settle::Yielded;
            }
        }
        let cache_state = self.mpv.get_property_string("demuxer-cache-state");
        let verdict = unsettled(cache_state.as_deref(), target);
        tracing::debug!(
            "thumbs: seek to {target:.1}s not settled in {SEEK_SETTLE_MS} ms ({verdict:?}): vo-configured={:?} \
             demuxer-cache-state={}",
            self.mpv.get_property_string("vo-configured"),
            cache_state.as_deref().unwrap_or("-"),
        );
        verdict
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

/// The window a [`Render::Libplacebo`] shadow's gpu-next renders into: created
/// hidden and never shown, owned by a thread of its own that pumps its
/// messages (mpv makes its output window a child of this one, and a parent
/// whose thread never reads its queue stalls the child's thread). Dropping it
/// signals `stop`, which ends the thread, which destroys the window; drop the
/// mpv that rendered into it first.
///
/// The stop signal is an event, not a message: posting to the window can fail
/// (a full queue among other things), and a shutdown that depends on queue
/// space would leave a live thread holding the window. The pump waits on the
/// event and its message queue together (`MsgWaitForMultipleObjects`).
#[cfg(windows)]
struct HiddenWindow {
    /// The HWND as mpv's `wid` takes it (an HWND is not `Send`; this is).
    wid: isize,
    /// The manual-reset event that tells the pump to finish (a HANDLE, held
    /// as `isize` for the same reason as `wid`).
    stop: isize,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The hidden window's size in pixels. gpu-next sizes its swapchain to it, and
/// that swapchain is never presented: a `video` screenshot renders at the
/// frame's own (already scaled) size whatever the window is. The smallest
/// size, so the swapchain costs next to nothing.
#[cfg(windows)]
const HIDDEN_WINDOW_PX: i32 = 1;

#[cfg(windows)]
impl HiddenWindow {
    fn create() -> Result<Self, String> {
        use windows::core::w;
        use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, MsgWaitForMultipleObjects,
            PeekMessageW, TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW, WS_POPUP,
        };

        // Manual reset: once set it stays set, so the pump cannot miss it.
        let stop = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|e| format!("thumbs: hidden window stop event: {e}"))?;
        let stop_raw = stop.0 as isize;
        let (sender, receiver) = std::sync::mpsc::channel::<Result<isize, String>>();
        let spawned = std::thread::Builder::new()
            .name("thumbs-window".into())
            .spawn(move || {
                let stop = HANDLE(stop_raw as *mut _);
                // The system STATIC class: no class to register, and its
                // window procedure is all a never-shown parent needs. No
                // WS_VISIBLE, and nothing ever shows it.
                let created = unsafe {
                    CreateWindowExW(
                        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                        w!("STATIC"),
                        w!("rillio-thumbs"),
                        WS_POPUP,
                        0,
                        0,
                        HIDDEN_WINDOW_PX,
                        HIDDEN_WINDOW_PX,
                        None,
                        None,
                        None,
                        None,
                    )
                };
                let hwnd = match created {
                    Ok(hwnd) => hwnd,
                    Err(e) => {
                        let _ = sender.send(Err(format!("thumbs: hidden window: {e}")));
                        return;
                    }
                };
                if sender.send(Ok(hwnd.0 as isize)).is_err() {
                    let _ = unsafe { DestroyWindow(hwnd) };
                    return;
                }
                let mut msg = MSG::default();
                'pump: loop {
                    // Everything queued first: the wait below only wakes for
                    // input that arrives after the queue was last looked at.
                    // `stop` is checked per message, so a queue something
                    // keeps refilling cannot hold the drop's join.
                    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                        unsafe {
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                        if unsafe { WaitForSingleObject(stop, 0) } == WAIT_OBJECT_0 {
                            break 'pump;
                        }
                    }
                    let woke = unsafe {
                        MsgWaitForMultipleObjects(Some(&[stop]), false, INFINITE, QS_ALLINPUT)
                    };
                    if woke == WAIT_OBJECT_0 {
                        // `stop` was set: the shadow is going.
                        break;
                    }
                    if woke.0 != WAIT_OBJECT_0.0 + 1 {
                        // Not the event, not input: the wait itself failed and
                        // this thread can no longer serve the window. Said
                        // loudly; the shadow's screenshots fail from here on.
                        tracing::error!(
                            "thumbs: hidden window wait failed ({:#x}): {}",
                            woke.0,
                            windows::core::Error::from_win32()
                        );
                        break;
                    }
                }
                let _ = unsafe { DestroyWindow(hwnd) };
            });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(e) => {
                let _ = unsafe { CloseHandle(stop) };
                return Err(format!("thumbs: hidden window thread: {e}"));
            }
        };
        match receiver.recv() {
            Ok(Ok(wid)) => Ok(Self { wid, stop: stop_raw, thread: Some(thread) }),
            // The thread has already ended (no window): nothing to stop.
            Ok(Err(e)) => {
                let _ = thread.join();
                let _ = unsafe { CloseHandle(stop) };
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                let _ = unsafe { CloseHandle(stop) };
                Err("thumbs: the hidden window thread died".to_string())
            }
        }
    }

    fn wid(&self) -> isize {
        self.wid
    }
}

#[cfg(windows)]
impl Drop for HiddenWindow {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::System::Threading::SetEvent;

        let stop = HANDLE(self.stop as *mut _);
        // SetEvent needs no queue space; it fails only on a handle that is
        // not a live event, and this one is owned here and closed only below.
        // Should it fail anyway, joining would wait forever on a thread that
        // was never told to stop: say so and leave the thread (and the
        // handle it still waits on) rather than hang the caller.
        match unsafe { SetEvent(stop) } {
            Ok(()) => {
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
                let _ = unsafe { CloseHandle(stop) };
            }
            Err(e) => tracing::error!(
                "thumbs: hidden window {:#x} could not be told to stop, its thread is left \
                 running: {e}",
                self.wid
            ),
        }
    }
}

/// No hidden-window renderer off Windows: a stream that needs libplacebo is
/// refused there (no preview, logged) rather than shown wrong.
#[cfg(not(windows))]
struct HiddenWindow;

#[cfg(not(windows))]
impl HiddenWindow {
    fn create() -> Result<Self, String> {
        Err("thumbs: libplacebo screenshots need a hidden window, which only the Windows shell \
             provides"
            .into())
    }

    fn wid(&self) -> isize {
        unreachable!("HiddenWindow::create never succeeds off Windows")
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

/// The url the SHADOW should open for what the player is playing, and with it
/// the stream's ONE identity in [`Inner`]: two callers naming the same stream
/// two ways would each switch the single shadow to their spelling, and every
/// switch drops the shadow, the cache and any capture in flight.
///
/// The web layer hands over `video.state.stream.url`, built by the core on the
/// SYMBOLIC server address ([`rillio_streaming_server::config::default_base_url`]).
/// On a boot whose server fell back to another port (11470 refused: held by
/// another process, or inside a reserved range) nothing listens there, so a
/// url on that origin maps to the port the server really bound. The byte-plane
/// form `rillio://<ih>/<idx>` maps to `<base_url>/<ih>/<idx>` the same way.
/// Anything else is returned untouched for [`validate_url`] to judge.
pub(crate) fn resolve_shadow_url(app: &tauri::AppHandle, url: &str) -> Result<String, String> {
    use tauri::Manager;

    let base_url = app.state::<crate::ServerHandle>().get().map(|s| s.base_url.clone());
    let symbolic = rillio_streaming_server::config::default_base_url();
    shadow_url_for(url, symbolic.as_str(), base_url.as_deref())
}

/// The pure half of [`resolve_shadow_url`]: `symbolic` is the address stream
/// urls are built on, `base_url` whatever the server bound (`None` while it is
/// still binding).
fn shadow_url_for(url: &str, symbolic: &str, base_url: Option<&str>) -> Result<String, String> {
    let trimmed = url.trim();
    let bound = || base_url.ok_or_else(|| "thumbs: the streaming server has not bound yet".to_string());
    if trimmed.to_ascii_lowercase().starts_with("rillio://") {
        let (info_hash, file_idx) = crate::stream_cb::parse_url(trimmed)
            .map_err(|e| format!("thumbs: BLOCKED malformed rillio:// url {url}: {e}"))?;
        return Ok(format!("{}/{info_hash}/{file_idx}", bound()?.trim_end_matches('/')));
    }
    let symbolic = symbolic.trim_end_matches('/');
    match trimmed.strip_prefix(symbolic) {
        // Same origin only: `/path`, `?query` or nothing may follow, never
        // more port digits or host (`:114700`, `.evil.com`).
        Some(rest) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') => {
            Ok(format!("{}{rest}", bound()?.trim_end_matches('/')))
        }
        _ => Ok(url.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IH: &str = "0123456789abcdef0123456789abcdef01234567";
    const BASE: &str = "http://127.0.0.1:49312/";
    const SYMBOLIC: &str = "http://127.0.0.1:11470/";

    /// The shadow must open the HTTP form of whatever the player loaded, on the
    /// port the server really got - never `rillio://`, which is registered on
    /// the PLAYER's mpv instance and not on this one.
    #[test]
    fn shadow_maps_the_byte_plane_back_to_http() {
        assert_eq!(
            shadow_url_for(&format!("rillio://{IH}/3"), SYMBOLIC, Some(BASE)),
            Ok(format!("http://127.0.0.1:49312/{IH}/3"))
        );
        // No trailing slash on the base, same answer (no doubled separator).
        assert_eq!(
            shadow_url_for(&format!("rillio://{IH}/0"), SYMBOLIC, Some("http://127.0.0.1:11470")),
            Ok(format!("http://127.0.0.1:11470/{IH}/0"))
        );
        // The mapped url is exactly what validate_url accepts.
        let mapped = shadow_url_for(&format!("rillio://{IH}/1"), SYMBOLIC, Some(BASE)).unwrap();
        validate_url(&mapped).unwrap();
    }

    /// The seek bar and the scene sweep name the stream two ways on a boot
    /// whose server fell back off 11470 (measured 2026-10-28: the sweep asked
    /// for `127.0.0.1:11470/...`, the seek bar for the bound port, and each ask
    /// switched the one shadow to its spelling; 1 hover in 8 got a frame).
    /// Both spellings must come out as ONE url, on the port really bound.
    #[test]
    fn the_symbolic_origin_maps_to_the_bound_port() {
        let symbolic = format!("http://127.0.0.1:11470/{IH}/1?tr=dht%3A{IH}");
        let bound = format!("http://127.0.0.1:49312/{IH}/1?tr=dht%3A{IH}");
        assert_eq!(shadow_url_for(&symbolic, SYMBOLIC, Some(BASE)), Ok(bound.clone()));
        assert_eq!(shadow_url_for(&bound, SYMBOLIC, Some(BASE)), Ok(bound.clone()));
        // Bare origin and query-only forms keep their shape.
        assert_eq!(
            shadow_url_for("http://127.0.0.1:11470", SYMBOLIC, Some(BASE)),
            Ok("http://127.0.0.1:49312".to_owned())
        );
        // A longer port or another host on the same prefix is not this origin.
        for other in [format!("http://127.0.0.1:114700/{IH}/1"), "http://127.0.0.1:11470.evil.com/x".to_owned()] {
            assert_eq!(shadow_url_for(&other, SYMBOLIC, Some(BASE)), Ok(other.clone()), "{other}");
        }
        // Before the server has bound, a symbolic url cannot be placed: loud.
        assert!(shadow_url_for(&symbolic, SYMBOLIC, None).is_err());
    }

    /// Everything else passes through untouched for validate_url to judge.
    #[test]
    fn shadow_leaves_other_urls_alone() {
        for url in [
            "https://cdn.example/a.mkv".to_owned(),
            "av://lavfi:testsrc".to_owned(),
        ] {
            assert_eq!(shadow_url_for(&url, SYMBOLIC, Some(BASE)), Ok(url.clone()), "{url:?}");
        }
        // ...and the pass-through still gets rejected by the scheme rule.
        assert!(validate_url(&shadow_url_for("av://lavfi:testsrc", SYMBOLIC, Some(BASE)).unwrap()).is_err());
    }

    /// A malformed byte-plane url is refused here too (it never becomes a
    /// plausible-looking http url), and an unbound server fails loud.
    #[test]
    fn shadow_refuses_what_it_cannot_map() {
        assert!(shadow_url_for(&format!("rillio://{IH}/0/movie.mkv"), SYMBOLIC, Some(BASE)).is_err());
        assert!(shadow_url_for(&format!("rillio://{IH}/-1"), SYMBOLIC, Some(BASE)).is_err());
        assert!(shadow_url_for("rillio://evil.com/0", SYMBOLIC, Some(BASE)).is_err());
        assert!(shadow_url_for(&format!("rillio://{IH}/0"), SYMBOLIC, None).is_err());
    }

    /// A full cache overwrites the frame unused the longest, where "used" is a
    /// read as much as a write; a frame that is still being looked at survives.
    #[test]
    fn cache_overwrites_the_least_recently_used() {
        let mut cache = ThumbCache::with_max(3);
        for bucket in [10, 11, 12] {
            cache.insert(bucket, format!("frame {bucket}"));
        }
        // Under the cap nothing is dropped.
        assert!([10, 11, 12].iter().all(|b| cache.contains(*b)));
        // 10 is the oldest write, but reading it makes 11 the stalest.
        assert_eq!(cache.get(10).map(String::as_str), Some("frame 10"));
        cache.insert(13, "frame 13".into());
        assert!(!cache.contains(11), "the least recently used frame is overwritten");
        assert!([10, 12, 13].iter().all(|b| cache.contains(*b)));
        // ...and so on, one out for each one in: 12 is next.
        cache.insert(14, "frame 14".into());
        assert!(!cache.contains(12));
        assert_eq!(cache.frames.len(), 3);
        // Rewriting a bucket that is already held evicts nothing.
        cache.insert(10, "frame 10 again".into());
        assert_eq!(cache.frames.len(), 3);
        assert_eq!(cache.get(10).map(String::as_str), Some("frame 10 again"));
        assert!([13, 14].iter().all(|b| cache.contains(*b)));
        // A miss is a miss, and does not disturb the order: 13 goes next.
        assert_eq!(cache.get(99), None);
        cache.insert(15, "frame 15".into());
        assert!(!cache.contains(13));
        cache.clear();
        assert!(cache.frames.is_empty());
    }

    // `video-params` exactly as the shipped libmpv (libmpv-20260610) reported
    // it for each sample on 2026-10-25, with the track's
    // `dolby-vision-profile` beside it. The samples: 45 s copied out of a
    // web release (profile 5, profile 8.1), and encodes made from the 8.1 one
    // through a raw pipe so no side data could leak (HDR10 with and without
    // MaxCLL/mastering metadata, HLG, SDR BT.709, SDR BT.601).
    const DV_P5: &str = r#"{"pixelformat":"yuv420p10","average-bpp":24,"w":3840,"h":1920,"dw":3840,"dh":1920,"crop-x":0,"crop-y":0,"crop-w":3840,"crop-h":1920,"aspect":2.000000,"aspect-name":"Univisium","par":1.000000,"sar":2.000000,"sar-name":"Univisium","colormatrix":"dolbyvision","colorlevels":"limited","primaries":"bt.2020","gamma":"pq","sig-peak":4.929080,"light":"display","chroma-location":"mpeg2/4/h264","stereo-in":"mono","rotate":0,"alpha":"none","min-luma":0.000104,"max-luma":1000.603271,"max-cll":0.000000,"max-fall":0.000000,"max-pq-y":0.704762,"avg-pq-y":0.330647}"#;
    const DV_P81: &str = r#"{"pixelformat":"yuv420p10","average-bpp":24,"w":3840,"h":1920,"dw":3840,"dh":1920,"crop-x":0,"crop-y":0,"crop-w":3840,"crop-h":1920,"aspect":2.000000,"aspect-name":"Univisium","par":1.000000,"sar":2.000000,"sar-name":"Univisium","colormatrix":"dolbyvision","colorlevels":"limited","primaries":"bt.2020","gamma":"pq","sig-peak":4.929080,"light":"display","chroma-location":"uhd","stereo-in":"mono","rotate":0,"alpha":"none","min-luma":0.000104,"max-luma":1000.603271,"max-cll":808.000000,"max-fall":138.000000,"scene-max-r":154.399994,"scene-max-g":139.100006,"scene-max-b":128.800003,"scene-avg":2.500000,"max-pq-y":0.520391,"avg-pq-y":0.300122,"prim-red-x":0.680000,"prim-red-y":0.320000,"prim-green-x":0.265000,"prim-green-y":0.690000,"prim-blue-x":0.150000,"prim-blue-y":0.060000,"prim-white-x":0.312700,"prim-white-y":0.329000}"#;
    const HDR10_LIGHT_LEVELS: &str = r#"{"pixelformat":"yuv420p10","average-bpp":24,"w":1920,"h":960,"dw":1920,"dh":960,"crop-x":0,"crop-y":0,"crop-w":1920,"crop-h":960,"aspect":2.000000,"aspect-name":"Univisium","par":1.000000,"sar":2.000000,"sar-name":"Univisium","colormatrix":"bt.2020-ncl","colorlevels":"limited","primaries":"bt.2020","gamma":"pq","sig-peak":4.926108,"light":"display","chroma-location":"mpeg2/4/h264","stereo-in":"mono","rotate":0,"alpha":"none","min-luma":0.005000,"max-luma":1000.000000,"max-cll":808.000000,"max-fall":138.000000,"prim-red-x":0.680000,"prim-red-y":0.320000,"prim-green-x":0.265000,"prim-green-y":0.690000,"prim-blue-x":0.150000,"prim-blue-y":0.060000,"prim-white-x":0.312700,"prim-white-y":0.329000}"#;
    const HDR10_BARE: &str = r#"{"pixelformat":"yuv420p10","average-bpp":24,"w":1920,"h":960,"dw":1920,"dh":960,"crop-x":0,"crop-y":0,"crop-w":1920,"crop-h":960,"aspect":2.000000,"aspect-name":"Univisium","par":1.000000,"sar":2.000000,"sar-name":"Univisium","colormatrix":"bt.2020-ncl","colorlevels":"limited","primaries":"bt.2020","gamma":"pq","sig-peak":49.261086,"light":"display","chroma-location":"mpeg2/4/h264","stereo-in":"mono","rotate":0,"alpha":"none","min-luma":0.000001,"max-luma":10000.000000,"max-cll":0.000000,"max-fall":0.000000}"#;
    const HLG: &str = r#"{"pixelformat":"yuv420p10","average-bpp":24,"w":1920,"h":960,"dw":1920,"dh":960,"crop-x":0,"crop-y":0,"crop-w":1920,"crop-h":960,"aspect":2.000000,"aspect-name":"Univisium","par":1.000000,"sar":2.000000,"sar-name":"Univisium","colormatrix":"bt.2020-ncl","colorlevels":"limited","primaries":"bt.2020","gamma":"hlg","sig-peak":4.926108,"light":"hlg","chroma-location":"mpeg2/4/h264","stereo-in":"mono","rotate":0,"alpha":"none","min-luma":0.000001,"max-luma":1000.000000,"max-cll":0.000000,"max-fall":0.000000}"#;
    const SDR_BT709: &str = r#"{"pixelformat":"yuv420p","average-bpp":12,"w":1920,"h":960,"dw":1920,"dh":960,"crop-x":0,"crop-y":0,"crop-w":1920,"crop-h":960,"aspect":2.000000,"aspect-name":"Univisium","par":1.000000,"sar":2.000000,"sar-name":"Univisium","colormatrix":"bt.709","colorlevels":"limited","primaries":"bt.709","gamma":"bt.1886","sig-peak":0.000000,"light":"display","chroma-location":"mpeg2/4/h264","stereo-in":"mono","rotate":0,"alpha":"none"}"#;
    const SDR_BT601: &str = r#"{"pixelformat":"yuv420p","average-bpp":12,"w":720,"h":360,"dw":720,"dh":360,"crop-x":0,"crop-y":0,"crop-w":720,"crop-h":360,"aspect":2.000000,"aspect-name":"Univisium","par":1.000000,"sar":2.000000,"sar-name":"Univisium","colormatrix":"bt.601","colorlevels":"limited","primaries":"bt.601-525","gamma":"bt.1886","sig-peak":0.000000,"light":"display","chroma-location":"mpeg2/4/h264","stereo-in":"mono","rotate":0,"alpha":"none"}"#;

    fn decide(video_params: &str, profile: Option<&str>) -> Result<Conversion, String> {
        let params = VideoParams::parse(video_params, profile)?
            .ok_or_else(|| format!("no colour reported yet: {video_params}"))?;
        conversion_for(&params)
    }

    /// Before the first frame mpv reports only the container's size (seen on
    /// the profile 5 sample): not ready, which is neither a decision nor a
    /// refusal.
    #[test]
    fn video_params_before_the_first_frame_are_not_ready() {
        assert_eq!(VideoParams::parse(r#"{"w":3840,"h":1920}"#, Some("5")), Ok(None));
        assert!(VideoParams::parse(DV_P5, Some("5")).unwrap().is_some());
    }

    /// A reported shape with some fields replaced: for the shapes no sample
    /// on this machine has (profile 7, an HLG base layer, ICtCp, ...).
    fn with(video_params: &str, fields: &[(&str, &str)]) -> String {
        let mut json: serde_json::Value = serde_json::from_str(video_params).unwrap();
        for (name, value) in fields {
            json[*name] = serde_json::Value::String((*value).to_owned());
        }
        json.to_string()
    }

    /// The bug (owner, 2026-10-25: "everything looks purple/green"): every
    /// stream was screenshot in software. Dolby Vision, whatever the profile,
    /// must reach libplacebo, which applies the RPU; the profile only names it.
    #[test]
    fn dolby_vision_is_rendered_by_libplacebo() {
        let dv = |profile| Conversion::Libplacebo(Need::DolbyVision { profile });
        // Profile 5: IPTPQc2, no picture at all without the RPU.
        assert_eq!(decide(DV_P5, Some("5")), Ok(dv(Some(5))));
        // Profile 8.1: the RPU rides an HDR10 base layer; mpv still reports
        // the frames as dolbyvision, and the RPU (not just the PQ base) wins.
        assert_eq!(decide(DV_P81, Some("8")), Ok(dv(Some(8))));
        // Profile 7 (UHD Blu-ray, base + enhancement layer): mpv decodes the
        // base layer with the RPU, the same shape as 8.1.
        assert_eq!(decide(DV_P81, Some("7")), Ok(dv(Some(7))));
        // Profile 8.4: the RPU on an HLG base layer.
        assert_eq!(decide(&with(DV_P81, &[("gamma", "hlg")]), Some("8")), Ok(dv(Some(8))));
        // The frames decide, not the track: no profile reported still means
        // Dolby Vision frames...
        assert_eq!(decide(DV_P5, None), Ok(dv(None)));
        // ...and a DV-flagged track whose frames arrive as plain HDR10 (no RPU
        // on them) is tone mapped as the HDR10 it is.
        assert_eq!(
            decide(HDR10_LIGHT_LEVELS, Some("8")),
            Ok(Conversion::Libplacebo(Need::Hdr { transfer: "pq".into() }))
        );
        // Profile 7 without the RPU is its HDR10 base layer too.
        assert_eq!(
            decide(HDR10_LIGHT_LEVELS, Some("7")),
            Ok(Conversion::Libplacebo(Need::Hdr { transfer: "pq".into() }))
        );
        for (params, profile) in [(DV_P5, "5"), (DV_P81, "8")] {
            assert_eq!(decide(params, Some(profile)).unwrap().render(), Render::Libplacebo);
        }
    }

    /// Profile 5 has no HDR10 base layer: frames that arrive without their
    /// RPU (stripped, lost) are refused, never tone mapped as if they were
    /// HDR10, whatever tags they carry.
    #[test]
    fn profile_5_without_its_rpu_is_refused() {
        for params in [HDR10_BARE, HDR10_LIGHT_LEVELS, SDR_BT709] {
            let err = decide(params, Some("5")).unwrap_err();
            assert!(err.contains("profile 5") && err.contains("without their RPU"), "{err}");
            assert!(err.contains("no HDR10 base layer"), "{err}");
        }
        // With the RPU on the frames it is the Dolby Vision route as ever.
        assert!(decide(DV_P5, Some("5")).is_ok());
    }

    /// HDR10 and HLG are tone mapped by libplacebo; light-level metadata
    /// (present or absent) changes nothing about the route, only what
    /// libplacebo does along it, exactly as in playback.
    #[test]
    fn hdr_is_tone_mapped_with_or_without_light_levels() {
        let pq = Ok(Conversion::Libplacebo(Need::Hdr { transfer: "pq".into() }));
        assert_eq!(decide(HDR10_LIGHT_LEVELS, None), pq);
        assert_eq!(decide(HDR10_BARE, None), pq);
        assert_eq!(decide(HLG, None), Ok(Conversion::Libplacebo(Need::Hdr { transfer: "hlg".into() })));
        // PQ on BT.709 primaries is still HDR: the curve outranks the gamut.
        assert_eq!(decide(&with(HDR10_BARE, &[("primaries", "bt.709")]), None), pq);
    }

    /// SDR BT.709-class video keeps the plain software screenshot it always
    /// had; only a gamut or colour system it cannot show moves it.
    #[test]
    fn sdr_stays_plain_unless_it_cannot_be() {
        assert_eq!(decide(SDR_BT709, None), Ok(Conversion::Plain));
        assert_eq!(decide(SDR_BT601, None), Ok(Conversion::Plain));
        assert_eq!(decide(SDR_BT709, None).unwrap().render(), Render::Software);
        assert_eq!(
            decide(&with(SDR_BT709, &[("colormatrix", "bt.2020-ncl"), ("primaries", "bt.2020")]), None),
            Ok(Conversion::Libplacebo(Need::WideGamut { primaries: "bt.2020".into() }))
        );
        assert_eq!(
            decide(&with(SDR_BT709, &[("colormatrix", "bt.2020-cl")]), None),
            Ok(Conversion::Libplacebo(Need::ColourSystem { colormatrix: "bt.2020-cl".into() }))
        );
        // ICtCp PQ is HDR first: the transfer names it.
        assert_eq!(
            decide(&with(HDR10_BARE, &[("colormatrix", "bt.2100-pq")]), None),
            Ok(Conversion::Libplacebo(Need::Hdr { transfer: "pq".into() }))
        );
    }

    /// Every name the decision knows is on exactly one side of each table, so
    /// no name is both "fine in software" and "needs libplacebo".
    #[test]
    fn the_name_tables_do_not_overlap() {
        for (a, b) in [
            (SOFTWARE_MATRICES, LIBPLACEBO_MATRICES),
            (SDR_TRANSFERS, HDR_TRANSFERS),
            (NARROW_PRIMARIES, WIDE_PRIMARIES),
        ] {
            for name in a {
                assert!(!b.contains(name), "{name} is on both sides");
            }
        }
    }

    /// No known conversion: refused with the reason, never guessed as SDR
    /// (a name a later mpv adds would otherwise come out purple or grey).
    /// Malformed reports are refused too.
    #[test]
    fn what_cannot_be_converted_is_refused_loudly() {
        for (field, value) in [
            ("colormatrix", "ipt-c2-of-the-future"),
            ("gamma", "auto"),
            ("primaries", "unknown"),
        ] {
            let err = decide(&with(SDR_BT709, &[(field, value)]), None).unwrap_err();
            assert!(err.contains(value), "{err}");
        }
        assert!(decide("not json", None).is_err());
        assert!(decide(r#"{"colormatrix":"bt.709","primaries":"bt.709"}"#, None)
            .unwrap_err()
            .contains("gamma"));
        assert!(decide(DV_P5, Some("five")).is_err());
    }

    /// A refused stream gets no preview: nothing queued, no worker, and it
    /// stays that way until the stream changes, when the new one is judged
    /// afresh.
    #[test]
    fn a_refused_stream_gets_no_previews() {
        let url = "http://127.0.0.1:11470/dv-profile-5".to_owned();
        let now = Instant::now();
        let mut inner = Inner::default();
        assert_eq!(
            inner.request_thumb(url.clone(), 7, 448, None, now),
            ThumbRequest::Pending { start_worker: true }
        );
        // The worker refuses (e.g. libplacebo could not start), then exits.
        let reason = "thumbs: Dolby Vision profile 5 needs libplacebo, which could not start";
        inner.refuse(reason);
        inner.worker_running = false;
        assert!(inner.refused.is_some() && inner.wanted.is_none() && inner.shadow.is_none());
        for bucket in [7, 8, 9] {
            let answer = inner.request_thumb(url.clone(), bucket, 448, None, now);
            assert_eq!(answer, ThumbRequest::Refused(reason.to_owned()));
            // ...and the web is told so, with the reason, to stop asking.
            assert_eq!(ThumbAnswer::from(answer), ThumbAnswer::Never { reason: reason.to_owned() });
        }
        assert!(inner.wanted.is_none() && !inner.worker_running);
        // Another stream is judged on its own.
        assert_eq!(
            inner.request_thumb("http://127.0.0.1:11470/sdr".to_owned(), 7, 448, None, now),
            ThumbRequest::Pending { start_worker: true }
        );
        assert!(inner.refused.is_none());
    }

    /// A shadow that could not READ the stream (measured 2026-10-28: opened
    /// while the torrent was not being served, its load ended in "loading
    /// failed" and every later job timed out for the rest of the video) is no
    /// verdict on the stream: previews stay on and a later request opens a new
    /// shadow, never faster than the lost one took to fail.
    #[test]
    fn a_lost_shadow_is_replaced_not_a_refusal() {
        let url = "http://127.0.0.1:49312/cold-torrent".to_owned();
        let t0 = Instant::now();
        let mut inner = Inner::default();
        assert_eq!(inner.request_thumb(url.clone(), 7, 448, None, t0), ThumbRequest::Pending { start_worker: true });
        // The worker's job comes back lost after the shadow lived 3 s.
        inner.wanted = None;
        let lived = Duration::from_secs(3);
        let back = inner.take_back::<()>(Captured::Lost { reason: "thumbs: loading failed".into(), lived }, t0);
        assert!(matches!(back, TakenBack::Lost));
        inner.worker_running = false;
        assert!(inner.refused.is_none(), "a lost shadow refuses nothing");
        assert!(inner.shadow.is_none());
        // Asked again before the lost shadow's lifetime has passed: still
        // pending (the web keeps asking), no new shadow yet.
        let early = t0 + lived / 2;
        let answer = inner.request_thumb(url.clone(), 7, 448, None, early);
        assert_eq!(answer, ThumbRequest::Pending { start_worker: false });
        assert_eq!(ThumbAnswer::from(answer), ThumbAnswer::Pending);
        assert_eq!(inner.wanted, Some(7), "the request stays queued");
        // Once it has passed, the next request starts a worker (a new shadow).
        assert_eq!(
            inner.request_thumb(url.clone(), 7, 448, None, t0 + lived),
            ThumbRequest::Pending { start_worker: true }
        );
        // A shadow that failed at once may be replaced at once.
        let _ = inner.take_back::<()>(Captured::Lost { reason: "x".into(), lived: Duration::ZERO }, t0);
        assert!(inner.may_respawn(t0));
    }

    /// Only mpv's "loading failed" means the bytes could not be had; anything
    /// else mpv says after reading the stream (no video, a renderer that
    /// cannot start) would end every new shadow the same way.
    #[test]
    fn a_load_that_could_not_read_is_retried_one_that_read_is_final() {
        assert!(matches!(
            load_end(mpv::MPV_ERROR_LOADING_FAILED, "loading failed", Render::Software, ""),
            LoadEnd::Lost(_)
        ));
        assert!(matches!(
            load_end(mpv::MPV_ERROR_LOADING_FAILED, "loading failed", Render::Libplacebo, ""),
            LoadEnd::Lost(_)
        ));
        // MPV_ERROR_NOTHING_TO_PLAY (-16): what a gpu-next that cannot start
        // ends a load with ("no audio or video data played").
        match load_end(-16, "no audio or video data played", Render::Libplacebo, "[vo] failed") {
            LoadEnd::Never(reason) => assert!(reason.contains("gpu-next could not show the stream"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    /// Every settled frame is judged on its own: captured only by a shadow
    /// that renders what it needs, a frame needing the other renderer forces
    /// a new shadow (whichever way: SDR after HDR goes back to software), a
    /// refusal anywhere refuses, and no verdict skips.
    #[test]
    fn each_frame_is_routed_by_what_it_needs() {
        let plain = || decide(SDR_BT709, None);
        let hdr = || decide(HDR10_BARE, None);
        let dv = || decide(DV_P5, Some("5"));
        assert_eq!(step_for(plain().map(Some), Render::Software), Step::Capture(plain().unwrap()));
        assert_eq!(step_for(hdr().map(Some), Render::Libplacebo), Step::Capture(hdr().unwrap()));
        assert_eq!(step_for(dv().map(Some), Render::Libplacebo), Step::Capture(dv().unwrap()));
        // An HDR segment reached on the software shadow, and back.
        assert_eq!(step_for(hdr().map(Some), Render::Software), Step::Respawn(hdr().unwrap()));
        assert_eq!(step_for(plain().map(Some), Render::Libplacebo), Step::Respawn(plain().unwrap()));
        // A profile 5 segment that lost its RPU, met mid-stream.
        let refused = decide(HDR10_BARE, Some("5"));
        assert!(matches!(step_for(refused.map(Some), Render::Libplacebo), Step::Refuse(_)));
        assert_eq!(step_for(Ok(None), Render::Software), Step::NotYet);
    }

    /// A stream whose frames change what they need drops every preview
    /// captured under the old conversion and the sweep's comparison sample;
    /// the same conversion again drops nothing; the sweep's progress survives
    /// a change (restarting it would never finish on an alternating file).
    #[test]
    fn a_change_of_conversion_drops_what_was_captured_under_the_old() {
        let plain = decide(SDR_BT709, None).unwrap();
        let hdr = decide(HDR10_BARE, None).unwrap();
        let dv = decide(DV_P81, Some("8")).unwrap();
        assert!(!conversion_changed(None, &plain));
        assert!(!conversion_changed(Some(&plain), &plain));
        assert!(conversion_changed(Some(&plain), &hdr));
        // Same renderer, different need: still a change.
        assert!(conversion_changed(Some(&hdr), &dv));

        let mut inner = Inner::default();
        inner.switch_stream(Some("http://127.0.0.1:11470/spliced".to_owned()), None);
        let mut scan = Scan::new(600.0);
        scan.next = 7;
        scan.prev_luma = Some(vec![1; 144]);
        inner.scan = Some(scan);
        inner.note_conversion(plain.clone());
        inner.cache.insert(1, "sdr frame".into());
        inner.note_conversion(plain.clone());
        assert!(inner.cache.contains(1), "the same conversion keeps the cache");
        inner.note_conversion(hdr.clone());
        assert!(!inner.cache.contains(1), "nothing captured as SDR survives the change");
        let scan = inner.scan.as_ref().unwrap();
        assert!(scan.prev_luma.is_none(), "no cut is claimed across the change");
        assert_eq!(scan.next, 7, "the sweep keeps its place");
        assert_eq!(inner.conversion, Some(hdr));
        // A new stream starts with no conversion at all.
        inner.switch_stream(None, None);
        assert_eq!(inner.conversion, None);
    }

    /// A seek that did not settle keeps its shadow only when the demuxer is
    /// not starved AND one of its cached ranges holds the target; a state that
    /// says neither is Unknown (kept, under the cumulative deadline). The
    /// recorded states are mpv's own, from the running shell 2026-11-22
    /// (trimmed to the fields that decide).
    #[test]
    fn only_a_target_inside_a_cached_range_keeps_the_shadow() {
        // A gpu-next shadow still configuring its VO after a seek to 4 s:
        // the target inside the cached range [0, 12.554]. Kept.
        let vo_configuring = r#"{"cache-end":14.515,"reader-pts":3.087,"underrun":false,"idle":true,
            "fw-bytes":33580160,"seekable-ranges":[{"start":0.0,"end":12.554}]}"#;
        assert_eq!(unsettled(Some(vo_configuring), 4.0), Settle::Busy);
        // Past the end of what it holds: dropped.
        assert_eq!(unsettled(Some(vo_configuring), 20.0), Settle::TimedOut);
        // A backward seek before a cached range [100, 110]: cache-end is far
        // past the target, yet the target is not cached. Dropped.
        let later_range = r#"{"cache-end":110.0,"underrun":false,"idle":true,
            "seekable-ranges":[{"start":100.0,"end":110.0}]}"#;
        assert_eq!(unsettled(Some(later_range), 10.0), Settle::TimedOut);
        // Disjoint ranges: inside one is kept, in the gap between is dropped.
        let disjoint = r#"{"cache-end":60.0,"underrun":false,"idle":true,
            "seekable-ranges":[{"start":0.0,"end":10.0},{"start":50.0,"end":60.0}]}"#;
        assert_eq!(unsettled(Some(disjoint), 55.0), Settle::Busy);
        assert_eq!(unsettled(Some(disjoint), 30.0), Settle::TimedOut);
        // A stale state read while the accepted seek has not reached the
        // demuxer yet: it still describes the old position, not the target.
        let stale = r#"{"cache-end":0.501,"reader-pts":0.0,"underrun":false,"idle":false,
            "seekable-ranges":[{"start":0.0,"end":0.501}]}"#;
        assert_eq!(unsettled(Some(stale), 4.0), Settle::TimedOut);
        // Waiting for bytes with none buffered: the stuck read.
        let starved = r#"{"underrun":true,"idle":false,"fw-bytes":0,"seekable-ranges":[]}"#;
        assert_eq!(unsettled(Some(starved), 0.0), Settle::TimedOut);
        // Starved even though an old range holds the target: starved wins.
        let starved_in_range = r#"{"underrun":true,"seekable-ranges":[{"start":0.0,"end":10.0}]}"#;
        assert_eq!(unsettled(Some(starved_in_range), 4.0), Settle::TimedOut);
        // Not starved, no ranges cached at all: the target is not here.
        let empty = r#"{"underrun":false,"idle":false,"seekable-ranges":[]}"#;
        assert_eq!(unsettled(Some(empty), 4.0), Settle::TimedOut);
        // States that prove neither: Unknown, not a verdict either way.
        assert_eq!(unsettled(None, 0.0), Settle::Unknown, "property unreadable");
        assert_eq!(unsettled(Some("not json"), 0.0), Settle::Unknown, "malformed");
        assert_eq!(unsettled(Some(r#"{"underrun":false}"#), 4.0), Settle::Unknown, "older mpv: no ranges");
        assert_eq!(
            unsettled(Some(r#"{"underrun":false,"seekable-ranges":[{"start":"x","end":1}]}"#), 4.0),
            Settle::Unknown,
            "malformed range"
        );
        assert_eq!(
            unsettled(Some(r#"{"seekable-ranges":[{"start":0.0,"end":10.0}]}"#), 4.0),
            Settle::Unknown,
            "no underrun flag"
        );
    }

    /// A shadow kept because its seek is Busy or Unknown is kept for at most
    /// BUSY_DEADLINE_MS from the FIRST such verdict: later accepted seeks do
    /// not restart the clock, only a captured frame does. On expiry it is
    /// retired as lost, and the loss paces the next shadow by the retired one's
    /// lifetime, so a retirement cannot turn into a rapid respawn loop.
    #[test]
    fn a_kept_shadow_is_retired_after_the_busy_deadline_and_paced() {
        let deadline = Duration::from_millis(BUSY_DEADLINE_MS);
        let t0 = Instant::now();
        let kept = std::cell::Cell::new(None);
        let just_before = deadline - Duration::from_millis(1);
        // The first verdict starts the clock; repeated verdicts (each a newly
        // accepted seek) do not restart it.
        assert!(!kept_overdue(&kept, Progress::Kept, t0));
        assert!(!kept_overdue(&kept, Progress::Kept, t0 + deadline / 2));
        assert!(!kept_overdue(&kept, Progress::Kept, t0 + just_before));
        assert!(kept_overdue(&kept, Progress::Kept, t0 + deadline), "kept past the cumulative deadline");
        // A delivered frame is progress: the clock stops.
        assert!(!kept_overdue(&kept, Progress::Frame, t0 + deadline));
        assert_eq!(kept.get(), None);

        // Busy, then seeks that settle on no capturable frame (NotYet) again
        // and again: the running clock still applies.
        let kept = std::cell::Cell::new(None);
        assert!(!kept_overdue(&kept, Progress::Kept, t0));
        assert!(!kept_overdue(&kept, Progress::NoFrame, t0 + deadline / 2));
        assert!(!kept_overdue(&kept, Progress::NoFrame, t0 + just_before));
        assert!(kept_overdue(&kept, Progress::NoFrame, t0 + deadline), "NotYet escaped the busy deadline");
        // Busy, then a capture that runs but fails: no reset, still bounded.
        let kept = std::cell::Cell::new(None);
        assert!(!kept_overdue(&kept, Progress::Kept, t0));
        assert!(!kept_overdue(&kept, Progress::NoFrame, t0 + just_before));
        assert!(kept.get().is_some(), "a failed capture reset the allowance");
        assert!(kept_overdue(&kept, Progress::NoFrame, t0 + deadline), "a failed capture escaped the busy deadline");
        // Busy, then a capture that succeeds: cleared; a later no-frame job
        // has no clock to check.
        let kept = std::cell::Cell::new(None);
        assert!(!kept_overdue(&kept, Progress::Kept, t0));
        assert!(!kept_overdue(&kept, Progress::Frame, t0 + just_before));
        assert!(!kept_overdue(&kept, Progress::NoFrame, t0 + deadline * 3));
        // A no-frame job on a shadow never kept starts no clock.
        let fresh = std::cell::Cell::new(None);
        assert!(!kept_overdue(&fresh, Progress::NoFrame, t0));
        assert_eq!(fresh.get(), None);
        // What a capture closure returns says whether it delivered a frame.
        assert!(Ok::<_, String>(()).delivered() && !Err::<(), _>("x").delivered());
        assert!(Some(1).delivered() && !None::<u8>.delivered() && ().delivered());

        // The retirement is a loss paced by the shadow's own lifetime (at least
        // one seek budget plus the deadline): no new shadow before that passes.
        let lived = Duration::from_millis(SEEK_SETTLE_MS) + deadline;
        let mut inner = Inner::default();
        inner.switch_stream(Some("http://127.0.0.1:1/x.mkv".into()), None);
        inner.lose_shadow("thumbs: no frame within the busy deadline", lived, t0);
        assert!(!inner.may_respawn(t0 + lived - Duration::from_millis(1)));
        assert!(inner.may_respawn(t0 + lived));
    }

    /// Retiring hands the teardown to the retirement thread: the caller does
    /// not wait for a slow drop (a shadow's mpv tearing down can take tens of
    /// seconds), and at most one retirement queues behind the one in progress
    /// (a third waits for room), so stalled teardowns cannot pile up.
    #[test]
    fn retiring_does_not_wait_for_the_teardown_and_is_bounded() {
        struct SlowDrop(Duration, Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for SlowDrop {
            fn drop(&mut self) {
                std::thread::sleep(self.0);
                self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        const TEARDOWN: Duration = Duration::from_millis(600);
        let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retirer = Retirer::new();
        // The first is taken by the thread at once, the second waits in the
        // queue: neither caller waits for a teardown.
        let t = Instant::now();
        retirer.retire(SlowDrop(TEARDOWN, dropped.clone()));
        std::thread::sleep(Duration::from_millis(50)); // the thread takes it
        retirer.retire(SlowDrop(TEARDOWN, dropped.clone()));
        assert!(t.elapsed() < TEARDOWN / 2, "a retirement waited for a teardown: {:?}", t.elapsed());
        // A third finds the queue full: it waits until the first is done.
        let t = Instant::now();
        retirer.retire(SlowDrop(TEARDOWN, dropped.clone()));
        assert!(t.elapsed() >= TEARDOWN / 2, "outstanding retirements are not bounded: {:?}", t.elapsed());
        // All of them are torn down in the end.
        let until = Instant::now() + TEARDOWN * 6;
        while dropped.load(std::sync::atomic::Ordering::SeqCst) < 3 {
            assert!(Instant::now() < until, "a retired value was never dropped");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A fall-back from the hardware decoder to software is reported whenever
    /// it happens, also after captures that did decode in hardware, and once
    /// per shadow. A shadow that asked for software decode reports nothing.
    #[test]
    fn a_decoder_fallback_is_reported_once_whenever_it_happens() {
        let reported = std::cell::Cell::new(false);
        // Hardware decode as asked: nothing to say, and it keeps watching.
        assert_eq!(decoder_fallback("d3d11va-copy", Some("d3d11va-copy"), &reported), None);
        assert_eq!(decoder_fallback("d3d11va-copy", Some("d3d11va-copy"), &reported), None);
        // A later fall-back to software is reported...
        let said = decoder_fallback("d3d11va-copy", Some("no"), &reported).expect("a fallback is reported");
        assert!(said.contains("d3d11va-copy") && said.contains("\"no\""), "{said}");
        // ...once.
        assert_eq!(decoder_fallback("d3d11va-copy", Some("no"), &reported), None);
        assert_eq!(decoder_fallback("d3d11va-copy", None, &reported), None);
        // mpv reporting no decoder at all counts as a fall-back too.
        assert!(decoder_fallback("d3d11va-copy", None, &std::cell::Cell::new(false)).is_some());
        // Software decode asked for: never a fall-back.
        assert_eq!(decoder_fallback("no", Some("no"), &std::cell::Cell::new(false)), None);
    }

    /// The renderer picks the decoder: GPU decode copied back before the
    /// software scale for libplacebo (bit-identical to software decode,
    /// bench_hwdec), never native d3d11va (its surfaces skip the scale:
    /// full-size screenshots); software decode for the SDR software shadow.
    #[test]
    fn the_renderer_chooses_the_decoder() {
        assert_eq!(hwdec_for(Render::Libplacebo), "d3d11va-copy");
        assert_eq!(hwdec_for(Render::Software), "no");
    }

    /// The player's own video-params pick the first renderer, so an HDR or
    /// Dolby Vision stream opens libplacebo at once instead of starting
    /// software and respawning on the first frame (the double open). The same
    /// verdict `conversion_for` reaches per frame, moved earlier.
    #[test]
    fn the_players_params_choose_the_first_renderer() {
        for params in [DV_P5, DV_P81, HDR10_BARE, HDR10_LIGHT_LEVELS, HLG] {
            assert_eq!(initial_render(Some(params), None), Some(Render::Libplacebo), "{params}");
        }
        for params in [SDR_BT709, SDR_BT601] {
            assert_eq!(initial_render(Some(params), None), Some(Render::Software), "{params}");
        }
        // No frame yet (only the container size), or no player up: no hint, and
        // the software default starts exactly as before.
        assert_eq!(initial_render(Some(r#"{"w":3840,"h":1920}"#), None), None);
        assert_eq!(initial_render(None, None), None);
    }

    /// The hint belongs to the stream: set when the stream is taken up, filled
    /// in if it was not known then, never churned afterwards, and reset by a
    /// new stream.
    #[test]
    fn the_render_hint_follows_the_stream() {
        let now = Instant::now();
        let mut inner = Inner::default();
        // The first request came before the player reported a frame: no hint.
        inner.request_thumb("http://x/1".into(), 0, 448, None, now);
        assert_eq!(inner.hint_render, None);
        // A later hover, the player's params now ready: fills the not-yet-open shadow's render.
        inner.request_thumb("http://x/1".into(), 1, 448, Some(Render::Libplacebo), now);
        assert_eq!(inner.hint_render, Some(Render::Libplacebo));
        // A still-later, different hint does not overwrite it (a shadow may be open on it).
        inner.request_thumb("http://x/1".into(), 2, 448, Some(Render::Software), now);
        assert_eq!(inner.hint_render, Some(Render::Libplacebo));
        // A new stream takes its own hint.
        inner.request_thumb("http://y/2".into(), 0, 448, Some(Render::Software), now);
        assert_eq!(inner.hint_render, Some(Render::Software));
    }

    /// A refusal met at capture time refuses the stream, as one found at open.
    #[test]
    fn a_refusal_at_capture_time_stops_the_stream() {
        let mut inner = Inner::default();
        inner.switch_stream(Some("http://127.0.0.1:11470/spliced".to_owned()), None);
        inner.wanted = Some(3);
        let back = inner.take_back::<()>(Captured::Refused("thumbs: profile 5 without RPU".into()), Instant::now());
        assert!(matches!(back, TakenBack::Refused));
        assert!(inner.refused.is_some() && inner.wanted.is_none());
    }

    /// A stream that changes colour mid-file, through the real shadow: the
    /// SDR half is captured in software, the first HDR frame forces the
    /// libplacebo renderer (nothing captured), the next ask captures it there,
    /// and going back to the SDR half returns to software. Built for this by
    /// concatenating an SDR and an HDR10 HEVC encode (see the report).
    /// Needs `libmpv-2.dll` and:
    ///   RILLIO_THUMBS_MIXED_FILE  the spliced file (SDR first half, HDR second)
    ///   RILLIO_THUMBS_BENCH_OUT   optional directory to keep the jpgs in
    /// Run: `cargo test --lib thumbs::tests::bench_mixed -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs libmpv-2.dll and RILLIO_THUMBS_MIXED_FILE"]
    fn bench_mixed_stream() {
        let file = std::env::var("RILLIO_THUMBS_MIXED_FILE").expect("RILLIO_THUMBS_MIXED_FILE");
        let out = std::env::var("RILLIO_THUMBS_BENCH_OUT").ok().map(std::path::PathBuf::from);
        const SDR_AT: f64 = 0.25;
        const HDR_AT: f64 = 0.75;
        let mut inner = Inner::default();
        inner.switch_stream(Some(file.clone()), None);
        let (shadow, first) = open_settled(&file, 448);
        assert_eq!(first, Conversion::Plain, "the file starts SDR");
        let duration = shadow.duration().expect("a duration");
        let mut shadow = Some(shadow);
        for (step, at, expect_render) in [
            ("sdr", SDR_AT, Render::Software),
            ("hdr", HDR_AT, Render::Libplacebo),
            ("sdr-again", SDR_AT, Render::Software),
        ] {
            let time = duration * at;
            let mut replaced = false;
            let jpeg = loop {
                let (width, render) = shadow.as_ref().map(|s| (s.width, s.render)).unwrap();
                match checked_capture(shadow.take().unwrap(), &file, time, &|| false, Shadow::capture_bytes) {
                    Captured::Parked => {
                        shadow = Some(Shadow::spawn(&file, width, render).expect("shadow respawn"));
                    }
                    Captured::Frame(s, conversion, jpeg) => {
                        assert_eq!(s.render, expect_render, "{step}");
                        println!("{step} at {time:.1}s: {conversion} on {:?}, replaced first: {replaced}", s.render);
                        inner.note_conversion(conversion);
                        shadow = Some(s);
                        break jpeg.expect("capture");
                    }
                    Captured::Replaced(s, conversion) => {
                        assert!(!replaced, "{step}: replaced twice");
                        replaced = true;
                        inner.note_conversion(conversion);
                        shadow = Some(s);
                    }
                    Captured::Skipped(s) | Captured::Yielded(s) => shadow = Some(s),
                    Captured::Refused(reason) | Captured::Lost { reason, .. } => {
                        panic!("{step}: refused: {reason}")
                    }
                }
            };
            assert_eq!(replaced, step != "sdr", "{step}: only a change of renderer replaces");
            if let Some(dir) = out.as_deref() {
                std::fs::write(dir.join(format!("mixed-{step}.jpg")), &jpeg).expect("writing");
            }
        }
    }

    /// The hidden window lives until dropped, the drop returns (its thread
    /// ended, not detached) and the window is gone after it, with messages
    /// still being posted to it right up to the drop. The stop never goes
    /// through the message queue, so a queue's state cannot keep it alive.
    #[cfg(windows)]
    #[test]
    fn the_hidden_window_stops_on_drop() {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, IsWindowVisible, PostMessageW, WM_NULL};

        const POSTS: usize = 1000;
        let window = HiddenWindow::create().expect("hidden window");
        let hwnd = HWND(window.wid() as *mut _);
        assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
        assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool(), "never shown");
        for _ in 0..POSTS {
            let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };
        }
        drop(window);
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool(), "destroyed with its thread");
    }

    /// The drop returns even while something keeps refilling the window's
    /// queue through it: the pump checks `stop` per message, not only once the
    /// queue is empty (Codex pass 2). The producers post until told to stop
    /// (a full queue only makes a post fail, so failures are ignored) and are
    /// joined before anything is asserted, so the test cannot hang on them.
    #[cfg(windows)]
    #[test]
    fn the_hidden_window_stops_while_its_queue_is_being_refilled() {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_NULL};

        const DROP_BUDGET: Duration = Duration::from_secs(5);
        /// Enough posters to outpace one pump, so the queue never empties.
        const PRODUCERS: usize = 4;
        let window = HiddenWindow::create().expect("hidden window");
        let wid = window.wid();
        // The producers end on this flag, never on the window's state: a
        // destroyed HWND can be reused by another window, which they must not
        // keep posting to.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let producers: Vec<_> = (0..PRODUCERS)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let hwnd = HWND(wid as *mut _);
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };
                    }
                })
            })
            .collect();
        // Let the producer get the queue busy before the drop starts.
        std::thread::sleep(Duration::from_millis(50));
        let (done, dropped) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(window);
            let _ = done.send(());
        });
        // Only that the drop returns: destruction is the_hidden_window_stops_on_drop's
        // to check, without producers racing a reused handle.
        let returned = dropped.recv_timeout(DROP_BUDGET).is_ok();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for producer in producers {
            producer.join().expect("producer");
        }
        assert!(returned, "the drop hung behind a refilled queue");
    }

    /// The shipped cap, and the bound it is derived for.
    #[test]
    fn cache_default_is_the_named_cap() {
        let mut cache = ThumbCache::default();
        for bucket in 0..(CACHE_MAX_THUMBS as u32 + 5) {
            cache.insert(bucket, String::new());
        }
        assert_eq!(cache.frames.len(), CACHE_MAX_THUMBS);
        assert!(!cache.contains(0) && cache.contains(CACHE_MAX_THUMBS as u32 + 4));
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

    /// How long a bench waits for a local file to open and settle (one seek
    /// budget per possible shadow: the software one, then its replacement).
    const BENCH_OPEN_MS: u64 = 2 * SEEK_SETTLE_MS;

    /// Capture the frame at `time` exactly as the worker does, asking again
    /// (as the worker's next job would) while the job is skipped or the
    /// renderer is replaced. Returns the shadow that captured it, what the
    /// frame needed, and the jpg.
    fn capture_at(mut shadow: Shadow, file: &str, time: f64) -> (Shadow, Conversion, Vec<u8>) {
        let deadline = Instant::now() + Duration::from_millis(BENCH_OPEN_MS);
        loop {
            let (width, render) = (shadow.width, shadow.render);
            match checked_capture(shadow, file, time, &|| false, Shadow::capture_bytes) {
                Captured::Frame(done, conversion, jpeg) => {
                    return (done, conversion, jpeg.expect("capture"));
                }
                Captured::Skipped(again) | Captured::Replaced(again, _) | Captured::Yielded(again) => {
                    assert!(Instant::now() < deadline, "{file} at {time:.1}s never captured");
                    shadow = again;
                }
                Captured::Parked => {
                    assert!(Instant::now() < deadline, "{file} at {time:.1}s never captured");
                    shadow = Shadow::spawn(file, width, render).expect("shadow respawn");
                }
                Captured::Refused(reason) | Captured::Lost { reason, .. } => {
                    panic!("{file} refused: {reason}")
                }
            }
        }
    }

    /// Open `file` exactly as the worker does: a software shadow, moved to
    /// whatever the first frame needs.
    fn open_settled(file: &str, width: u32) -> (Shadow, Conversion) {
        let shadow = Shadow::spawn(file, width, Render::Software).expect("shadow spawn");
        let (shadow, conversion, _) = capture_at(shadow, file, 0.0);
        (shadow, conversion)
    }

    /// The shadow as it was before the colour fix: software screenshots
    /// whatever the stream is. The bench's "before".
    fn open_unconverted(file: &str, width: u32) -> Shadow {
        let shadow = Shadow::spawn(file, width, Render::Software).expect("shadow spawn");
        let deadline = Instant::now() + Duration::from_millis(BENCH_OPEN_MS);
        while shadow.duration().is_none() || shadow.mpv.get_property_string("video-params").is_none() {
            assert!(Instant::now() < deadline, "{file} never opened");
            std::thread::sleep(SETTLE_POLL);
        }
        shadow
    }

    /// A shadow whose load mpv ends because the stream cannot be READ is lost,
    /// with mpv's own reason, never left to time out job after job in silence
    /// (and never a refusal: the same stream may be readable a moment later).
    /// Driven here by a file that does not exist ("loading failed"), on both
    /// renderers. Needs `libmpv-2.dll` (next to the test binary or via
    /// `RILLIO_LIBMPV`).
    /// Run: `cargo test --lib thumbs::tests::unreadable_stream -- --ignored`.
    #[test]
    #[ignore = "needs libmpv-2.dll"]
    fn unreadable_stream_loses_the_shadow_with_mpvs_reason() {
        let missing = std::env::temp_dir().join("rillio-thumbs-no-such-file.mkv");
        assert!(!missing.exists());
        let missing = missing.to_string_lossy().to_string();
        for (render, says) in [
            (Render::Libplacebo, "gpu-next could not show the stream"),
            (Render::Software, "the shadow could not show the stream"),
        ] {
            // Through the worker's own path: the seek never settles, and the
            // loss still comes out of it.
            let shadow = Shadow::spawn(&missing, 448, render).expect("shadow spawn");
            match checked_capture(shadow, &missing, 0.0, &|| false, |_| ()) {
                Captured::Lost { reason, .. } => assert!(reason.contains(says), "{reason}"),
                Captured::Refused(reason) => panic!("{render:?}: an unreadable file was refused: {reason}"),
                Captured::Frame(..) | Captured::Skipped(_) | Captured::Replaced(..) | Captured::Yielded(_)
                | Captured::Parked => panic!("{render:?}: an unreadable file was not noticed"),
            }
        }
    }

    /// Lever (a): the player's video-params open the first shadow on the right
    /// renderer, saving the software-open-then-respawn ("double open") for an
    /// HDR/DV stream. Per file: read the colour the player would have (one probe
    /// open), decide the hint, then time the OLD path (software spawn, respawn
    /// on the first frame) against the NEW path (spawn on the hint, no respawn),
    /// to a captured first frame, REPS each. An SDR file is the control: its
    /// hint is Software, so both paths are the same and neither respawns. Needs
    /// `libmpv-2.dll` (next to the test binary or via `RILLIO_LIBMPV`) and:
    ///   RILLIO_THUMBS_DOUBLEOPEN_FILES  the videos, separated by `;`
    ///   RILLIO_THUMBS_BENCH_WIDTH       the thumbnail width (default 640)
    /// Run: `cargo test --lib thumbs::tests::bench_initial_render -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs libmpv-2.dll and RILLIO_THUMBS_DOUBLEOPEN_FILES"]
    fn bench_initial_render_saves_the_double_open() {
        const REPS: usize = 7;
        // Software decode of a 4K HEVC 10-bit DV frame, twice over on the old
        // path (the software shadow, then its libplacebo replacement), is slow
        // on a debug build; give the capture room so the bench measures the
        // cost instead of timing out on it.
        const BENCH_CAPTURE_MS: u64 = 40_000;
        let files = std::env::var("RILLIO_THUMBS_DOUBLEOPEN_FILES").expect("RILLIO_THUMBS_DOUBLEOPEN_FILES");
        let width = thumb_width(
            std::env::var("RILLIO_THUMBS_BENCH_WIDTH").ok().and_then(|w| w.trim().parse().ok()).unwrap_or(640),
        )
        .expect("a usable width");
        // The hint the real code uses comes from the PLAYER, which renders with
        // gpu-next (libplacebo): a DV stream reports colormatrix "dolbyvision"
        // only once that VO has the RPU, which a software probe never sees. So
        // read the bench's hint from a libplacebo shadow (the player's own
        // renderer), settled on a frame, waiting for the colour to decide.
        let player_hint = |file: &str| -> Option<Render> {
            let probe = Shadow::spawn(file, width, Render::Libplacebo).expect("probe spawn");
            let _ = probe.seek_settled(0.0, &|| false);
            let deadline = Instant::now() + Duration::from_millis(BENCH_CAPTURE_MS);
            loop {
                let params = probe.mpv.get_property_string("video-params");
                let profile = probe.mpv.get_property_string("current-tracks/video/dolby-vision-profile");
                let hint = initial_render(params.as_deref(), profile.as_deref());
                if hint.is_some() || Instant::now() >= deadline {
                    break hint;
                }
                std::thread::sleep(SETTLE_POLL);
            }
        };
        // Pay libplacebo's one-time GPU/Vulkan init now (the first probe above
        // already created a libplacebo context), before any measured open.

        for file in files.split(';').map(str::trim).filter(|f| !f.is_empty()) {
            // What the player's own renderer reports for this file, and the hint.
            let hint = player_hint(file);

            // Time one open-to-first-frame; `start` None is the OLD default
            // (software), Some is the hinted renderer. Returns the millis and
            // whether it had to respawn.
            let timed = |start: Option<Render>| -> (f64, bool) {
                let t = Instant::now();
                let mut shadow = Shadow::spawn(file, width, start.unwrap_or(Render::Software)).expect("spawn");
                let mut replaced = false;
                let deadline = Instant::now() + Duration::from_millis(BENCH_CAPTURE_MS);
                loop {
                    let (w, r) = (shadow.width, shadow.render);
                    match checked_capture(shadow, file, 0.0, &|| false, Shadow::capture_bytes) {
                        Captured::Frame(done, _c, jpeg) => {
                            jpeg.expect("capture");
                            drop(done);
                            break;
                        }
                        Captured::Replaced(again, _) => {
                            replaced = true;
                            shadow = again;
                        }
                        Captured::Skipped(again) | Captured::Yielded(again) => {
                            assert!(Instant::now() < deadline, "{file} never captured");
                            shadow = again;
                        }
                        Captured::Parked => {
                            assert!(Instant::now() < deadline, "{file} never captured");
                            shadow = Shadow::spawn(file, w, r).expect("respawn");
                        }
                        Captured::Refused(reason) | Captured::Lost { reason, .. } => panic!("{file}: {reason}"),
                    }
                }
                (t.elapsed().as_secs_f64() * 1000.0, replaced)
            };

            let (mut before, mut after) = (Vec::new(), Vec::new());
            let (mut before_respawns, mut after_respawns) = (0usize, 0usize);
            for _ in 0..REPS {
                let (ms, r) = timed(None);
                before.push(ms);
                before_respawns += r as usize;
                let (ms, r) = timed(hint);
                after.push(ms);
                after_respawns += r as usize;
            }
            let mut before_sorted = before.clone();
            let mut after_sorted = after.clone();
            let (before_med, _) = median_p95(&mut before_sorted);
            let (after_med, _) = median_p95(&mut after_sorted);
            let ms = |v: &[f64]| v.iter().map(|x| format!("{x:.0}")).collect::<Vec<_>>().join(", ");
            println!("== {file}");
            println!("  hint: {hint:?}");
            println!("  OLD software-start: median {before_med:.0} ms, respawned {before_respawns}/{REPS}, all [{}]", ms(&before));
            println!("  NEW hint-start:     median {after_med:.0} ms, respawned {after_respawns}/{REPS}, all [{}]", ms(&after));
        }
    }

    /// A file still opening when a seek budget runs out is KEPT across budgets
    /// (`seek_settled` returns `Opening`, `checked_capture` returns Skipped) so
    /// a slow open is not restarted every request - UNTIL the cumulative
    /// opening deadline, after which a stream that never opens is retired (Lost),
    /// not held forever. Driven by a server that accepts the connection and
    /// never answers, so mpv's open never completes and never errors (check_load
    /// stays Ok). Needs `libmpv-2.dll`. Runs ~OPENING_DEADLINE_MS.
    /// Run: `cargo test --lib thumbs::tests::a_still_opening -- --ignored`.
    #[test]
    #[ignore = "needs libmpv-2.dll"]
    fn a_still_opening_shadow_is_kept_not_dropped() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for s in listener.incoming() {
                held.push(s); // accept, read nothing, answer nothing
            }
        });
        let url = format!("http://127.0.0.1:{port}/never-opens.mkv");
        // seek_settled on a not-yet-open file is Opening, never TimedOut.
        let opening = Shadow::spawn(&url, 320, Render::Software).expect("spawn");
        assert_eq!(opening.seek_settled(0.0, &|| false), Settle::Opening);
        drop(opening);
        // A fresh shadow is kept (Skipped) early, and retired (Lost) once its
        // cumulative opening deadline passes - kept across budgets, not forever.
        let mut shadow = Some(Shadow::spawn(&url, 320, Render::Software).expect("spawn"));
        let start = Instant::now();
        let mut kept_at_least_once = false;
        let mut retired = false;
        // Bound the loop well past the deadline so a bug (kept forever) fails
        // rather than hangs.
        while start.elapsed() < Duration::from_millis(OPENING_DEADLINE_MS * 3) {
            match checked_capture(shadow.take().unwrap(), &url, 0.0, &|| false, |_| ()) {
                Captured::Skipped(s) => {
                    kept_at_least_once = true;
                    shadow = Some(s);
                }
                Captured::Lost { .. } => {
                    retired = true;
                    assert!(
                        start.elapsed() >= Duration::from_millis(OPENING_DEADLINE_MS),
                        "retired before the cumulative opening deadline"
                    );
                    break;
                }
                _ => panic!("a still-opening shadow was neither kept nor retired"),
            }
        }
        assert!(kept_at_least_once, "a still-opening shadow must be kept across the first budgets");
        assert!(retired, "a never-opening shadow must be retired after the cumulative deadline, not kept forever");
    }

    /// Cold shadows that are slow to show their first frame (several 4K DV
    /// opens at once, a GPU busy with other work) must still get there: a seek
    /// whose bytes are in hand but whose frame the VO has not shown yet is
    /// progress, not a stuck read. Measured before the fix: four dv-p5 shadows
    /// opened together never warmed in 60 s on a LOCAL FILE (each timed out,
    /// was dropped, and its replacement paid the whole open again). Needs
    /// `libmpv-2.dll` and RILLIO_THUMBS_BENCH_FILE (a 4K DV/HDR file).
    /// Run: `cargo test --lib thumbs::tests::cold_shadows_opened_together -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs libmpv-2.dll and RILLIO_THUMBS_BENCH_FILE"]
    fn cold_shadows_opened_together_all_show_a_frame() {
        const SHADOWS: usize = 4;
        const WITHIN: Duration = Duration::from_secs(60);
        let file = std::env::var("RILLIO_THUMBS_BENCH_FILE").expect("RILLIO_THUMBS_BENCH_FILE");
        let mut shadows: Vec<Option<Shadow>> =
            (0..SHADOWS).map(|_| Some(Shadow::spawn(&file, 640, Render::Libplacebo).expect("spawn"))).collect();
        let mut warmed = vec![None; SHADOWS];
        let mut parks = vec![0; SHADOWS];
        let mut retired = vec![0; SHADOWS];
        let start = Instant::now();
        while warmed.iter().any(Option::is_none) && start.elapsed() < WITHIN {
            let cold: Vec<usize> = (0..SHADOWS).filter(|&i| warmed[i].is_none()).collect();
            for i in cold {
                match checked_capture(shadows[i].take().unwrap(), &file, 0.0, &|| false, |_| ()) {
                    Captured::Frame(s, ..) => {
                        warmed[i] = Some(start.elapsed());
                        shadows[i] = Some(s);
                    }
                    Captured::Skipped(s) | Captured::Replaced(s, _) | Captured::Yielded(s) => shadows[i] = Some(s),
                    Captured::Parked => {
                        parks[i] += 1;
                        shadows[i] = Some(Shadow::spawn(&file, 640, Render::Libplacebo).expect("respawn"));
                    }
                    // Kept past the busy deadline: retired, and the worker
                    // would open another once the pacing allows.
                    Captured::Lost { .. } => {
                        retired[i] += 1;
                        shadows[i] = Some(Shadow::spawn(&file, 640, Render::Libplacebo).expect("respawn"));
                    }
                    Captured::Refused(r) => panic!("shadow {i}: {r}"),
                }
            }
        }
        println!("first frames at {warmed:?}, parks {parks:?}, retired {retired:?}");
        assert!(warmed.iter().all(Option::is_some), "not every cold shadow showed a frame in {WITHIN:?}: {warmed:?}, parks {parks:?}");
    }

    /// A seek into a region whose bytes do NOT come (a torrent region not
    /// downloaded yet) must still time out and drop the shadow (Parked): mpv
    /// does not abandon that read, and every later job would wait behind it.
    /// Driven by an HTTP server that serves the file's head and tail (so it
    /// opens) and holds every read in the middle forever. Needs
    /// `libmpv-2.dll` and RILLIO_THUMBS_STALL_FILE (a small mkv, e.g. the
    /// hdr10-cll or sdr sample).
    /// Run: `cargo test --lib thumbs::tests::a_seek_into_bytes_that_never_come -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs libmpv-2.dll and RILLIO_THUMBS_STALL_FILE"]
    fn a_seek_into_bytes_that_never_come_is_still_parked() {
        use std::io::{Read, Write};
        let path = std::env::var("RILLIO_THUMBS_STALL_FILE").expect("RILLIO_THUMBS_STALL_FILE");
        let bytes = std::sync::Arc::new(std::fs::read(&path).expect("the file"));
        let len = bytes.len();
        // Served: the first and last quarter. Held forever: the middle half.
        let (stall_from, stall_to) = (len / 4, len - len / 4);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let bytes = bytes.clone();
                std::thread::spawn(move || {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    let from: usize = request
                        .lines()
                        .find_map(|l| l.strip_prefix("range: bytes="))
                        .and_then(|r| r.split('-').next())
                        .and_then(|a| a.trim().parse().ok())
                        .unwrap_or(0);
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Type: video/x-matroska\r\n\
                         Content-Range: bytes {from}-{}/{len}\r\nContent-Length: {}\r\n\r\n",
                        len - 1,
                        len - from
                    );
                    if stream.write_all(head.as_bytes()).is_err() {
                        return;
                    }
                    let mut at = from;
                    while at < len {
                        if (stall_from..stall_to).contains(&at) {
                            // Bytes that never come: hold the connection open.
                            loop {
                                std::thread::sleep(Duration::from_secs(1));
                                if stream.write_all(&[]).is_err() {
                                    return;
                                }
                            }
                        }
                        let end = if at < stall_from { stall_from.min(len) } else { len }.min(at + 64 * 1024);
                        if stream.write_all(&bytes[at..end]).is_err() {
                            return;
                        }
                        at = end;
                    }
                });
            }
        });
        let url = format!("http://127.0.0.1:{port}/stall.mkv");
        let mut shadow = Shadow::spawn(&url, 320, Render::Software).expect("spawn");
        // It opens and shows the head (the served first quarter).
        let open_deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match checked_capture(shadow, &url, 0.0, &|| false, |_| ()) {
                Captured::Frame(s, ..) => { shadow = s; break; }
                Captured::Skipped(s) | Captured::Replaced(s, _) | Captured::Yielded(s) => {
                    let p = |n: &str| s.mpv.get_property_string(n).unwrap_or_else(|| "-".into());
                    println!(
                        "    head not shown yet: seeking={} time-pos={} video-params={} demuxer-cache-state={}",
                        p("seeking"), p("time-pos"), p("video-params").chars().take(60).collect::<String>(),
                        p("demuxer-cache-state"),
                    );
                    shadow = s
                }
                other => panic!("the served head did not open: {}", match other {
                    Captured::Parked => "parked".to_owned(),
                    Captured::Refused(r) | Captured::Lost { reason: r, .. } => r,
                    _ => unreachable!(),
                }),
            }
            assert!(Instant::now() < open_deadline, "the served head never showed a frame");
        }
        // A seek into the middle (held bytes) is a stuck read: dropped.
        let duration = shadow.duration().expect("a duration");
        let middle = duration / 2.0;
        let t = Instant::now();
        let outcome = checked_capture(shadow, &url, middle, &|| false, |_| ());
        let took = t.elapsed();
        assert!(matches!(outcome, Captured::Parked), "a seek into bytes that never come must park the shadow");
        // Its teardown waits for mpv to cancel the held read; the worker must
        // not: the job ends with its seek budget, not with the teardown.
        let budget = Duration::from_millis(SEEK_SETTLE_MS) + Duration::from_secs(2);
        assert!(took < budget, "parking waited for the shadow's teardown: {took:?}");
    }

    /// The evidence for [`hwdec_for`]. Per file, on the player's own hinted
    /// renderer, capture with hwdec=no (software decode, the colour reference)
    /// and with each candidate hwdec, INTERLEAVED per time point (so the
    /// owner's live GPU/CPU load lands on every arm alike). Reports each arm's
    /// open-to-first-frame and per-capture ms, the paired per-point ratio to
    /// software, and the mean per-channel RGB difference to software's image -
    /// the colour MUST NOT move (the shipped choice measured 0.000; the earlier
    /// SW-vs-filter noise floor was DV ~1.9, SDR ~0.7). Needs `libmpv-2.dll` and:
    ///   RILLIO_THUMBS_BENCH_FILES  the videos, separated by `;`
    ///   RILLIO_THUMBS_BENCH_WIDTH  the thumbnail width (default 640)
    ///   RILLIO_THUMBS_HWDEC_CANDIDATES  the hwdecs to test, `;` separated
    ///                                   (default `d3d11va-copy`; native
    ///                                   `d3d11va` fails the size check: its
    ///                                   D3D11 surfaces bypass the software
    ///                                   scale and mpv shows the full frame)
    ///   RILLIO_THUMBS_BENCH_OUT  optional directory to keep every jpg in
    /// A candidate FAILS (the test panics at the end, after every file is
    /// reported) when any image size differs from software's (a different size
    /// is itself a behaviour change, and no colour diff is defined across it),
    /// or when mpv did not actually decode with it (`hwdec-current` = no: the
    /// timing would be software's).
    /// Run: `cargo test --lib thumbs::tests::bench_hwdec -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs libmpv-2.dll and RILLIO_THUMBS_BENCH_FILES"]
    fn bench_hwdec() {
        const SAMPLES: u32 = 8;
        const ROUNDS: u32 = 2;
        const OPEN_REPS: usize = 6;
        let files = std::env::var("RILLIO_THUMBS_BENCH_FILES").expect("RILLIO_THUMBS_BENCH_FILES");
        let width = thumb_width(
            std::env::var("RILLIO_THUMBS_BENCH_WIDTH").ok().and_then(|w| w.trim().parse().ok()).unwrap_or(640),
        )
        .expect("a usable width");
        let candidates: Vec<String> = std::env::var("RILLIO_THUMBS_HWDEC_CANDIDATES")
            .unwrap_or_else(|_| "d3d11va-copy".to_string())
            .split(';')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .collect();
        let out = std::env::var("RILLIO_THUMBS_BENCH_OUT").ok().map(std::path::PathBuf::from);
        if let Some(dir) = &out {
            std::fs::create_dir_all(dir).expect("the output directory");
        }
        let mut failures: Vec<String> = Vec::new();

        // One arm = one hwdec, on the renderer the player's frames call for.
        let spawn_arm = |file: &str, render: Render, arm: &str| -> Shadow {
            let hwdec: &'static str = Box::leak(arm.to_owned().into_boxed_str());
            Shadow::spawn_decoding(file, width, render, hwdec).expect("spawn")
        };
        // One capture at `time` on `shadow` through the worker's checked path:
        // its ms and the decoded RGB pixels. A generous deadline: a 4K software
        // decode under live GPU contention (the owner working) can run many
        // seconds, and here that is slowness to measure, not a failure.
        const HWDEC_CAPTURE_MS: u64 = 60_000;
        let capture_one = |mut shadow: Shadow, file: &str, arm: &str, time: f64| -> (Shadow, f64, Vec<u8>) {
            let t = Instant::now();
            let deadline = t + Duration::from_millis(HWDEC_CAPTURE_MS);
            let mut parked = 0;
            let jpeg = loop {
                let render = shadow.render;
                match checked_capture(shadow, file, time, &|| false, Shadow::capture_bytes) {
                    Captured::Frame(s, _c, jpeg) => {
                        shadow = s;
                        break jpeg.expect("capture");
                    }
                    Captured::Replaced(s, _) => shadow = s,
                    Captured::Skipped(s) | Captured::Yielded(s) => {
                        assert!(Instant::now() < deadline, "{file} arm {arm} at {time:.1}s never captured (parked {parked}x)");
                        shadow = s;
                    }
                    Captured::Parked => {
                        parked += 1;
                        println!("     {arm} at {time:.1}s: seek did not settle in {SEEK_SETTLE_MS} ms, shadow dropped ({parked}x)");
                        assert!(Instant::now() < deadline, "{file} arm {arm} at {time:.1}s never captured (parked {parked}x)");
                        shadow = spawn_arm(file, render, arm);
                    }
                    Captured::Refused(reason) | Captured::Lost { reason, .. } => panic!("{file} arm {arm}: {reason}"),
                }
            };
            (shadow, t.elapsed().as_secs_f64() * 1000.0, jpeg)
        };
        let decode = |jpeg: &[u8]| {
            image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).expect("jpg").to_rgb8()
        };

        // Mean per-channel absolute difference between two images of the SAME
        // size (the caller has checked; there is no diff across sizes).
        let mean_rgb_diff = |a: &image::RgbImage, b: &image::RgbImage| -> f64 {
            assert_eq!(a.dimensions(), b.dimensions(), "mean_rgb_diff across sizes");
            let (sum, n) = a.pixels().zip(b.pixels()).fold((0u64, 0u64), |(s, n), (p, q)| {
                (s + p.0.iter().zip(q.0).map(|(x, y)| x.abs_diff(y) as u64).sum::<u64>(), n + 3)
            });
            sum as f64 / n as f64
        };

        // Arm 0 is the reference: software decode, software scale.
        let arms: Vec<String> = std::iter::once("no".to_string()).chain(candidates.iter().cloned()).collect();
        for file in files.split(';').map(str::trim).filter(|f| !f.is_empty()) {
            let stem = std::path::Path::new(file).file_stem().and_then(|s| s.to_str()).unwrap_or(file);
            // The renderer the player uses for this file (libplacebo for DV/HDR).
            // Decide it from a gpu-next probe, waiting for the colour to SETTLE:
            // a DV stream reports colormatrix "dolbyvision" only once the VO has
            // the RPU, so reading it once (before that) misclassifies DV as SDR.
            let probe = Shadow::spawn(file, width, Render::Libplacebo).expect("probe");
            let _ = probe.seek_settled(0.0, &|| false);
            let deadline = Instant::now() + Duration::from_millis(BENCH_OPEN_MS);
            let (duration, render) = loop {
                let d = probe.duration();
                let params = probe.mpv.get_property_string("video-params");
                let profile = probe.mpv.get_property_string("current-tracks/video/dolby-vision-profile");
                let hint = initial_render(params.as_deref(), profile.as_deref());
                if let (Some(d), Some(render)) = (d, hint) {
                    break (d, render);
                }
                assert!(Instant::now() < deadline, "{stem} never reported duration + colour");
                std::thread::sleep(SETTLE_POLL);
            };
            drop(probe);

            // INTERLEAVED: every arm keeps its own warm shadow, and each time
            // point is captured by every arm back to back (order rotated per
            // point), so the owner's live GPU/CPU load lands on all arms alike.
            // Each round uses its own time points (no demuxer-cache reuse).
            let step = duration / f64::from(SAMPLES * ROUNDS + 1);
            let times: Vec<f64> = (1..=SAMPLES * ROUNDS).map(|i| step * f64::from(i)).collect();
            // Open-to-first-frame per arm (spawn + first capture at 0, the
            // first-hover cost), OPEN_REPS times, arms alternated.
            let mut open_ms: Vec<Vec<f64>> = vec![Vec::new(); arms.len()];
            for rep in 0..OPEN_REPS {
                for k in 0..arms.len() {
                    let a = (rep + k) % arms.len();
                    let t = Instant::now();
                    let (s, _, _) = capture_one(spawn_arm(file, render, &arms[a]), file, &arms[a], 0.0);
                    open_ms[a].push(t.elapsed().as_secs_f64() * 1000.0);
                    drop(s);
                }
            }
            let mut shadows: Vec<Option<Shadow>> = arms.iter().map(|arm| Some(spawn_arm(file, render, arm))).collect();
            // Warm every arm on a first frame (open cost is measured above).
            for (a, arm) in arms.iter().enumerate() {
                let (s, _, _) = capture_one(shadows[a].take().unwrap(), file, arm, 0.0);
                shadows[a] = Some(s);
            }
            let mut ms: Vec<Vec<f64>> = vec![Vec::new(); arms.len()];
            let mut imgs: Vec<Vec<image::RgbImage>> = vec![Vec::new(); arms.len()];
            for (i, &time) in times.iter().enumerate() {
                for k in 0..arms.len() {
                    let a = (i + k) % arms.len();
                    let (s, took, jpeg) = capture_one(shadows[a].take().unwrap(), file, &arms[a], time);
                    shadows[a] = Some(s);
                    ms[a].push(took);
                    if let Some(dir) = &out {
                        std::fs::write(dir.join(format!("{stem}-{}-{i:02}.jpg", arms[a])), &jpeg).expect("writing the jpg");
                    }
                    imgs[a].push(decode(&jpeg));
                }
            }
            let state: Vec<String> = shadows
                .iter()
                .map(|s| {
                    let s = s.as_ref().unwrap();
                    let prop = |name: &str| s.mpv.get_property_string(name).unwrap_or_else(|| "-".into());
                    let vop: serde_json::Value = serde_json::from_str(&prop("video-out-params")).unwrap_or_default();
                    format!(
                        "hwdec-current={} vo-params: {} {}x{} chroma-location={}",
                        prop("hwdec-current"),
                        vop["pixelformat"],
                        vop["w"],
                        vop["h"],
                        vop["chroma-location"],
                    )
                })
                .collect();
            drop(shadows);

            println!("== {stem} ({render:?}), {} interleaved captures per arm at {width}px", times.len());
            for (a, arm) in arms.iter().enumerate() {
                // Per point, this arm's time over software's at the same point
                // (same moment, same load): the contention-robust comparison.
                let mut ratio: Vec<f64> = ms[a].iter().zip(&ms[0]).map(|(x, sw)| x / sw).collect();
                let (med, p95) = median_p95(&mut ms[a].clone());
                let (ratio_med, _) = median_p95(&mut ratio);
                let sizes: std::collections::BTreeSet<_> = imgs[a].iter().map(|i| i.dimensions()).collect();
                let opens: Vec<String> = open_ms[a].iter().map(|m| format!("{m:.0}")).collect();
                let (open_med, _) = median_p95(&mut open_ms[a].clone());
                println!(
                    "  {arm}: median {med:.0} ms, p95 {p95:.0} ms, paired ratio vs no {ratio_med:.2}; sizes {sizes:?}; \
                     open-to-first-frame median {open_med:.0} ms, all [{}]",
                    opens.join(", ")
                );
                println!("     {}", state[a]);
                if a == 0 {
                    continue;
                }
                if state[a].starts_with("hwdec-current=no") || state[a].starts_with("hwdec-current=-") {
                    failures.push(format!("{stem}/{arm}: mpv did not decode with it ({})", state[a]));
                }
                let mismatched: Vec<String> = imgs[0]
                    .iter()
                    .zip(&imgs[a])
                    .enumerate()
                    .filter(|(_, (x, y))| x.dimensions() != y.dimensions())
                    .map(|(i, (x, y))| format!("#{i} {:?} vs {:?}", x.dimensions(), y.dimensions()))
                    .collect();
                if !mismatched.is_empty() {
                    println!("  {arm}: SIZE MISMATCH vs software: {} of {}", mismatched.len(), times.len());
                    failures.push(format!("{stem}/{arm}: size differs from software: {}", mismatched[0]));
                    continue;
                }
                let diffs: Vec<f64> = imgs[0].iter().zip(&imgs[a]).map(|(x, y)| mean_rgb_diff(x, y)).collect();
                let worst = diffs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let mean = diffs.iter().sum::<f64>() / diffs.len() as f64;
                println!("  {arm}: mean-RGB diff vs software: mean {mean:.3}, worst {worst:.3}");
            }
        }
        assert!(failures.is_empty(), "hwdec candidates failed:\n{}", failures.join("\n"));
    }

    thread_local! {
        /// Extra mpv options the next [`Shadow::spawn_decoding`] on this thread
        /// applies over its own (bench arms only).
        pub(super) static BENCH_EXTRA_OPTIONS: std::cell::RefCell<Vec<(String, String)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Where a live capture's time goes. Per file, on the player's own hinted
    /// renderer and decoder, one warm shadow per READ ARM (the same file read
    /// different ways: a local path, HTTP, HTTP with other cache options),
    /// captured INTERLEAVED per time point so every arm sees the same machine
    /// load, each capture split into seek-to-settled and screenshot ms. Run
    /// twice: alone, and beside a "player" (a second mpv PLAYING the same file
    /// on gpu-next with hardware decode, muted, as the shell's player does).
    ///   RILLIO_THUMBS_BENCH_FILES  local video paths, `;` separated
    ///   RILLIO_THUMBS_READ_ARMS    `label=url[|opt=val,opt=val]`, `;` separated;
    ///                              `{path}` / `{name}` stand for the file's path
    ///                              and file name (e.g. `http=http://127.0.0.1:8778/{name}`)
    ///   RILLIO_THUMBS_BENCH_WIDTH  thumbnail width (default 640)
    /// Run: `cargo test --lib thumbs::tests::bench_read_path -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs libmpv-2.dll, RILLIO_THUMBS_BENCH_FILES and RILLIO_THUMBS_READ_ARMS"]
    fn bench_read_path() {
        const SAMPLES: u32 = 8;
        let files = std::env::var("RILLIO_THUMBS_BENCH_FILES").expect("RILLIO_THUMBS_BENCH_FILES");
        let arms_spec = std::env::var("RILLIO_THUMBS_READ_ARMS").expect("RILLIO_THUMBS_READ_ARMS");
        let width = thumb_width(
            std::env::var("RILLIO_THUMBS_BENCH_WIDTH").ok().and_then(|w| w.trim().parse().ok()).unwrap_or(640),
        )
        .expect("a usable width");
        // (label, url template, extra options)
        let arms: Vec<(String, String, Vec<(String, String)>)> = arms_spec
            .split(';')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(|arm| {
                let (label, rest) = arm.split_once('=').expect("label=url");
                let (url, opts) = rest.split_once('|').unwrap_or((rest, ""));
                let opts = opts
                    .split(',')
                    .filter(|o| !o.is_empty())
                    .map(|o| {
                        let (k, v) = o.split_once('=').expect("opt=val");
                        (k.to_owned(), v.to_owned())
                    })
                    .collect();
                (label.to_owned(), url.to_owned(), opts)
            })
            .collect();
        let spawn_arm = |url: &str, render: Render, opts: &[(String, String)]| -> Shadow {
            BENCH_EXTRA_OPTIONS.with(|extra| *extra.borrow_mut() = opts.to_vec());
            let shadow = Shadow::spawn(url, width, render).expect("spawn");
            BENCH_EXTRA_OPTIONS.with(|extra| extra.borrow_mut().clear());
            shadow
        };
        // The stand-in player: what shell.rs's player is to this file, as far
        // as load goes (gpu-next, hardware decode, playing, looping, muted).
        let spawn_player = |path: &str| -> (Mpv, HiddenWindow) {
            let window = HiddenWindow::create().expect("player window");
            let mpv = Mpv::load(&mpv::default_dll_path()).expect("player dll");
            let wid = window.wid().to_string();
            for (name, value) in [
                ("vo", "gpu-next"),
                ("wid", wid.as_str()),
                ("hwdec", "auto-copy"),
                ("ao", "null"),
                ("mute", "yes"),
                ("loop-file", "inf"),
                ("config", "no"),
                ("load-scripts", "no"),
                ("osc", "no"),
            ] {
                mpv.set_option(name, value).unwrap_or_else(|e| panic!("player {name}={value}: {e}"));
            }
            mpv.initialize().expect("player init");
            mpv.command(&["loadfile", path]).expect("player loadfile");
            (mpv, window)
        };

        for path in files.split(';').map(str::trim).filter(|f| !f.is_empty()) {
            let name = std::path::Path::new(path).file_name().and_then(|s| s.to_str()).expect("a file name");
            let urls: Vec<String> =
                arms.iter().map(|(_, url, _)| url.replace("{path}", path).replace("{name}", name)).collect();
            // The renderer the player's frames call for (gpu-next probe, colour settled).
            let probe = Shadow::spawn(path, width, Render::Libplacebo).expect("probe");
            let _ = probe.seek_settled(0.0, &|| false);
            let deadline = Instant::now() + Duration::from_millis(BENCH_OPEN_MS);
            let (duration, render) = loop {
                let params = probe.mpv.get_property_string("video-params");
                let profile = probe.mpv.get_property_string("current-tracks/video/dolby-vision-profile");
                if let (Some(d), Some(r)) = (probe.duration(), initial_render(params.as_deref(), profile.as_deref())) {
                    break (d, r);
                }
                assert!(Instant::now() < deadline, "{name} never reported duration + colour");
                std::thread::sleep(SETTLE_POLL);
            };
            drop(probe);

            for (phase, with_player) in [("alone", false), ("beside a playing player", true)] {
                let _player = with_player.then(|| spawn_player(path));
                if with_player {
                    std::thread::sleep(Duration::from_secs(3)); // let it reach steady playback
                }
                // Own time points per phase (no demuxer-cache reuse across phases).
                let offset = if with_player { 0.5 } else { 0.0 };
                let step = duration / f64::from(SAMPLES + 1);
                let times: Vec<f64> = (1..=SAMPLES).map(|i| step * (f64::from(i) + offset * 0.5)).collect();
                // Spawn and warm each arm on a first frame ONE AT A TIME (four
                // 4K opens at once park each other past the 4 s settle budget
                // forever: measured). Open cost is not this bench's.
                let mut shadows: Vec<Option<Shadow>> = Vec::new();
                for (a, url) in urls.iter().enumerate() {
                    let mut s = spawn_arm(url, render, &arms[a].2);
                    let warm_started = Instant::now();
                    let mut parks = 0;
                    loop {
                        match checked_capture(s, url, 0.0, &|| false, |_| ()) {
                            Captured::Frame(done, ..) => { s = done; break; }
                            Captured::Skipped(again) | Captured::Replaced(again, _) | Captured::Yielded(again) => s = again,
                            Captured::Parked => { parks += 1; s = spawn_arm(url, render, &arms[a].2) }
                            Captured::Refused(r) | Captured::Lost { reason: r, .. } => panic!("{name} {}: {r}", arms[a].0),
                        }
                        assert!(warm_started.elapsed() < Duration::from_secs(60), "{name} {} never warmed ({parks} parks)", arms[a].0);
                    }
                    if parks > 0 {
                        println!("  ({} warmed after {parks} parks, {:.0} ms)", arms[a].0, warm_started.elapsed().as_secs_f64() * 1000.0);
                    }
                    shadows.push(Some(s));
                }
                // Per arm: seek ms, screenshot ms (settled captures), and misses.
                let mut seek_ms: Vec<Vec<f64>> = vec![Vec::new(); arms.len()];
                let mut shot_ms: Vec<Vec<f64>> = vec![Vec::new(); arms.len()];
                let mut misses: Vec<Vec<String>> = vec![Vec::new(); arms.len()];
                for (i, &time) in times.iter().enumerate() {
                    for k in 0..arms.len() {
                        let a = (i + k) % arms.len();
                        let shadow = shadows[a].take().unwrap();
                        let t = Instant::now();
                        let settle = shadow.seek_settled(time, &|| false);
                        let seek = t.elapsed().as_secs_f64() * 1000.0;
                        if settle != Settle::Settled {
                            misses[a].push(format!("{time:.1}s {settle:?} after {seek:.0} ms"));
                            // As the worker does: a stuck read starts clean, a
                            // busy shadow is kept.
                            shadows[a] = Some(if settle == Settle::Busy {
                                shadow
                            } else {
                                drop(shadow);
                                spawn_arm(&urls[a], render, &arms[a].2)
                            });
                            continue;
                        }
                        match step_for(shadow.frame_conversion(), shadow.render) {
                            Step::Capture(_) => {}
                            other => panic!("{name} {}: frame at {time:.1}s: {other:?}", arms[a].0),
                        }
                        let t = Instant::now();
                        shadow.capture_bytes().expect("capture");
                        shot_ms[a].push(t.elapsed().as_secs_f64() * 1000.0);
                        seek_ms[a].push(seek);
                        shadows[a] = Some(shadow);
                    }
                }
                drop(shadows);
                println!("== {name} ({render:?}, {}) {phase}, {} points per arm", hwdec_for(render), times.len());
                for (a, (label, _, opts)) in arms.iter().enumerate() {
                    let fmt = |v: &mut Vec<f64>| {
                        if v.is_empty() { return "-".to_owned(); }
                        let (m, p) = median_p95(v);
                        format!("median {m:.0} / p95 {p:.0}")
                    };
                    println!(
                        "  {label:<14} seek {} ms | screenshot {} ms | misses {}{} {opts:?}",
                        fmt(&mut seek_ms[a].clone()),
                        fmt(&mut shot_ms[a].clone()),
                        misses[a].len(),
                        if misses[a].is_empty() { String::new() } else { format!(" [{}]", misses[a].join("; ")) },
                    );
                }
            }
        }
    }

    /// What a scene-sweep sample between two hovers costs the next hover. Per
    /// file, on its hinted renderer and decoder, one warm shadow; per time
    /// point, alternating which comes first: a hover capture at the hover width
    /// on a shadow already at it ("same width"), and a hover capture right
    /// after a sweep sample at SCAN_WIDTH ("after a flip", the width change and
    /// back each rebuild the filter chain). Reports both and the sweep sample.
    ///   RILLIO_THUMBS_BENCH_FILES  local video paths, `;` separated
    /// Run: `cargo test --lib thumbs::tests::bench_width_flip -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs libmpv-2.dll and RILLIO_THUMBS_BENCH_FILES"]
    fn bench_width_flip() {
        const SAMPLES: u32 = 8;
        const HOVER: u32 = 640;
        let files = std::env::var("RILLIO_THUMBS_BENCH_FILES").expect("RILLIO_THUMBS_BENCH_FILES");
        for path in files.split(';').map(str::trim).filter(|f| !f.is_empty()) {
            let name = std::path::Path::new(path).file_name().and_then(|s| s.to_str()).expect("a name");
            let probe = Shadow::spawn(path, HOVER, Render::Libplacebo).expect("probe");
            let _ = probe.seek_settled(0.0, &|| false);
            let deadline = Instant::now() + Duration::from_millis(BENCH_OPEN_MS);
            let (duration, render) = loop {
                let params = probe.mpv.get_property_string("video-params");
                let profile = probe.mpv.get_property_string("current-tracks/video/dolby-vision-profile");
                if let (Some(d), Some(r)) = (probe.duration(), initial_render(params.as_deref(), profile.as_deref())) {
                    break (d, r);
                }
                assert!(Instant::now() < deadline, "{name} never reported duration + colour");
                std::thread::sleep(SETTLE_POLL);
            };
            drop(probe);
            // The worker's checked path with a deadline long enough for a cold
            // 4K frame under live load (capture_at's 8 s is not).
            let capture_at = |mut shadow: Shadow, path: &str, at: f64| -> (Shadow, (), ()) {
                let deadline = Instant::now() + Duration::from_secs(60);
                loop {
                    let render = shadow.render;
                    match checked_capture(shadow, path, at, &|| false, |_| ()) {
                        Captured::Frame(s, ..) => return (s, (), ()),
                        Captured::Skipped(s) | Captured::Replaced(s, _) | Captured::Yielded(s) => {
                            let p = |n: &str| s.mpv.get_property_string(n).unwrap_or_else(|| "-".into());
                            println!(
                                "    skipped at {at:.1}s: seeking={} time-pos={} vo-configured={} hwdec-current={} \
                                 video-params={} errors={:?}",
                                p("seeking"), p("time-pos"), p("vo-configured"), p("hwdec-current"),
                                p("video-params").chars().take(80).collect::<String>(),
                                s.mpv_errors.borrow(),
                            );
                            shadow = s
                        }
                        Captured::Parked => shadow = Shadow::spawn(path, HOVER, render).expect("respawn"),
                        Captured::Refused(r) | Captured::Lost { reason: r, .. } => panic!("{path}: {r}"),
                    }
                    assert!(Instant::now() < deadline, "{path} at {at:.1}s never captured");
                }
            };
            let mut shadow = Shadow::spawn(path, HOVER, render).expect("spawn");
            let (s, _, _) = capture_at(shadow, path, 0.0);
            shadow = s;
            // Distinct points for each kind so neither reuses the other's cache.
            let step = duration / f64::from(2 * SAMPLES + 1);
            let (mut same, mut flipped, mut sweep) = (Vec::new(), Vec::new(), Vec::new());
            for i in 0..SAMPLES {
                let a = step * f64::from(2 * i + 1);
                let b = step * f64::from(2 * i + 2);
                let mut hover_same = |shadow: Shadow, at: f64| {
                    let t = Instant::now();
                    let mut shadow = shadow;
                    shadow.set_width(HOVER).expect("width");
                    let (s, _, _) = capture_at(shadow, path, at);
                    same.push(t.elapsed().as_secs_f64() * 1000.0);
                    s
                };
                let mut sweep_then_hover = |shadow: Shadow, at: f64| {
                    let mut shadow = shadow;
                    let t = Instant::now();
                    shadow.set_width(SCAN_WIDTH).expect("scan width");
                    let (s, _, _) = capture_at(shadow, path, at + step / 2.0);
                    sweep.push(t.elapsed().as_secs_f64() * 1000.0);
                    shadow = s;
                    let t = Instant::now();
                    shadow.set_width(HOVER).expect("width");
                    let (s, _, _) = capture_at(shadow, path, at);
                    flipped.push(t.elapsed().as_secs_f64() * 1000.0);
                    s
                };
                if i % 2 == 0 {
                    shadow = hover_same(shadow, a);
                    shadow = sweep_then_hover(shadow, b);
                } else {
                    shadow = sweep_then_hover(shadow, a);
                    shadow = hover_same(shadow, b);
                }
            }
            let show = |v: &mut Vec<f64>| { let (m, p) = median_p95(v); format!("median {m:.0} / p95 {p:.0} ms") };
            println!("== {name} ({render:?}, {})", hwdec_for(render));
            println!("  hover, same width:        {}", show(&mut same));
            println!("  hover after a sweep flip: {}", show(&mut flipped));
            println!("  the sweep sample itself:  {}", show(&mut sweep));
        }
    }

    /// The median and the 95th percentile (nearest rank) of `millis`.
    fn median_p95(millis: &mut [f64]) -> (f64, f64) {
        const P95: f64 = 0.95;
        millis.sort_by(|a, b| a.total_cmp(b));
        let n = millis.len();
        let rank = ((P95 * n as f64).ceil() as usize).clamp(1, n);
        (millis[n / 2], millis[rank - 1])
    }

    /// Colour before/after through the real shadow code path, per file: what
    /// mpv reports, the conversion decided from it, then the same evenly
    /// spread positions captured by the unconverted shadow ("before") and by
    /// the settled one ("after"), each timed (seek + screenshot), and scene
    /// scan samples interleaved on the settled shadow. Needs `libmpv-2.dll`
    /// (next to the test binary or via `RILLIO_LIBMPV`) and:
    ///   RILLIO_THUMBS_BENCH_FILES  the videos, separated by `;`
    ///   RILLIO_THUMBS_BENCH_WIDTH  the thumbnail width, e.g. `448`
    ///   RILLIO_THUMBS_BENCH_OUT    the directory the jpgs are written to
    /// Run: `cargo test --lib thumbs::tests::bench_colour -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs libmpv-2.dll and RILLIO_THUMBS_BENCH_FILES"]
    fn bench_colour_conversion() {
        const SAMPLES: u32 = 12;
        let files = std::env::var("RILLIO_THUMBS_BENCH_FILES").expect("RILLIO_THUMBS_BENCH_FILES");
        let width = thumb_width(
            std::env::var("RILLIO_THUMBS_BENCH_WIDTH")
                .expect("RILLIO_THUMBS_BENCH_WIDTH")
                .trim()
                .parse()
                .expect("a width"),
        )
        .expect("a usable width");
        let out = std::path::PathBuf::from(
            std::env::var("RILLIO_THUMBS_BENCH_OUT").expect("RILLIO_THUMBS_BENCH_OUT"),
        );
        std::fs::create_dir_all(&out).expect("the output directory");

        for file in files.split(';').map(str::trim).filter(|f| !f.is_empty()) {
            let stem = std::path::Path::new(file)
                .file_stem()
                .and_then(|s| s.to_str())
                .expect("a file name")
                .to_owned();
            let started = Instant::now();
            let (mut after, conversion) = open_settled(file, width);
            let open_ms = started.elapsed().as_secs_f64() * 1000.0;
            let duration = after.duration().expect("a duration");
            let times: Vec<f64> =
                (1..=SAMPLES).map(|i| duration * f64::from(i) / f64::from(SAMPLES + 1)).collect();
            println!("== {stem}");
            println!(
                "  video-params: {}",
                after.mpv.get_property_string("video-params").unwrap_or_default()
            );
            println!(
                "  dolby-vision-profile: {:?}, current-vo: {:?}",
                after.mpv.get_property_string("current-tracks/video/dolby-vision-profile"),
                after.mpv.get_property_string("current-vo"),
            );
            println!(
                "  conversion: {conversion} -> {:?}, opened + settled in {open_ms:.0} ms",
                after.render
            );

            // Through the worker's own checked path: seek, re-read the frame's
            // parameters, capture only if this shadow renders them.
            let mut after_ms = Vec::new();
            let mut scan_ms = Vec::new();
            for (index, &time) in times.iter().enumerate() {
                let t = Instant::now();
                after.set_width(width).expect("thumb width");
                let (shadow, frame, jpeg) = capture_at(after, file, time);
                after = shadow;
                after_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                assert_eq!(frame, conversion, "{stem} changed conversion at {time:.1}s");
                std::fs::write(out.join(format!("{stem}-after-{index:02}.jpg")), &jpeg)
                    .expect("writing the jpg");
            }
            // The scene sweep on the same shadow: at its own width, seek,
            // check, capture, reduce to the luma grid.
            for &time in &times {
                let t = Instant::now();
                after.set_width(SCAN_WIDTH).expect("scan width");
                let (shadow, _, jpeg) = capture_at(after, file, time);
                after = shadow;
                luma_grid(&jpeg).expect("luma");
                scan_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            drop(after);

            let mut before = open_unconverted(file, width);
            let mut before_ms = Vec::new();
            for (index, &time) in times.iter().enumerate() {
                let t = Instant::now();
                before.set_width(width).expect("thumb width");
                assert_eq!(before.seek_settled(time, &|| false), Settle::Settled, "seek to {time:.1}s");
                let jpeg = before.capture_bytes().expect("capture");
                before_ms.push(t.elapsed().as_secs_f64() * 1000.0);
                std::fs::write(out.join(format!("{stem}-before-{index:02}.jpg")), &jpeg)
                    .expect("writing the jpg");
            }
            drop(before);

            for (label, millis) in
                [("before", &mut before_ms), ("after", &mut after_ms), ("scan", &mut scan_ms)]
            {
                let (median, p95) = median_p95(millis);
                println!(
                    "  {label}: median {median:.0} ms, p95 {p95:.0} ms over {} captures",
                    millis.len()
                );
            }
        }
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

        let (mut shadow, _) = open_settled(&file, widths[0]);
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
                let mut ipc_micros = 0u128;
                let mut millis: Vec<f64> = Vec::new();
                let mut scan_millis: Vec<f64> = Vec::new();
                let mut size = (0, 0);
                for (index, &time) in times.iter().enumerate() {
                    if interleave_scan {
                        let started = Instant::now();
                        shadow.set_width(SCAN_WIDTH).expect("scan width");
                        assert_eq!(shadow.seek_settled(time, &|| false), Settle::Settled, "scan seek to {time:.0}s");
                        luma_grid(&shadow.capture_bytes().expect("scan capture")).expect("luma");
                        scan_millis.push(started.elapsed().as_secs_f64() * 1000.0);
                    }
                    let started = Instant::now();
                    shadow.set_width(width).expect("thumb width");
                    assert_eq!(shadow.seek_settled(time, &|| false), Settle::Settled, "seek to {time:.0}s did not settle");
                    let jpeg = shadow.capture_bytes().expect("capture");
                    millis.push(started.elapsed().as_secs_f64() * 1000.0);
                    bytes_total += jpeg.len();
                    // The shell's half of the IPC: the data URL, then the JSON
                    // string Tauri hands the webview.
                    let started = Instant::now();
                    use base64::Engine as _;
                    let data_url = format!(
                        "data:image/jpeg;base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(&jpeg)
                    );
                    let json = serde_json::to_string(&Some(data_url)).expect("json");
                    ipc_micros += started.elapsed().as_micros();
                    assert!(json.len() > jpeg.len());
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
                     ({} as a data URL), median {:.0} ms, slowest {:.0} ms over {n} samples, \
                     base64 + json {} us/thumb",
                    size.0,
                    size.1,
                    bytes_total / n,
                    (bytes_total / n).div_ceil(3) * 4,
                    millis[n / 2],
                    millis[n - 1],
                    ipc_micros / n as u128,
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
) -> Result<ThumbAnswer, String> {
    if !time_sec.is_finite() || time_sec < 0.0 {
        return Err(format!("thumbs: no preview exists at {time_sec} s"));
    }
    let url = resolve_shadow_url(&app, &url)?;
    validate_url(&url)?;
    let width = thumb_width(width_px)?;
    let bucket = (time_sec / BUCKET_SECONDS).round() as u32;

    let hint = render_hint(&app);
    let inner_arc = state.0.clone();
    let mut inner = inner_arc.lock().map_err(|_| "thumbs: poisoned")?;
    let answer = inner.request_thumb(url, bucket, width, hint, Instant::now());
    if let ThumbRequest::Pending { start_worker: true } = answer {
        let arc = inner_arc.clone();
        // A dedicated OS thread: every step (mpv FFI, settle polling, file
        // IO) is blocking, and exactly one capture runs at a time.
        std::thread::spawn(move || worker(arc));
    }
    Ok(answer.into())
}

/// What the web layer is told about one preview (`player_thumb`). The web asks
/// again while it is `Pending` and the position is still hovered, and never
/// again for this stream once it is `Never`: the shell, which runs the
/// shadow, is the one that knows whether a frame is coming, so no budget of
/// its own guesses it on the other side.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum ThumbAnswer {
    /// The frame, as a data URL.
    Frame {
        #[serde(rename = "dataUrl")]
        data_url: String,
    },
    /// Being made, or not readable yet (a region not downloaded, a shadow
    /// being replaced): ask again.
    Pending,
    /// This stream gets no previews, and why.
    Never { reason: String },
}

impl From<ThumbRequest> for ThumbAnswer {
    fn from(request: ThumbRequest) -> Self {
        match request {
            ThumbRequest::Hit(data_url) => ThumbAnswer::Frame { data_url },
            ThumbRequest::Pending { .. } => ThumbAnswer::Pending,
            ThumbRequest::Refused(reason) => ThumbAnswer::Never { reason },
        }
    }
}

/// What a thumbnail request finds.
#[derive(Debug, PartialEq, Eq)]
enum ThumbRequest {
    /// The frame is cached.
    Hit(String),
    /// Queued as the latest wanted bucket; `start_worker` when no worker is
    /// running to pick it up (the caller spawns one).
    Pending { start_worker: bool },
    /// This stream gets no previews ([`Inner::refuse`]), and why: nothing is
    /// queued.
    Refused(String),
}

impl Inner {
    /// The bookkeeping half of [`player_thumb`]: switch streams or widths when
    /// they changed, answer from the cache, else queue `bucket`.
    fn request_thumb(&mut self, url: String, bucket: u32, width: u32, hint: Option<Render>, now: Instant) -> ThumbRequest {
        if self.url.as_deref() != Some(url.as_str()) {
            self.switch_stream(Some(url), hint);
        } else {
            // Same stream: fill the hint if it was not known at the first
            // request (the player had not reported a frame yet), so a later
            // hover still seeds a shadow not opened so far. Never overwrite a
            // hint already set - the first shadow may be open on it.
            self.hint_render = self.hint_render.or(hint);
        }
        if self.width != width {
            // The preview changed size (window moved to a denser screen, zoom):
            // the shadow stays, the frames captured for the old size go.
            self.width = width;
            self.cache.clear();
            self.wanted = None;
        }
        if let Some(hit) = self.cache.get(bucket) {
            return ThumbRequest::Hit(hit.clone());
        }
        if let Some(reason) = &self.refused {
            return ThumbRequest::Refused(reason.clone());
        }
        self.wanted = Some(bucket);
        ThumbRequest::Pending { start_worker: self.claim_worker(now) }
    }

    /// Let go of the shadow, if any, without waiting for its teardown (this
    /// runs under the lock [`player_thumb`] needs; see [`retire`]).
    fn retire_shadow(&mut self) {
        if let Some(shadow) = self.shadow.take() {
            retire(shadow);
        }
    }

    /// Whether the caller should start a worker now (and if so, it is marked
    /// running). Not while one runs, and not while a lost shadow's pacing
    /// holds: the request stays queued and a later one starts it.
    fn claim_worker(&mut self, now: Instant) -> bool {
        if self.worker_running || (self.shadow.is_none() && !self.may_respawn(now)) {
            return false;
        }
        self.worker_running = true;
        true
    }

    /// A new shadow may be spawned for this stream: always, unless the last
    /// one was lost less than its own lifetime ago. A stream that cannot be
    /// read fails each shadow at the speed it fails, so the respawns are paced
    /// by that speed: one that refused at once is retried at once, one that
    /// hung for a minute first is not retried for a minute. No constant of
    /// ours guesses how long "not readable yet" lasts.
    fn may_respawn(&self, now: Instant) -> bool {
        self.lost.as_ref().map_or(true, |lost| now.saturating_duration_since(lost.at) >= lost.lived)
    }

    /// No correct thumbnail can be made for the current stream: stop its
    /// previews and its scene sweep, and say why, once (nothing retries until
    /// the stream changes). A preview in the wrong colours reads as a broken
    /// file; none at all is the honest answer.
    fn refuse(&mut self, reason: &str) {
        tracing::warn!(
            "thumbs: no previews for {}: {reason}",
            self.url.as_deref().unwrap_or("(no stream)")
        );
        self.refused = Some(reason.to_owned());
        self.wanted = None;
        self.retire_shadow();
    }

    /// The shadow could not read the stream (`reason`) after living `lived`:
    /// drop it, keep the stream's previews ON, and let a later request open a
    /// new shadow (paced by [`Inner::may_respawn`]). Said once per reason, so
    /// a stream that stays unreadable does not flood the log.
    fn lose_shadow(&mut self, reason: &str, lived: Duration, now: Instant) {
        if self.lost.as_ref().map(|lost| lost.reason.as_str()) != Some(reason) {
            tracing::warn!(
                "thumbs: the shadow for {} could not read it (a later request opens a new one): \
                 {reason}",
                self.url.as_deref().unwrap_or("(no stream)")
            );
        }
        self.lost = Some(LostShadow { at: now, lived, reason: reason.to_owned() });
        self.retire_shadow();
    }

    /// A new stream (or none): drop the old shadow (its decoder pipeline is
    /// for the old url) and everything known about the old stream with it.
    fn switch_stream(&mut self, url: Option<String>, hint: Option<Render>) {
        self.url = url;
        self.generation += 1;
        self.retire_shadow();
        self.cache.clear();
        self.wanted = None;
        self.scan = None;
        self.scan_requested = false;
        self.refused = None;
        self.lost = None;
        self.conversion = None;
        self.hint_render = hint;
    }

    /// Record what the frame just checked by [`checked_capture`] needs. When
    /// it differs from what the stream's frames needed before, every
    /// thumbnail captured under the old conversion goes (nothing captured
    /// under the old route is served afterwards), and so does the sweep's
    /// comparison sample, so no scene cut is claimed across the change.
    ///
    /// The sweep's progress is kept on purpose: it walks the file in order,
    /// so restarting it at every change would never finish on a file that
    /// alternates (each crossing of a segment boundary is a change). Its
    /// samples are luma grids of frames that were each checked before
    /// capture, so the ones already taken stay valid.
    fn note_conversion(&mut self, conversion: Conversion) {
        let url = self.url.as_deref().unwrap_or("(no stream)");
        if conversion_changed(self.conversion.as_ref(), &conversion) {
            tracing::info!(
                "thumbs: {url} changed from {} to {conversion}: its cached previews are dropped",
                self.conversion.as_ref().expect("a change has a before"),
            );
            self.cache.clear();
            if let Some(scan) = self.scan.as_mut() {
                scan.prev_luma = None;
            }
        } else if self.conversion.is_none() {
            tracing::info!("thumbs: {conversion} ({url})");
        }
        self.conversion = Some(conversion);
    }

    /// Put the shadow back after a [`checked_capture`] and record what the
    /// frame needed. Returns what was captured, when anything was.
    fn take_back<T>(&mut self, captured: Captured<T>, now: Instant) -> TakenBack<T> {
        match captured {
            Captured::Frame(shadow, conversion, value) => {
                self.note_conversion(conversion);
                self.shadow = Some(shadow);
                self.lost = None;
                TakenBack::Frame(value)
            }
            Captured::Yielded(shadow) => {
                self.shadow = Some(shadow);
                TakenBack::Yielded
            }
            Captured::Parked => TakenBack::Skipped,
            Captured::Lost { reason, lived } => {
                self.lose_shadow(&reason, lived, now);
                TakenBack::Lost
            }
            Captured::Skipped(shadow) => {
                self.shadow = Some(shadow);
                TakenBack::Skipped
            }
            Captured::Replaced(shadow, conversion) => {
                self.note_conversion(conversion);
                self.shadow = Some(shadow);
                TakenBack::Replaced
            }
            Captured::Refused(reason) => {
                self.refuse(&reason);
                TakenBack::Refused
            }
        }
    }
}

/// What [`Inner::take_back`] leaves the worker.
enum TakenBack<T> {
    Frame(T),
    /// Nothing captured, the shadow unchanged.
    Skipped,
    /// Nothing captured, a new shadow in place (the same target must be
    /// sampled again on it).
    Replaced,
    /// The stream gets no previews; the worker ends.
    Refused,
    /// The shadow was lost; the worker ends, a later request starts one.
    Lost,
    /// A sweep sample gave way to a hover; nothing captured.
    Yielded,
}

/// Tear the shadow down (player unmount / playback stop). Frees the decoder
/// and closes the shadow's connection to the streaming server.
#[tauri::command]
pub async fn player_thumb_stop(state: State<'_, ThumbsState>) -> Result<(), String> {
    let mut inner = state.0.lock().map_err(|_| "thumbs: poisoned")?;
    inner.switch_stream(None, None);
    Ok(())
}

/// The renderer the live player's frames call for, for the first shadow of this
/// stream ([`initial_render`]). Read from what the player already decoded; the
/// profile is not needed (a hint never refuses). `None` when no player is up or
/// it has reported no frame yet.
fn render_hint(app: &tauri::AppHandle) -> Option<Render> {
    initial_render(crate::shell::player_video_params(app).as_deref(), None)
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
    let hint = render_hint(&app);
    let inner_arc = state.0.clone();
    let mut inner = inner_arc.lock().map_err(|_| "thumbs: poisoned")?;
    if inner.url.as_deref() != Some(url.as_str()) {
        inner.switch_stream(Some(url), hint);
    } else {
        inner.hint_render = inner.hint_render.or(hint);
    }
    if inner.refused.is_some() {
        return Ok(Some(Vec::new()));
    }
    if let Some(result) = inner.scan.as_ref().and_then(|scan| scan.result.clone()) {
        return Ok(Some(result));
    }
    inner.scan_requested = true;
    if inner.claim_worker(Instant::now()) {
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
        let (job, url, generation, shadow_missing, hint_render) = {
            let mut inner = match inner_arc.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            let job = if let Some(bucket) = inner.wanted.take() {
                if inner.cache.contains(bucket) {
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
            if inner.shadow.is_none() && !inner.may_respawn(Instant::now()) {
                // A lost shadow's pacing holds: the job stays queued and the
                // next request after it starts a worker again.
                if let Job::Thumb(bucket, _) = job {
                    inner.wanted.get_or_insert(bucket);
                }
                inner.worker_running = false;
                return;
            }
            (job, url, inner.generation, inner.shadow.is_none(), inner.hint_render)
        };

        if shadow_missing {
            // Born at the size its first job needs, so a spawn is never
            // followed straight away by a filter rebuild.
            let width = match job {
                Job::Thumb(_, width) => width,
                Job::ScanStep => SCAN_WIDTH,
            };
            // Open on the renderer the player's frames call for, when it told
            // us one; otherwise the software path, which `checked_capture`
            // moves to libplacebo when a frame mpv reports needs it. The hint
            // only saves the open-then-respawn; the per-frame check still rules.
            let render = hint_render.unwrap_or(Render::Software);
            match Shadow::spawn(&url, width, render) {
                Ok(shadow) => {
                    let mut inner = match inner_arc.lock() {
                        Ok(guard) => guard,
                        Err(_) => return,
                    };
                    if inner.generation != generation {
                        // Url changed while spawning; the shadow is for the old
                        // stream. Retire it and loop for the new state.
                        retire(shadow);
                        continue;
                    }
                    inner.shadow = Some(shadow);
                }
                Err(e) => {
                    let mut inner = match inner_arc.lock() {
                        Ok(guard) => guard,
                        Err(_) => return,
                    };
                    if inner.generation == generation {
                        inner.refuse(&format!("thumbs: shadow spawn failed: {e}"));
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
                // Nothing is captured from a frame this shadow does not render
                // correctly: a thumbnail in the wrong colours is worse than none.
                let captured = match shadow.set_width(width) {
                    Ok(()) => checked_capture(
                        shadow,
                        &url,
                        bucket as f64 * BUCKET_SECONDS,
                        // A hover never gives way: it IS what the sweep yields to.
                        &|| false,
                        Shadow::capture,
                    ),
                    Err(e) => {
                        tracing::debug!("thumbs: bucket {bucket}: {e}");
                        Captured::Skipped(shadow)
                    }
                };

                let mut inner = match inner_arc.lock() {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
                if inner.generation != generation {
                    // Url changed mid-capture: the shadow and the frame belong
                    // to the old stream; drop the frame, retire the shadow.
                    if let Some(shadow) = captured.into_shadow() {
                        retire(shadow);
                    }
                    continue;
                }
                match inner.take_back(captured, Instant::now()) {
                    // The preview changed size mid-capture: this frame is the
                    // old size, and the cache now holds the new one only.
                    TakenBack::Frame(Ok(_)) if inner.width != width => {}
                    TakenBack::Frame(Ok(data_url)) => inner.cache.insert(bucket, data_url),
                    TakenBack::Frame(Err(e)) => tracing::debug!("thumbs: bucket {bucket}: {e}"),
                    TakenBack::Skipped | TakenBack::Yielded => tracing::debug!(
                        "thumbs: bucket {bucket}: no frame to judge (seek did not settle, region \
                         not downloaded yet?)"
                    ),
                    TakenBack::Replaced => {
                        tracing::debug!("thumbs: bucket {bucket}: renderer replaced, asked again")
                    }
                    TakenBack::Refused | TakenBack::Lost => {
                        inner.worker_running = false;
                        return;
                    }
                }
            }
            Job::ScanStep => {
                // A sweep sample gives way to any hover that is waiting.
                let hover_waiting = || inner_arc.lock().map_or(true, |inner| inner.wanted.is_some());
                // Snapshot the scan cursor under the lock. The sweep is laid
                // out from the file's duration, so until the shadow has the
                // file open there is no cursor yet.
                let step = {
                    let mut inner = match inner_arc.lock() {
                        Ok(guard) => guard,
                        Err(_) => return,
                    };
                    if inner.generation != generation {
                        retire(shadow);
                        continue;
                    }
                    if inner.scan.is_none() {
                        if let Some(duration) = shadow.duration() {
                            inner.scan = Some(Scan::new(duration));
                        }
                    }
                    inner.scan.as_ref().map(|scan| (scan.next, scan.spacing))
                };
                let Some((index, spacing)) = step else {
                    // Not open yet. Wait for it OUTSIDE the lock, through the
                    // same checked path a capture takes: it waits as long as
                    // a seek may (or until a hover comes), and it is what
                    // notices a load mpv ended, which a bare poll of the
                    // duration never would (a dead shadow then spun here for
                    // the rest of the video, measured 2026-10-28).
                    let opened = checked_capture(shadow, &url, 0.0, &hover_waiting, |_| ());
                    let mut inner = match inner_arc.lock() {
                        Ok(guard) => guard,
                        Err(_) => return,
                    };
                    if inner.generation != generation {
                        if let Some(shadow) = opened.into_shadow() {
                            retire(shadow);
                        }
                        continue;
                    }
                    if let TakenBack::Refused | TakenBack::Lost = inner.take_back(opened, Instant::now()) {
                        inner.worker_running = false;
                        return;
                    }
                    continue;
                };

                let target = index as f64 * spacing;
                // The sweep samples at its own small width whatever size the
                // hover thumbnails are being cut at, through the same checked
                // path, so it compares converted (SDR) frames only.
                let captured = match shadow.set_width(SCAN_WIDTH) {
                    Ok(()) => checked_capture(shadow, &url, target, &hover_waiting, |shadow| {
                        shadow.capture_bytes().ok().and_then(|bytes| luma_grid(&bytes).ok())
                    }),
                    Err(e) => {
                        tracing::warn!("thumbs: scene sweep sample {index}: {e}");
                        Captured::Skipped(shadow)
                    }
                };

                let mut inner = match inner_arc.lock() {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
                if inner.generation != generation {
                    if let Some(shadow) = captured.into_shadow() {
                        retire(shadow);
                    }
                    continue;
                }
                let luma = match inner.take_back(captured, Instant::now()) {
                    TakenBack::Frame(luma) => luma,
                    // An unsettled seek (undownloaded torrent region) skips the
                    // sample rather than stalling the sweep; prev resets below
                    // so no false cut spans the hole.
                    TakenBack::Skipped => None,
                    // A new renderer, or a hover went first: the same sample
                    // is taken again later.
                    TakenBack::Replaced | TakenBack::Yielded => continue,
                    TakenBack::Refused | TakenBack::Lost => {
                        inner.worker_running = false;
                        return;
                    }
                };
                if let Some(scan) = inner.scan.as_mut() {
                    // The live comparison sample, not the snapshot: a change of
                    // conversion noted by take_back has just dropped it.
                    if let (Some(prev), Some(current)) =
                        (scan.prev_luma.as_deref(), luma.as_deref())
                    {
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
