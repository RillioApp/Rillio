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
    /// After a failed shadow spawn (no dll / broken stream), stop retrying for
    /// this url; every later request would fail the same way.
    disabled: bool,
    /// What the stream's last checked frame needed ([`Inner::note_conversion`]):
    /// a frame that needs something else invalidates the cached previews.
    conversion: Option<Conversion>,
}

struct Shadow {
    mpv: Mpv,
    /// The width the `vf=scale` chain currently outputs.
    width: u32,
    /// How this shadow's screenshots are rendered. Nothing is captured from a
    /// frame whose conversion this does not serve ([`checked_capture`]).
    render: Render,
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
}

/// THE one path to a capture, hover thumbnail or sweep sample alike: seek to
/// `target`, decide from what mpv reports for the frame landed on, and run
/// `capture` only when this shadow renders that conversion. Decided per frame,
/// not once per stream: a stream can change colour along the way (an SDR
/// segment spliced before an HDR or Dolby Vision one), and a name that must
/// be refused can appear anywhere.
fn checked_capture<T>(
    shadow: Shadow,
    url: &str,
    target: f64,
    capture: impl FnOnce(&Shadow) -> T,
) -> Captured<T> {
    if !shadow.seek_settled(target) {
        // mpv's own verdict still counts: a libplacebo load that mpv ended in
        // error never settles a seek, and must not time out in silence.
        return match shadow.check_load() {
            Err(reason) => Captured::Refused(reason),
            Ok(()) => Captured::Skipped(shadow),
        };
    }
    match step_for(shadow.frame_conversion(), shadow.render) {
        Step::Capture(conversion) => {
            let captured = capture(&shadow);
            Captured::Frame(shadow, conversion, captured)
        }
        Step::NotYet => Captured::Skipped(shadow),
        Step::Refuse(reason) => Captured::Refused(reason),
        Step::Respawn(conversion) => {
            let (width, render) = (shadow.width, conversion.render());
            // The old shadow's decoder and connection go before the new one
            // opens.
            drop(shadow);
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

impl Shadow {
    /// A muted mpv over `url` whose decode chain outputs `width`, rendering its
    /// screenshots as `render` says. Options follow Harbor's shadow flags;
    /// `hr-seek=no` + keyframe seeks are the speed. [`checked_capture`]
    /// confirms each frame wants this `render` before it is captured.
    fn spawn(url: &str, width: u32, render: Render) -> Result<Self, String> {
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
                // mpv's own error lines, so a refusal can say why gpu-next
                // failed ([`Shadow::check_load`]). Diagnostics only.
                if let Err(e) = mpv.request_log_messages("error") {
                    tracing::debug!("thumbs: mpv error log unavailable: {e}");
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
        Ok(Self {
            mpv,
            width,
            render,
            mpv_errors: std::cell::RefCell::new(Vec::new()),
            _window: window,
        })
    }

    /// What the frame mpv holds right now needs ([`conversion_for`] over what
    /// mpv reports for it), read without waiting: callers ask once a seek has
    /// settled, when the frame and its parameters are already there.
    /// `Ok(None)` while there is no decoded frame yet, or (libplacebo) while
    /// the VO is still configuring: slow, not broken (seen with the player
    /// decoding 4K and the subtitle model loading at once), so the job is
    /// skipped and the next one asks again. `Err` when mpv ended the load in
    /// error ([`Shadow::check_load`]) or the frame cannot be converted.
    fn frame_conversion(&self) -> Result<Option<Conversion>, String> {
        self.check_load()?;
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

    /// Drain the shadow's events (nothing else reads them). For a libplacebo
    /// shadow, a load mpv ended in error is final, and the `Err` carries mpv's
    /// reason and its own error lines: the same stream just opened fine in
    /// software, so what failed is the renderer (gpu-next could not start).
    /// A software shadow keeps today's rule, a dead stream simply never shows
    /// a frame and every job times out.
    fn check_load(&self) -> Result<(), String> {
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
                mpv::MpvEvent::EndFile { reason, error }
                    if reason == mpv::MPV_END_FILE_REASON_ERROR
                        && self.render == Render::Libplacebo =>
                {
                    return Err(format!(
                        "thumbs: gpu-next could not show the stream ({}); mpv said: {}",
                        self.mpv.error_string(error),
                        self.mpv_errors.borrow().join("; ")
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

    /// Keyframe-seek to `target` and wait for the seek to settle. `false` on
    /// timeout (undownloaded torrent region / dead stream) - do not capture.
    /// mpv refuses a seek until the file is open, which is every first job on
    /// a fresh shadow (and every first job after a renderer change): the seek
    /// is asked again within the same budget instead of failing at once.
    fn seek_settled(&self, target: f64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(SEEK_SETTLE_MS);
        while self.mpv.command(&["seek", &format!("{target:.3}"), "absolute+keyframes"]).is_err() {
            // No retry starts after the budget: the sleep is capped to what
            // is left, and the deadline is checked again before the next ask.
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            std::thread::sleep(SETTLE_POLL.min(left));
            if Instant::now() >= deadline {
                return false;
            }
        }
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
        let mut inner = Inner::default();
        assert_eq!(
            inner.request_thumb(url.clone(), 7, 448),
            ThumbRequest::Pending { start_worker: true }
        );
        // The worker refuses (e.g. libplacebo could not start), then exits.
        inner.refuse("thumbs: Dolby Vision profile 5 needs libplacebo, which could not start");
        inner.worker_running = false;
        assert!(inner.disabled && inner.wanted.is_none() && inner.shadow.is_none());
        for bucket in [7, 8, 9] {
            assert_eq!(inner.request_thumb(url.clone(), bucket, 448), ThumbRequest::Refused);
        }
        assert!(inner.wanted.is_none() && !inner.worker_running);
        // Another stream is judged on its own.
        assert_eq!(
            inner.request_thumb("http://127.0.0.1:11470/sdr".to_owned(), 7, 448),
            ThumbRequest::Pending { start_worker: true }
        );
        assert!(!inner.disabled);
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
        inner.switch_stream(Some("http://127.0.0.1:11470/spliced".to_owned()));
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
        inner.switch_stream(None);
        assert_eq!(inner.conversion, None);
    }

    /// A refusal met at capture time refuses the stream, as one found at open.
    #[test]
    fn a_refusal_at_capture_time_stops_the_stream() {
        let mut inner = Inner::default();
        inner.switch_stream(Some("http://127.0.0.1:11470/spliced".to_owned()));
        inner.wanted = Some(3);
        let back = inner.take_back::<()>(Captured::Refused("thumbs: profile 5 without RPU".into()));
        assert!(matches!(back, TakenBack::Refused));
        assert!(inner.disabled && inner.wanted.is_none());
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
        inner.switch_stream(Some(file.clone()));
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
                match checked_capture(shadow.take().unwrap(), &file, time, Shadow::capture_bytes) {
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
                    Captured::Skipped(s) => shadow = Some(s),
                    Captured::Refused(reason) => panic!("{step}: refused: {reason}"),
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
    /// queue is empty (Codex pass 2). The producer posts until the window is
    /// gone (a full queue only makes a post fail, so failures are ignored),
    /// so it cannot outlive the drop.
    #[cfg(windows)]
    #[test]
    fn the_hidden_window_stops_while_its_queue_is_being_refilled() {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, PostMessageW, WM_NULL};

        const DROP_BUDGET: Duration = Duration::from_secs(5);
        /// Enough posters to outpace one pump, so the queue never empties.
        const PRODUCERS: usize = 4;
        let window = HiddenWindow::create().expect("hidden window");
        let wid = window.wid();
        let producers: Vec<_> = (0..PRODUCERS)
            .map(|_| {
                std::thread::spawn(move || {
                    let hwnd = HWND(wid as *mut _);
                    while unsafe { IsWindow(Some(hwnd)) }.as_bool() {
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
        assert!(dropped.recv_timeout(DROP_BUDGET).is_ok(), "the drop hung behind a refilled queue");
        assert!(!unsafe { IsWindow(Some(HWND(wid as *mut _))) }.as_bool());
        for producer in producers {
            producer.join().expect("producer");
        }
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
            match checked_capture(shadow, file, time, Shadow::capture_bytes) {
                Captured::Frame(done, conversion, jpeg) => {
                    return (done, conversion, jpeg.expect("capture"));
                }
                Captured::Skipped(again) | Captured::Replaced(again, _) => {
                    assert!(Instant::now() < deadline, "{file} at {time:.1}s never captured");
                    shadow = again;
                }
                Captured::Refused(reason) => panic!("{file} refused: {reason}"),
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

    /// A libplacebo shadow whose load mpv ends in error is refused with mpv's
    /// own reason, never left to time out job after job in silence. Driven
    /// here by a file that does not exist, the same END_FILE error a gpu-next
    /// that cannot start produces (measured: "no audio or video data played"
    /// after "Error opening/initializing the selected video_out"). Needs
    /// `libmpv-2.dll` (next to the test binary or via `RILLIO_LIBMPV`).
    /// Run: `cargo test --lib thumbs::tests::libplacebo_load -- --ignored`.
    #[test]
    #[ignore = "needs libmpv-2.dll"]
    fn libplacebo_load_failure_is_refused_with_mpvs_reason() {
        let missing = std::env::temp_dir().join("rillio-thumbs-no-such-file.mkv");
        assert!(!missing.exists());
        let missing = missing.to_string_lossy().to_string();
        // Through the worker's own path: the seek never settles, and the
        // refusal still comes out of it.
        let shadow = Shadow::spawn(&missing, 448, Render::Libplacebo).expect("shadow spawn");
        let err = match checked_capture(shadow, &missing, 0.0, |_| ()) {
            Captured::Refused(e) => e,
            Captured::Frame(..) | Captured::Skipped(_) | Captured::Replaced(..) => {
                panic!("a missing file was not refused")
            }
        };
        assert!(err.contains("gpu-next could not show the stream"), "{err}");
        // The software shadow keeps its old rule: no frame, no verdict, the job
        // is skipped.
        let software = Shadow::spawn(&missing, 448, Render::Software).expect("shadow spawn");
        assert!(matches!(checked_capture(software, &missing, 0.0, |_| ()), Captured::Skipped(_)));
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
                assert!(before.seek_settled(time), "seek to {time:.1}s");
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
    match inner.request_thumb(url, bucket, width) {
        ThumbRequest::Hit(data_url) => Ok(Some(data_url)),
        ThumbRequest::Pending { start_worker } => {
            if start_worker {
                let arc = inner_arc.clone();
                // A dedicated OS thread: every step (mpv FFI, settle polling,
                // file IO) is blocking, and exactly one capture runs at a time.
                std::thread::spawn(move || worker(arc));
            }
            Ok(None)
        }
        ThumbRequest::Refused => Ok(None),
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
    /// This stream gets no previews ([`Inner::refuse`]): nothing is queued.
    Refused,
}

impl Inner {
    /// The bookkeeping half of [`player_thumb`]: switch streams or widths when
    /// they changed, answer from the cache, else queue `bucket`.
    fn request_thumb(&mut self, url: String, bucket: u32, width: u32) -> ThumbRequest {
        if self.url.as_deref() != Some(url.as_str()) {
            self.switch_stream(Some(url));
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
        if self.disabled {
            return ThumbRequest::Refused;
        }
        self.wanted = Some(bucket);
        let start_worker = !self.worker_running;
        self.worker_running = true;
        ThumbRequest::Pending { start_worker }
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
        self.disabled = true;
        self.wanted = None;
        self.shadow = None;
    }

    /// A new stream (or none): drop the old shadow (its decoder pipeline is
    /// for the old url) and everything known about the old stream with it.
    fn switch_stream(&mut self, url: Option<String>) {
        self.url = url;
        self.generation += 1;
        self.shadow = None;
        self.cache.clear();
        self.wanted = None;
        self.scan = None;
        self.scan_requested = false;
        self.disabled = false;
        self.conversion = None;
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
    fn take_back<T>(&mut self, captured: Captured<T>) -> TakenBack<T> {
        match captured {
            Captured::Frame(shadow, conversion, value) => {
                self.note_conversion(conversion);
                self.shadow = Some(shadow);
                TakenBack::Frame(value)
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
}

/// Tear the shadow down (player unmount / playback stop). Frees the decoder
/// and closes the shadow's connection to the streaming server.
#[tauri::command]
pub async fn player_thumb_stop(state: State<'_, ThumbsState>) -> Result<(), String> {
    let mut inner = state.0.lock().map_err(|_| "thumbs: poisoned")?;
    inner.switch_stream(None);
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
        inner.switch_stream(Some(url));
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
            (job, url, inner.generation, inner.shadow.is_none())
        };

        if shadow_missing {
            // Born at the size its first job needs, so a spawn is never
            // followed straight away by a filter rebuild.
            let width = match job {
                Job::Thumb(_, width) => width,
                Job::ScanStep => SCAN_WIDTH,
            };
            // Every stream starts on the software path; `checked_capture` moves
            // it to libplacebo when a frame mpv reports needs it.
            match Shadow::spawn(&url, width, Render::Software) {
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
                    // to the old stream; drop both.
                    continue;
                }
                match inner.take_back(captured) {
                    // The preview changed size mid-capture: this frame is the
                    // old size, and the cache now holds the new one only.
                    TakenBack::Frame(Ok(_)) if inner.width != width => {}
                    TakenBack::Frame(Ok(data_url)) => inner.cache.insert(bucket, data_url),
                    TakenBack::Frame(Err(e)) => tracing::debug!("thumbs: bucket {bucket}: {e}"),
                    TakenBack::Skipped => tracing::debug!(
                        "thumbs: bucket {bucket}: no frame to judge (seek did not settle, region \
                         not downloaded yet?)"
                    ),
                    TakenBack::Replaced => {
                        tracing::debug!("thumbs: bucket {bucket}: renderer replaced, asked again")
                    }
                    TakenBack::Refused => {
                        inner.worker_running = false;
                        return;
                    }
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
                    (scan.next, scan.spacing)
                };
                let (index, spacing) = step;

                let target = index as f64 * spacing;
                // The sweep samples at its own small width whatever size the
                // hover thumbnails are being cut at, through the same checked
                // path, so it compares converted (SDR) frames only.
                let captured = match shadow.set_width(SCAN_WIDTH) {
                    Ok(()) => checked_capture(shadow, &url, target, |shadow| {
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
                    continue;
                }
                let luma = match inner.take_back(captured) {
                    TakenBack::Frame(luma) => luma,
                    // An unsettled seek (undownloaded torrent region) skips the
                    // sample rather than stalling the sweep; prev resets below
                    // so no false cut spans the hole.
                    TakenBack::Skipped => None,
                    // A new renderer: the same sample is taken again on it.
                    TakenBack::Replaced => continue,
                    TakenBack::Refused => {
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
