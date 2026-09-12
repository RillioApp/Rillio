//! S3 part 3 - the shell ⇄ web-client player bridge.
//!
//! The web client (`packages/video/src/ShellVideo`) drives a native mpv over a
//! tiny IPC: it sends `mpv-command` / `mpv-set-prop` / `mpv-observe-prop` and
//! listens for `mpv-prop-change` / `mpv-event-ended`. On desktop we carry that
//! IPC over Tauri: [`shell_send`] receives the outbound messages and drives
//! libmpv; a background event-loop thread emits the inbound ones as the Tauri
//! event `shell-signal`.
//!
//! Because mpv is a native HTTP client it fetches the streaming server's
//! `/{infohash}/{idx}` URL directly - no browser CORS/Private-Network preflight,
//! no HTML5 codec gate. That both starts the torrent download (our stream route
//! auto-creates the torrent on first byte-range) and decodes every codec
//! (HEVC/HDR/10-bit) that WebView2 cannot.
//!
//! Rendering note: this first cut lets mpv open its own output window (no `wid`
//! embedding) so the always-visible web UI keeps driving playback. Compositing
//! mpv *behind* a transparent WebView (single-window UX) is the S3 part-4
//! follow-up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::mpv::{self, Mpv, MpvEvent};

/// Lazily-created native player. Held in Tauri state; `None` until the web
/// client first talks to the shell (on mount, via [`shell_init`]).
#[derive(Default)]
pub struct ShellState(Mutex<Option<NativePlayer>>);

/// Android: a weak handle to the live mpv Controller so the Surface bridge can
/// re-attach the SurfaceView after init / on every surfaceCreated (see
/// `Controller::attach_surface`). Weak so a dropped player doesn't leak.
#[cfg(target_os = "android")]
static ANDROID_PLAYER: Mutex<Option<std::sync::Weak<Controller>>> = Mutex::new(None);

/// Whether the main window was maximized when it last entered fullscreen.
///
/// tao's undecorated-window `WM_NCCALCSIZE` handler insets the client area while
/// a borderless window is maximized (so a maximized frameless window keeps the
/// taskbar edge reachable and does not spill off the monitor). Entering
/// fullscreen with `WS_MAXIMIZE` still set carries that inset into fullscreen, so
/// the video + overlays stop at the taskbar edge while the frame reaches the full
/// monitor (tao#1087, tauri#11788). We unmaximize before going fullscreen and
/// restore the maximized state on exit; this remembers it across the toggle.
#[cfg(desktop)]
static WAS_MAXIMIZED_BEFORE_FS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Android JNI-callback entry point (called from `surface::android` on
/// surfaceCreated): attach the current video Surface to the live player, if any.
/// No-op before the player exists - `ensure` attaches the stored surface then.
#[cfg(target_os = "android")]
pub fn android_reattach_surface() {
    let Some(wid) = crate::surface::android::current_wid() else { return };
    let ctrl = ANDROID_PLAYER.lock().unwrap().as_ref().and_then(|w| w.upgrade());
    if let Some(ctrl) = ctrl {
        ctrl.attach_surface(wid);
    }
}

/// The native player, chosen per device (the factory in [`ShellState::ensure`]
/// picks the variant at runtime). Windows = embedded libmpv; Android [Phase 2]
/// = a Media3 bridge forwarding to a Kotlin plugin. The web bridge
/// ([`shell_send`]) and the direct commands drive playback through this enum, so
/// no calling code names a concrete backend. Cheap to clone (each variant holds
/// an `Arc`). This is the Rust-idiomatic form of TropxMotion's transport factory
/// for a closed backend set: runtime selection, one interface.
#[derive(Clone)]
pub enum NativePlayer {
    Mpv(Arc<Controller>),
}

impl NativePlayer {
    /// An outbound `mpv-command` argv (already allowlist-checked by the caller).
    fn command(&self, refs: &[&str]) -> Result<(), String> {
        match self {
            NativePlayer::Mpv(ctrl) => ctrl.run_command(refs),
        }
    }
    fn set_property(&self, name: &str, value: &str) -> Result<(), String> {
        match self {
            NativePlayer::Mpv(ctrl) => ctrl.mpv.set_property(name, value),
        }
    }
    fn observe_property(&self, name: &str) -> Result<(), String> {
        match self {
            NativePlayer::Mpv(ctrl) => ctrl.mpv.observe_property(name),
        }
    }
    /// Whether this player can read `rillio://` in-process (see
    /// [`register_stream_protocol`]).
    fn stream_cb(&self) -> bool {
        match self {
            NativePlayer::Mpv(ctrl) => ctrl.stream_cb,
        }
    }
    /// Snapshot of the last-known player properties (the stats panel).
    fn stats(&self) -> serde_json::Map<String, Value> {
        match self {
            NativePlayer::Mpv(ctrl) => ctrl.props.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }
    }
    /// A JPEG data-URL of the current frame for the panel backdrop. Blocking.
    /// Capability: only where the player renders behind the WebView (embed).
    fn snapshot_blocking(&self) -> Result<String, String> {
        match self {
            NativePlayer::Mpv(ctrl) => {
                if let Some(cached) = ctrl.recent_snapshot() {
                    return Ok(cached);
                }
                capture_snapshot(ctrl, &snapshot_temp_path())
            }
        }
    }
    /// Blur the live video under the given panel rects. Capability: gpu_blur.
    fn blur_rect(
        &self,
        app: &AppHandle,
        rects: Vec<BlurRect>,
        viewport: BlurViewport,
    ) -> Result<(), String> {
        match self {
            NativePlayer::Mpv(ctrl) => blur_rect_mpv(ctrl, app, rects, viewport),
        }
    }
}

/// A live mpv instance plus a cache of its latest property values.
pub struct Controller {
    mpv: Arc<Mpv>,
    /// Whether `rillio://` is registered on this instance, i.e. whether mpv can
    /// read torrent bytes in-process. False on a libmpv without
    /// `mpv_stream_cb_add_ro`; the web client then keeps loading the HTTP URL.
    stream_cb: bool,
    /// Latest value of every observed mpv property, for the "Stats for nerds"
    /// panel (`shell_mpv_stats`). Updated by the event loop on every change.
    props: Mutex<serde_json::Map<String, Value>>,
    /// Last video-frame snapshot handed to the web layer, for the rate limit in
    /// [`player_snapshot`].
    snapshot: Mutex<SnapshotCache>,
    /// GPU panel-blur shader state (see [`player_blur_rect`]).
    blur: Mutex<BlurState>,
}

/// The most recent [`player_snapshot`] result and when it was produced.
#[derive(Default)]
struct SnapshotCache {
    data_url: Option<String>,
    at: Option<Instant>,
}

/// Extra mpv properties (beyond what ShellVideo observes) that the stats panel
/// wants: codecs, bitrates, fps, hardware-decode status, container, geometry.
const STATS_PROPS: &[&str] = &[
    "video-codec",
    "video-format",
    "audio-codec",
    "audio-codec-name",
    "audio-params",
    "container-fps",
    "estimated-vf-fps",
    "video-bitrate",
    "audio-bitrate",
    "hwdec-current",
    "file-format",
    "width",
    "height",
];

/// Properties the shell observes for its OWN geometry maths - neither the stats
/// panel nor the web player reads them. `osd-dimensions` carries the video
/// rectangle inside the mpv window, which is what maps a web panel's rect into
/// the blur shader's OUTPUT coordinates (see [`blur_shader_opts`]).
const GEOMETRY_PROPS: &[&str] = &["osd-dimensions"];

/// High-frequency props whose forwarding to the web UI is rate-limited.
const HF_PROPS: &[&str] = &["time-pos", "demuxer-cache-time"];
/// Minimum spacing between forwarded high-frequency updates (~5/s).
const HF_INTERVAL: Duration = Duration::from_millis(200);

/// Minimum spacing between real video-frame captures ([`player_snapshot`]).
/// Anything sooner is served the previous frame instead: the web layer polls at
/// ~3/s while a panel is open, so this is a cheap guard against a caller (or a
/// future one) asking for a fresh screenshot per animation frame.
const SNAPSHOT_MIN_INTERVAL: Duration = Duration::from_millis(200);
/// Longest edge of the snapshot handed to the web layer. It is only ever shown
/// blurred behind a panel's dark glass, so detail is destroyed anyway: keep it
/// small and the base64 payload with it (~a few KB per frame).
const SNAPSHOT_MAX_WIDTH: u32 = 320;
/// JPEG quality for the same reason: blur hides the artifacts.
const SNAPSHOT_JPEG_QUALITY: u8 = 60;

/// The GLSL user shader that blurs the video under the player's panels. EMBEDDED
/// rather than bundled as a Tauri resource: mpv's `glsl-shaders` takes FILE
/// PATHS, so it has to reach disk either way, and writing it out ourselves keeps
/// `cargo run` and the NSIS bundle on exactly one code path with no
/// resource-resolution difference between them.
const BLUR_SHADER: &str = include_str!("shaders/panel-blur.glsl");

/// The most rects [`player_blur_rect`] accepts. Matches `MAX_RECTS` in the
/// shader, which holds four sets of `r<i>{x,y,w,h}` parameters.
const MAX_BLUR_RECTS: usize = 4;

/// Gaussian radius for the panel blur, in CSS px (scaled to output pixels along
/// with everything else). Matches the `blur-[24px]` the web fallback uses, so
/// flipping the flag changes where the blur comes from, not how strong it looks.
/// A shell constant, never a web argument: the web chooses WHERE to blur, the
/// shell chooses HOW MUCH.
// 64, not the web fallback's 24: the shader blurs at QUARTER resolution (the
// standard frosted-glass move - single-res gaussians read as haze, not frost),
// and behind the panels' dark glass a 24px gaussian was invisible in practice.
const BLUR_RADIUS_CSS_PX: f64 = 64.0;

/// Minimum spacing between `glsl-shader-opts` writes. A panel open/close is one
/// call, but a window drag-resize re-measures every frame; this throttles
/// leading+trailing (never dropping the final state) so a drag cannot spam mpv.
const BLUR_MIN_INTERVAL: Duration = Duration::from_millis(16);

/// How long to wait before re-applying the params once the shader is first
/// loaded. mpv DROPS `glsl-shader-opts` values for parameters that are not
/// registered yet (mpv#12039, closed as not-planned), and registration only
/// happens when the VO thread gets around to parsing the file we just handed it -
/// long after our `set_property` returned. One deferred re-apply closes that
/// window; it is a race with an upstream ordering quirk, not a fallback.
const BLUR_PARAM_SETTLE: Duration = Duration::from_millis(250);

/// One panel's bounds, in CSS px relative to the WebView viewport. `corner` is
/// that panel's own border radius, also in CSS px: it is per-panel rather than
/// one shared setting because the menus are rounded while the side drawer sits
/// flush against the window edge with square corners.
#[derive(Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct BlurRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    corner: f64,
}

/// The WebView viewport, in CSS px (`innerWidth`/`innerHeight`).
#[derive(Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct BlurViewport {
    width: f64,
    height: f64,
}

/// The blur state the web layer last asked for.
#[derive(Clone, PartialEq, Debug)]
struct BlurRequest {
    rects: Vec<BlurRect>,
    viewport: BlurViewport,
}

#[derive(Default)]
struct BlurState {
    /// Whether the shader has been written to disk and handed to mpv. Latches on
    /// the first call and never clears: see [`player_blur_rect`] for why.
    loaded: bool,
    /// When the last `glsl-shader-opts` write went out (the throttle's clock).
    last_applied: Option<Instant>,
    /// The latest state asked for. Never consumed, only overwritten: a trailing
    /// apply and the post-load re-apply both need the CURRENT state, not a queue.
    desired: Option<BlurRequest>,
    /// Whether a trailing apply is already scheduled and will read `desired`.
    trailing: bool,
    /// The last option string written, to skip redundant writes. `None` forces
    /// the next apply through (used after a load, when mpv's parameters are new).
    last_opts: Option<String>,
}

/// SECURITY (S1): the ONLY mpv `command`s the web player is allowed to run over
/// the bridge. mpv's command set also includes `run`, `subprocess` and
/// `load-script` (arbitrary local code execution) and `loadlist`/`set` (which
/// can reach dangerous properties), so any of those reaching mpv from web
/// content would be XSS -> RCE. The WebView renders remote, addon-driven content
/// and `withGlobalTauri` exposes `invoke` globally, so this list must be exactly
/// what `packages/video/src/ShellVideo/ShellVideo.js` actually sends: only
/// `stop` and `loadfile` (grep it before widening this).
const MPV_COMMAND_ALLOWLIST: &[&str] = &["loadfile", "stop"];

/// SECURITY (S5): the ONLY mpv properties the web player is allowed to `set`
/// over the bridge. Enumerated from every `mpv-set-prop` in ShellVideo.js:
/// transport, audio, track selection, decode/output selection, aspect/scaling
/// and subtitle styling. mpv exposes many other settable properties (scripts,
/// stream-record, screenshot paths, ...) that must never be reachable from web
/// content. `start` is applied by the shell itself (loadfile handling below),
/// not over the bridge, so it is deliberately absent here.
const MPV_SETPROP_ALLOWLIST: &[&str] = &[
    // transport / playback
    "pause",
    "speed",
    "time-pos",
    "volume",
    "mute",
    // track selection
    "aid",
    "sid",
    // decode / video output (chosen by the player per stream)
    "hwdec",
    "vo",
    // separate-window on-screen-controls / input behaviour
    "osc",
    "input-default-bindings",
    "input-vo-keyboard",
    // aspect / scaling (videoScale: cover/fill/contain)
    "keepaspect",
    "panscan",
    // subtitle styling
    "sub-ass-override",
    "sub-scale",
    "sub-pos",
    "sub-delay",
    "sub-color",
    "sub-back-color",
    "sub-border-color",
    // a/v sync (audio menu: shifts the audio track against the video)
    "audio-delay",
];

/// What [`shell_init`] returns to the web client's `useShell`.
#[derive(Serialize)]
pub struct ShellInit {
    version: String,
    #[serde(rename = "gpuVideoProcessing")]
    gpu_video_processing: bool,
    /// Whether native playback is available (libmpv loaded + initialized).
    ok: bool,
}

/// One inbound IPC signal, emitted as the Tauri event `shell-signal`. `event` is
/// the method name the web client listens for (`mpv-prop-change` /
/// `mpv-event-ended`); `payload` is its single argument.
#[derive(Serialize, Clone)]
struct ShellSignal {
    event: String,
    payload: Value,
}

impl ShellState {
    /// Get the native player, creating it (per-device backend) on first use.
    /// Cheap after the first call. Errors if the backend can't be initialized
    /// (e.g. libmpv can't be loaded on Windows).
    fn ensure(&self, app: &AppHandle) -> Result<NativePlayer, String> {
        let mut guard = self.0.lock().unwrap();
        if let Some(player) = guard.as_ref() {
            return Ok(player.clone());
        }
        // Embed mpv into the app's native surface (on Windows, the main window's
        // HWND is the mpv `wid`) so video renders inside the app instead of a
        // separate top-level window. The web UI, transparent during playback,
        // overlays it. The surface backend is chosen per platform (see
        // `surface::create`); a platform without embedding returns no wid and mpv
        // opens its own window.
        let surface = crate::surface::create();
        let wid = surface.video_wid(app);
        let ctrl = Arc::new(Controller::create(app, wid)?);
        // Android: register the live player so a later surfaceCreated re-attaches
        // its Surface, and attach now if one is already present (the common case -
        // the SurfaceView exists before the web calls shell_init). Attaching
        // post-init is what actually lands frames on the SurfaceView.
        #[cfg(target_os = "android")]
        {
            *ANDROID_PLAYER.lock().unwrap() = Some(Arc::downgrade(&ctrl));
            if let Some(wid) = wid {
                ctrl.attach_surface(wid);
            }
        }
        spawn_event_loop(ctrl.clone(), app.clone());
        // With force-window, mpv's (black) output window exists from startup; push
        // it behind the WebView as soon as it appears so it never covers the UI.
        if wid.is_some() {
            let app = app.clone();
            std::thread::spawn(move || {
                let surface = crate::surface::create();
                for _ in 0..30 {
                    std::thread::sleep(Duration::from_millis(50));
                    surface.composite_behind_ui(&app);
                }
            });
        }
        let player = NativePlayer::Mpv(ctrl);
        *guard = Some(player.clone());
        tracing::info!("shell: native mpv player ready (wid={wid:?})");
        Ok(player)
    }
}

/// Register the `rillio://` protocol on a freshly loaded (NOT yet initialized)
/// mpv, backed by the streaming server's engine. Returns whether it took.
///
/// The engine is looked up PER OPEN rather than captured here: the player is
/// created when the web client mounts, which can race the server's bind, and
/// capturing "no engine yet" would disable the byte plane for the whole
/// session. By the time mpv opens anything the web has already gone through the
/// router (the create POST), so the lookup always succeeds in practice; a
/// failure fails the load loudly instead of silently playing from nowhere.
fn register_stream_protocol(app: &AppHandle, mpv: &Mpv) -> bool {
    use crate::stream_cb::SourceFactory;
    use tauri::Manager;

    let app = app.clone();
    let result = crate::stream_cb::register(mpv, move |info_hash: &str, file_idx: u32| {
        let server = app
            .state::<crate::ServerHandle>()
            .get()
            .ok_or_else(|| "the streaming server has not bound yet".to_string())?;
        crate::stream_cb::EngineSources::new(server.engine.clone(), server.runtime.clone())
            .open(info_hash, file_idx)
    });
    match result {
        Ok(()) => {
            tracing::info!("mpv: rillio:// registered; torrent bytes stay in-process");
            true
        }
        Err(e) => {
            tracing::warn!("mpv: rillio:// unavailable ({e}); the player keeps using the HTTP stream url");
            false
        }
    }
}

impl Controller {
    /// Load libmpv and initialize an idle instance ready for `loadfile`. When
    /// `wid` is set, mpv renders into that window handle (embedded); otherwise it
    /// opens its own output window.
    fn create(app: &AppHandle, wid: Option<isize>) -> Result<Self, String> {
        let dll = mpv::default_dll_path();
        let mpv = Mpv::load(&dll)?;
        // The `rillio://` byte plane. Registered BEFORE initialize() (mpv's
        // requirement) and BEFORE the streaming server has necessarily bound:
        // the factory resolves the engine lazily, per open, so a player created
        // during the bind race still serves every later loadfile. A libmpv
        // without the symbol simply leaves this false and the web keeps using
        // the HTTP URL - the documented fallback, never a hard failure.
        let stream_cb = register_stream_protocol(app, &mpv);
        // ffmpeg's mediacodec hwdec + audiotrack AO and mpv's android VO all
        // reach Java over JNI; hand them the JavaVM before anything initializes.
        // Fail loud: without it they die cryptically at first use instead.
        #[cfg(target_os = "android")]
        mpv::set_ffmpeg_java_vm()?;
        // `wid` must be set before initialize(): mpv creates its video output as
        // a child of this window rather than a standalone one. WINDOWS ONLY:
        // Android attaches the surface AFTER init (see below and attach_surface) -
        // setting wid before init there binds gpu-next to a not-yet-laid-out
        // SurfaceView and frames never post to it (video stays black). This
        // mirrors mpv-android (is.xyz.mpv): options before init, wid + force-window
        // in surfaceCreated after init.
        #[cfg(not(target_os = "android"))]
        if let Some(wid) = wid {
            if let Err(e) = mpv.set_option("wid", &wid.to_string()) {
                tracing::warn!("mpv: could not set wid={wid}: {e} (falling back to own window)");
            }
        }
        // Stay alive across stop/eof so the same instance plays every title; the
        // web client sends `stop` then a later `loadfile`. No stray window at
        // startup - mpv opens its output window only once a file plays.
        //
        // Best-effort: a minimal libmpv build may lack some of these options
        // (e.g. `osc` needs the Lua OSC). A missing option must not sink the
        // whole player, so we log and continue rather than abort init.
        // When embedded (wid set), keep a black output window alive from startup
        // so the transparent WebView reveals BLACK during the buffering gap (before
        // the first frame) instead of the desktop behind the app. Composited to the
        // back immediately (see ensure) so it never covers the UI.
        // Android forces the window only when the surface is attached post-init
        // (attach_surface); at init it has no window yet.
        #[cfg(target_os = "android")]
        let force_window = "no";
        #[cfg(not(target_os = "android"))]
        let force_window = if wid.is_some() { "yes" } else { "no" };
        for (name, value) in [
            ("idle", "yes"),
            ("force-window", force_window),
            ("config", "no"), // ignore any on-machine mpv.conf
            ("terminal", "no"),
            ("osc", "no"), // the web UI is our on-screen controls
            ("input-default-bindings", "no"),
            ("input-vo-keyboard", "no"),
            // Be patient with the loopback stream: on a fresh large title the
            // server holds the response while librqbit runs its initial checksum
            // pass (can be ~a minute). Default network-timeout (60s) could abort
            // the open; give generous headroom.
            ("network-timeout", "600"),
            // HDR: `gpu-next` (libplacebo) handles PQ/HLG properly (the default
            // `gpu` vo washes highlights to grey); keep `gpu` as a fallback for
            // a minimal libmpv build that lacks it.
            ("vo", "gpu-next,gpu"),
            // Tone-mapping path (SDR displays only): measure scene peak for the
            // roll-off, and use bt.2446a, the curve the mpv manual recommends
            // for well-mastered content (the default spline reads flat).
            ("hdr-compute-peak", "yes"),
            ("tone-mapping", "bt.2446a"),
        ] {
            if let Err(e) = mpv.set_option(name, value) {
                tracing::warn!("mpv: option {name}={value} not applied: {e}");
            }
        }
        // Windows-only: HDR passthrough into the DWM-composited window,
        // diagnosed 2026-07-12 on a Windows-HDR desktop.
        // The hint re-flags the swapchain to the content's colorspace (PQ
        // for HDR files, sRGB for SDR files, per file) and DWM honors it,
        // including for our embedded child window. `auto` engages only when
        // the display can actually present the space; SDR displays fall
        // back to tone-mapping. Two defaults break it and must be overridden:
        // - hint-mode defaults to `target`, which ADAPTS the image toward
        //   the display's inferred characterization instead of passing the
        //   source signal through; with the (sRGB) display ICC profile in
        //   that inference the picture goes washed-out grey, because the
        //   manual guarantees "the ICC profile always takes precedence over
        //   any metadata". `source` is the true passthrough mode.
        // - icc-profile-auto is forced off for the same reason.
        // On Android none of this applies: mpv's GLES context has no HDR
        // colorspace path, so output is tone-mapped SDR via the options above
        // (HDR output mode is tracked as android-mpv E2).
        #[cfg(windows)]
        for (name, value) in [
            ("target-colorspace-hint", "auto"),
            ("target-colorspace-hint-mode", "source"),
            ("icc-profile-auto", "no"),
        ] {
            if let Err(e) = mpv.set_option(name, value) {
                tracing::warn!("mpv: option {name}={value} not applied: {e}");
            }
        }
        // Android: route audio through AudioTrack (the JNI AO), and bring up
        // gpu-next on the EGL/GLES context of the ANativeWindow we passed as
        // `wid`. The exact mpv-android recipe (is.xyz.mpv MPVView.initOptions):
        // gpu-context=android + opengl-es=yes. opengl-es=yes is load-bearing -
        // without it gpu-next tries a DESKTOP GL context (absent on Android), the
        // android VO fails to open, and mpv falls back to the render-API `libmpv`
        // VO (which needs an mpv_render_context we don't create) -> the fatal
        // "No render context set" and a black video plane. (JNI/JavaVM is already
        // handed to ffmpeg above; the working audiotrack AO proves that part.)
        #[cfg(target_os = "android")]
        for (name, value) in [
            ("ao", "audiotrack"),
            ("gpu-context", "android"),
            ("opengl-es", "yes"),
        ] {
            if let Err(e) = mpv.set_option(name, value) {
                tracing::warn!("mpv: option {name}={value} not applied: {e}");
            }
        }
        // Debug knob for HDR diagnosis: override the render backend (e.g.
        // RILLIO_MPV_GPU_API=vulkan; default is d3d11). Some libplacebo builds
        // only engage HDR passthrough correctly on one API.
        if let Ok(api) = std::env::var("RILLIO_MPV_GPU_API") {
            tracing::info!("mpv: gpu-api override: {api}");
            if let Err(e) = mpv.set_option("gpu-api", &api) {
                tracing::warn!("mpv: option gpu-api={api} not applied: {e}");
            }
        }
        // Debug knob: arbitrary extra init options as `name=value` pairs split
        // on ';' (e.g. RILLIO_MPV_SET="target-colorspace-hint-mode=source;
        // vf=format:dolbyvision=no"). Applied last so it can override the
        // defaults above. Developer-only: env vars are not reachable from web
        // content, so this does not widen the bridge allowlists.
        if let Ok(sets) = std::env::var("RILLIO_MPV_SET") {
            for entry in sets.split(';').filter(|s| !s.trim().is_empty()) {
                if let Some((name, value)) = entry.split_once('=') {
                    let (name, value) = (name.trim(), value.trim());
                    tracing::info!("mpv: debug option {name}={value}");
                    if let Err(e) = mpv.set_option(name, value) {
                        tracing::warn!("mpv: debug option {name}={value} not applied: {e}");
                    }
                }
            }
        }
        // Only surface mpv's own errors by default (verbose "v" floods the event
        // loop and lags the UI). Opt into verbose with RILLIO_MPV_VERBOSE (env on
        // desktop; on Android there is no env plumbing, so flip this to "v" and
        // rebuild to debug the VO/GPU-context path on a real device). Requested
        // BEFORE initialize() so the VO creation logs (force-window=yes builds the
        // VO up front) are captured - that init window is where gpu-next's EGL
        // context on the `wid` Surface is negotiated.
        let log_level = if std::env::var("RILLIO_MPV_VERBOSE").is_ok() { "v" } else { "error" };
        if let Err(e) = mpv.request_log_messages(log_level) {
            tracing::warn!("mpv: request_log_messages failed: {e}");
        }
        mpv.initialize()?;
        // Observe the extra stats properties ShellVideo doesn't, so the panel can
        // show codec/bitrate/hwdec/etc. (Their changes flow through the same
        // event loop and land in `props`.)
        // GEOMETRY_PROPS ride the same event loop into the same `props` cache;
        // the shell reads them itself (blur geometry) and never forwards them.
        for name in STATS_PROPS.iter().chain(GEOMETRY_PROPS.iter()) {
            if let Err(e) = mpv.observe_property(name) {
                tracing::debug!("mpv: observe {name} failed: {e}");
            }
        }
        Ok(Self {
            mpv: Arc::new(mpv),
            stream_cb,
            props: Mutex::new(serde_json::Map::new()),
            snapshot: Mutex::new(SnapshotCache::default()),
            blur: Mutex::new(BlurState::default()),
        })
    }

    /// Android: attach (or re-attach) the video Surface to a LIVE mpv, matching
    /// mpv-android's surfaceCreated -> set `wid` + `force-window=yes` AFTER init.
    /// Setting wid pre-init binds gpu-next's GLES context to a not-yet-ready
    /// SurfaceView and frames never post; doing it here (surface laid out and
    /// visible) is what makes video actually render into the SurfaceView. Called
    /// once from `ensure` after init and again on every `surfaceCreated`.
    #[cfg(target_os = "android")]
    fn attach_surface(&self, wid: isize) {
        if let Err(e) = self.mpv.set_option("wid", &wid.to_string()) {
            tracing::warn!("mpv: android attach wid={wid} failed: {e}");
            return;
        }
        if let Err(e) = self.mpv.set_option("force-window", "yes") {
            tracing::warn!("mpv: android force-window failed: {e}");
        }
        tracing::info!("mpv: android surface attached (wid={wid})");
    }

    /// The last snapshot, if it is still inside [`SNAPSHOT_MIN_INTERVAL`].
    /// `None` means "capture a fresh one".
    fn recent_snapshot(&self) -> Option<String> {
        let snap = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        match (snap.at, &snap.data_url) {
            (Some(at), Some(url)) if at.elapsed() < SNAPSHOT_MIN_INTERVAL => Some(url.clone()),
            _ => None,
        }
    }

    fn store_snapshot(&self, data_url: &str) {
        let mut snap = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        snap.data_url = Some(data_url.to_string());
        snap.at = Some(Instant::now());
    }

    /// Run an already-allowlist-checked `mpv-command` argv, applying the two
    /// mpv-specific normalizations the web client's argv needs. Kept here (in the
    /// mpv backend) rather than in `shell_send` so the bridge dispatch stays
    /// backend-agnostic.
    fn run_command(&self, refs: &[&str]) -> Result<(), String> {
        // Drop cached stats when playback stops so the panel doesn't show a
        // previous title's codec/bitrate.
        if refs.first() == Some(&"stop") {
            self.props.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        // Normalize `loadfile`. The web client appends a positional
        // `replace`/index/`start=+N` (resume time) form whose exact shape depends
        // on its mpv-version guess; this libmpv build misparses it ("loadfile
        // option must be an integer: start=+90") and the load fails, so nothing
        // ever streams. Apply the resume time via the `start` property and issue a
        // clean two-arg loadfile instead.
        if refs.first() == Some(&"loadfile") {
            // URL already validated as http(s) by check_mpv_command.
            let url = refs.get(1).copied().unwrap_or_default();
            let start = refs
                .iter()
                .find_map(|a| a.strip_prefix("start=+").or_else(|| a.strip_prefix("start=")));
            // Reset to "none" for fresh plays so a prior resume point doesn't leak
            // into the next title.
            let _ = self.mpv.set_property("start", start.unwrap_or("none"));
            tracing::debug!("mpv loadfile url={url} start={start:?}");
            return self.mpv.command(&["loadfile", url, "replace"]);
        }
        self.mpv.command(refs)
    }
}

/// The event-loop thread: block on mpv events and forward the ones the web
/// client cares about as `shell-signal`. Runs for the app's lifetime.
fn spawn_event_loop(ctrl: Arc<Controller>, app: AppHandle) {
    let mpv = ctrl.mpv.clone();
    std::thread::Builder::new()
        .name("mpv-events".into())
        .spawn(move || {
            // Whether we've pushed the embedded mpv child behind the WebView for
            // the current playback (mpv creates its output window on the first
            // frame; do it once video-params arrive).
            let mut composited = false;
            // Rate-limit high-frequency props (mpv fires time-pos/cache-time
            // many×/s); each emit is an IPC hop + React update, so throttle to
            // keep the UI responsive. The web player caps its own time to 1/s.
            let mut last_hf: HashMap<&'static str, Instant> = HashMap::new();
            loop {
            match mpv.wait_event(-1.0) {
                MpvEvent::Shutdown => break,
                MpvEvent::LogMessage { prefix, level, text } => {
                    tracing::debug!("mpv[{level}] {prefix}: {text}");
                }
                MpvEvent::PropertyChange { name, value } => {
                    // Cache the latest value for the stats panel.
                    // Recover a poisoned lock rather than cascade-panic: the props
                    // cache is non-critical stats, a panicked reader must not sink
                    // the event loop.
                    ctrl.props
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(name.clone(), value.clone());
                    // High-signal, low-noise props at debug (skip the per-frame ones).
                    if !matches!(name.as_str(), "time-pos" | "demuxer-cache-time") {
                        tracing::debug!("mpv prop {name} = {value}");
                    }
                    // Once the video output exists, drop it behind the WebView so
                    // the (transparent-during-playback) web UI overlays it. Only
                    // when in-window embedding is enabled (S4).
                    if name == "video-params" && value.is_object() && !composited {
                        if crate::mpv_embed_enabled() {
                            crate::surface::create().composite_behind_ui(&app);
                        }
                        composited = true;
                    }
                    if name == "video-params" && value.is_null() {
                        composited = false; // unloaded; re-composite on next file
                    }
                    // Stats-only props (our extra observes) are read by the panel
                    // via `shell_mpv_stats`; don't forward them to the web player
                    // - some update per-frame and would flood the UI event stream.
                    let mut forward = !STATS_PROPS.contains(&name.as_str())
                        && !GEOMETRY_PROPS.contains(&name.as_str());
                    if forward {
                        if let Some(key) = HF_PROPS.iter().find(|k| **k == name.as_str()) {
                            let now = Instant::now();
                            match last_hf.get(key) {
                                Some(t) if now.duration_since(*t) < HF_INTERVAL => forward = false,
                                _ => { last_hf.insert(key, now); }
                            }
                        }
                    }
                    if forward {
                        emit(
                            &app,
                            "mpv-prop-change",
                            serde_json::json!({ "name": name, "data": value }),
                        );
                    }
                }
                MpvEvent::EndFile { reason, error } => {
                    tracing::debug!("mpv end-file: reason={reason} error={error}");
                    // mpv_end_file_reason: 0 eof, 2 stop, 3 quit, 4 error, 5 redirect.
                    let reason_str = match reason {
                        0 => "eof",
                        2 => "stop",
                        3 => "quit",
                        4 => "error",
                        5 => "redirect",
                        _ => "other",
                    };
                    let err = if error < 0 {
                        Value::String(mpv.error_string(error))
                    } else {
                        Value::Null
                    };
                    emit(
                        &app,
                        "mpv-event-ended",
                        serde_json::json!({ "reason": reason_str, "error": err }),
                    );
                }
                MpvEvent::Other => {}
            }
            }
        })
        .expect("spawn mpv-events thread");
}

fn emit(app: &AppHandle, event: &str, payload: Value) {
    if let Err(e) = app.emit("shell-signal", ShellSignal { event: event.into(), payload }) {
        tracing::warn!("shell: emit {event} failed: {e}");
    }
}

/// Handshake: called once when the web client's `useShell` mounts. Creates the
/// native player and reports the shell version + capabilities. `ok=false` (mpv
/// unavailable) tells the web client to fall back to the in-WebView player.
#[tauri::command]
pub fn shell_init(app: AppHandle, state: State<ShellState>) -> ShellInit {
    let ok = match state.ensure(&app) {
        Ok(_) => true,
        Err(e) => {
            tracing::error!("shell_init: native player unavailable: {e}");
            false
        }
    };
    // Single source of truth: the app version from tauri.conf.json (also what the
    // updater compares), not a hardcoded string.
    let version = app.package_info().version.to_string();
    ShellInit { version, gpu_video_processing: false, ok }
}

/// Snapshot of mpv's current media properties for the "Stats for nerds" panel:
/// codec/resolution/HDR/bit-depth/fps/bitrate/hwdec (video) and
/// codec/channels/sample-rate/bitrate (audio). Polled ~1×/s while the panel is
/// open. Values are whatever mpv last reported (some are null until playback).
#[tauri::command]
pub fn shell_mpv_stats(app: AppHandle, state: State<ShellState>) -> Value {
    match state.ensure(&app) {
        Ok(player) => Value::Object(player.stats()),
        Err(_) => Value::Object(serde_json::Map::new()),
    }
}

/// A downscaled JPEG of the CURRENT VIDEO FRAME as a `data:` URL, for the
/// player's menu/drawer backdrop (apps/web routes/Player/SnapshotBackdrop).
///
/// Why this exists: mpv renders into a NATIVE child window behind the
/// transparent WebView, so CSS `backdrop-filter` on a panel samples only web
/// content and blurs nothing. The web layer therefore asks the shell for the
/// frame and blurs it itself.
///
/// SECURITY: this is a dedicated, READ-ONLY, argument-less command. It does NOT
/// widen the generic `shell_send` bridge - `MPV_COMMAND_ALLOWLIST` and
/// `MPV_SETPROP_ALLOWLIST` are untouched, and web content still cannot reach
/// `screenshot-to-file` (or any other command) through them. The only thing web
/// content can ask for here is "a picture of the frame the user is already
/// watching", with the path chosen by the shell, never by the caller.
///
/// Fails loud (an `Err` the web layer swallows into its dark-glass fallback)
/// when there is no video, when this libmpv lacks screenshot support, or when
/// any of the file/decode steps fail.
#[tauri::command]
pub async fn player_snapshot(app: AppHandle, state: State<'_, ShellState>) -> Result<String, String> {
    let player = state.ensure(&app)?;
    // The mpv round-trip, the file read and the PNG decode + resize + JPEG
    // encode are all blocking and run at ~3/s: keep them off the async runtime's
    // worker threads. The recent-snapshot rate limit lives inside the backend.
    tauri::async_runtime::spawn_blocking(move || player.snapshot_blocking())
        .await
        .map_err(|e| format!("player_snapshot: capture task failed: {e}"))?
}

/// A fresh temp path for one screenshot. Unique per call: mpv refuses to
/// overwrite an existing screenshot file, and two in-flight captures must never
/// share a path.
fn snapshot_temp_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // .jpg, NOT .png: mpv picks the encoder from the EXTENSION, and lossless-
    // encoding a full-resolution (4K, HDR) frame costs SECONDS, which is what made
    // the backdrop arrive whole seconds after a menu opened.
    std::env::temp_dir().join(format!("rillio-snapshot-{nanos}.jpg"))
}

/// Capture one frame: mpv -> temp PNG -> downscaled JPEG data URL. Blocking.
fn capture_snapshot(ctrl: &Controller, temp: &Path) -> Result<String, String> {
    // `screenshot-to-file <path> video` - "video" mode is the decoded frame
    // WITHOUT OSD or subtitles, at source resolution. It goes through the plain
    // string-argv `mpv_command` our FFI already binds (mpv.rs), so no new FFI
    // surface is needed. The command is synchronous: on Ok the file is written.
    // HDR note: mpv applies its own screenshot tone-mapping here; whatever
    // SDR-ish image comes out is fine under blur + dark glass, so we
    // deliberately do not configure it.
    let path = temp.to_string_lossy().to_string();
    // Encoder settings for LATENCY, not fidelity: this frame is downscaled to 320px
    // and blurred by 24px before anyone sees it. 8-bit output (a 16-bit HDR PNG/JPEG
    // is far heavier to write and decode) at a low quality. Set per capture because
    // set_property is cheap and this keeps the capture self-contained; failures are
    // ignored, mpv just uses its defaults.
    let _ = ctrl.mpv.set_property("screenshot-high-bit-depth", "no");
    let _ = ctrl.mpv.set_property("screenshot-jpeg-quality", "50");
    ctrl.mpv
        .command(&["screenshot-to-file", &path, "video"])
        .map_err(|e| format!("player_snapshot: mpv screenshot failed: {e}"))?;

    let raw = std::fs::read(temp)
        .map_err(|e| format!("player_snapshot: reading {path}: {e}"))?;
    // Best-effort cleanup; a leaked temp file must not fail the capture.
    let _ = std::fs::remove_file(temp);

    let image = image::load_from_memory(&raw)
        .map_err(|e| format!("player_snapshot: decoding the screenshot: {e}"))?;
    let image = downscale(image, SNAPSHOT_MAX_WIDTH);

    let mut jpeg: Vec<u8> = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, SNAPSHOT_JPEG_QUALITY)
        .encode_image(&image.to_rgb8())
        .map_err(|e| format!("player_snapshot: encoding the jpeg: {e}"))?;

    use base64::Engine as _;
    let data_url = format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&jpeg)
    );
    ctrl.store_snapshot(&data_url);
    Ok(data_url)
}

/// Scale `image` down so its width is at most `max_width`, preserving aspect.
/// Never scales up (a smaller-than-max frame is already cheap enough).
fn downscale(image: image::DynamicImage, max_width: u32) -> image::DynamicImage {
    let (width, height) = (image.width(), image.height());
    if width <= max_width || width == 0 {
        return image;
    }
    let scaled_height = ((height as u64 * max_width as u64) / width as u64).max(1) as u32;
    // thumbnail_exact(): the fast path, meant for exactly this. A filtered resize
    // over 8M source pixels would cost more than the whole rest of the pipeline,
    // and the result is about to be blurred by 24px anyway.
    image.thumbnail_exact(max_width, scaled_height)
}

/// Blur the video behind the player's open panels, for real, on the GPU.
///
/// WHY THIS EXISTS: mpv renders into a NATIVE child window behind the transparent
/// WebView, so CSS `backdrop-filter` on a panel has nothing to sample and blurs
/// nothing. [`player_snapshot`] answered that by pulling frames back off the GPU,
/// which lands tens to hundreds of ms late and reads as lag. A user shader inside
/// mpv's own pipeline is the only way to get a live, free blur.
///
/// `rects` are the open panels in CSS px relative to the WebView `viewport`; an
/// EMPTY list means "nothing is open, stop blurring". Each rect carries its own
/// corner radius, so the blur ends on exactly the rounded edge the panel is
/// drawn with rather than showing blurred video outside a rounded corner.
///
/// LOAD-ONCE, THEN TOGGLE. The shader is loaded lazily, on the first call that
/// actually wants a blur, and is never unloaded. Two things fall out of that:
///   - With the web-side flag off nothing ever calls this, so the shader is never
///     loaded and the render pipeline that carries HDR/DV passthrough is
///     bit-for-bit what it is today. That is the whole point of the ordering.
///   - Once loaded, open/close is a `glsl-shader-opts` write - a uniform update,
///     because every parameter is `//!TYPE DYNAMIC` - not a pipeline rebuild. And
///     at `enabled=0` the shader's `//!WHEN enabled 0 >` skips both stages
///     outright, so a closed panel costs nothing. Unloading on close would buy
///     that same nothing back at the price of a recompile on every open.
///
/// SECURITY: a dedicated command, like [`player_snapshot`]. It does NOT widen the
/// generic `shell_send` bridge - `MPV_COMMAND_ALLOWLIST` and
/// `MPV_SETPROP_ALLOWLIST` are untouched, and web content still cannot reach
/// `glsl-shaders` (a FILE PATH property, hence a real one to keep out of reach)
/// through them. The only thing web content can ask for here is "blur under these
/// rectangles"; the shader, the path and the radius are all chosen by the shell.
///
/// Fails loud (an `Err` the web layer logs once and then gives up on, falling
/// back to today's dark glass) when there is no video geometry yet or mpv rejects
/// the shader.
#[tauri::command]
pub fn player_blur_rect(
    app: AppHandle,
    state: State<ShellState>,
    rects: Vec<BlurRect>,
    viewport: BlurViewport,
) -> Result<(), String> {
    if rects.len() > MAX_BLUR_RECTS {
        return Err(format!(
            "player_blur_rect: {} rects, but the shader holds {MAX_BLUR_RECTS}",
            rects.len()
        ));
    }
    state.ensure(&app)?.blur_rect(&app, rects, viewport)
}

/// The mpv implementation of the panel blur (called from `NativePlayer::blur_rect`
/// for the `Mpv` variant). Takes the concrete `Arc<Controller>` because the
/// throttle's trailing apply clones it into a thread.
fn blur_rect_mpv(
    ctrl: &Arc<Controller>,
    app: &AppHandle,
    rects: Vec<BlurRect>,
    viewport: BlurViewport,
) -> Result<(), String> {
    // "Stop blurring" before anything was ever blurred: nothing to do, and in
    // particular nothing worth loading a shader for.
    if rects.is_empty() && !ctrl.blur.lock().unwrap_or_else(|e| e.into_inner()).loaded {
        return Ok(());
    }
    ensure_blur_shader(app, ctrl)?;

    let wait = {
        let mut blur = ctrl.blur.lock().unwrap_or_else(|e| e.into_inner());
        blur.desired = Some(BlurRequest { rects, viewport });
        match blur.last_applied.map(|t| t.elapsed()) {
            Some(elapsed) if elapsed < BLUR_MIN_INTERVAL => {
                if blur.trailing {
                    // One is already scheduled and will read `desired`, which we
                    // just overwrote - so it lands on the newest state, not this.
                    return Ok(());
                }
                blur.trailing = true;
                Some(BLUR_MIN_INTERVAL - elapsed)
            }
            _ => None,
        }
    };
    match wait {
        None => apply_blur(ctrl),
        Some(delay) => {
            let ctrl = ctrl.clone();
            std::thread::Builder::new()
                .name("mpv-blur-trailing".into())
                .spawn(move || {
                    std::thread::sleep(delay);
                    if let Err(e) = apply_blur(&ctrl) {
                        tracing::warn!("shell: panel blur trailing apply failed: {e}");
                    }
                })
                .map_err(|e| format!("player_blur_rect: spawning the trailing apply: {e}"))?;
            Ok(())
        }
    }
}

/// Where the blur shader is handed to mpv from. Under the app data dir, next to
/// the rest of our per-machine state.
fn blur_shader_path(app: &AppHandle) -> Result<PathBuf, String> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("player_blur_rect: no app data dir: {e}"))?;
    Ok(dir.join("shaders").join("panel-blur.glsl"))
}

/// Write the shader out and hand it to mpv, once per mpv instance.
fn ensure_blur_shader(app: &AppHandle, ctrl: &Arc<Controller>) -> Result<(), String> {
    if ctrl.blur.lock().unwrap_or_else(|e| e.into_inner()).loaded {
        return Ok(());
    }
    let path = blur_shader_path(app)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("player_blur_rect: creating {}: {e}", dir.display()))?;
    }
    // Rewritten every session, never reused: the file is a handoff to mpv, not
    // user data, and an app update must not be shadowed by a stale copy.
    std::fs::write(&path, BLUR_SHADER)
        .map_err(|e| format!("player_blur_rect: writing {}: {e}", path.display()))?;
    let text = path.to_string_lossy().to_string();
    // mpv splits its path LISTS on ',', so a comma in the path would silently
    // become two bogus shader paths. Nothing we control puts one there; if that
    // ever changes, say so rather than render a mystery.
    if text.contains(',') {
        return Err(format!("player_blur_rect: shader path contains a comma: {text}"));
    }
    // Set the list wholesale rather than -append: we own it outright (Rillio ships
    // no other user shader, and `config=no` keeps any on-machine mpv.conf out), and
    // appending would stack a duplicate copy on every load.
    ctrl.mpv.set_property("glsl-shaders", &text)?;
    // Subtitles/OSD are drawn AFTER the video pipeline the shader hooks, so
    // without this a subtitle line under a panel stays razor sharp on top of
    // the frosted video. blend-subtitles folds them into the frame BEFORE the
    // output pass, putting them under the blur with everything else. Best
    // effort: an mpv build that rejects it just keeps sharp subs under panels.
    if let Err(e) = ctrl.mpv.set_property("blend-subtitles", "yes") {
        tracing::warn!("shell: blend-subtitles unavailable, subs stay above the panel blur: {e}");
    }
    {
        let mut blur = ctrl.blur.lock().unwrap_or_else(|e| e.into_inner());
        blur.loaded = true;
        blur.last_opts = None; // nothing has been applied to this fresh pipeline
    }
    tracing::info!("shell: panel blur shader loaded from {text}");
    // See BLUR_PARAM_SETTLE: re-apply once the VO thread has actually parsed the
    // file and registered the parameters, or this first open silently has no blur.
    let ctrl = ctrl.clone();
    std::thread::Builder::new()
        .name("mpv-blur-settle".into())
        .spawn(move || {
            std::thread::sleep(BLUR_PARAM_SETTLE);
            ctrl.blur.lock().unwrap_or_else(|e| e.into_inner()).last_opts = None;
            if let Err(e) = apply_blur(&ctrl) {
                tracing::warn!("shell: panel blur settle re-apply failed: {e}");
            }
        })
        .map_err(|e| format!("player_blur_rect: spawning the settle re-apply: {e}"))?;
    Ok(())
}

/// Push the currently desired blur state to mpv.
fn apply_blur(ctrl: &Controller) -> Result<(), String> {
    let desired = {
        let mut blur = ctrl.blur.lock().unwrap_or_else(|e| e.into_inner());
        blur.trailing = false;
        blur.last_applied = Some(Instant::now());
        blur.desired.clone()
    };
    let Some(req) = desired else { return Ok(()) };
    let opts = blur_shader_opts(ctrl, &req)?;
    {
        let mut blur = ctrl.blur.lock().unwrap_or_else(|e| e.into_inner());
        if blur.last_opts.as_deref() == Some(opts.as_str()) {
            return Ok(());
        }
        blur.last_opts = Some(opts.clone());
    }
    ctrl.mpv.set_property("glsl-shader-opts", &opts)
}

/// The video rectangle inside the mpv window, from mpv's `osd-dimensions`:
/// `(x, y, w, h, window_w, window_h)`, all in the window's physical pixels.
///
/// `osd-dimensions` reports the video rect's size (`w`/`h`) plus its margins to
/// each window edge (`ml`/`mr`/`mt`/`mb`), so the window size falls out of the
/// same object and no second source of truth (nor any assumed DPR) is needed.
fn video_rect(ctrl: &Controller) -> Result<(f64, f64, f64, f64, f64, f64), String> {
    let props = ctrl.props.lock().unwrap_or_else(|e| e.into_inner());
    let dims = props
        .get("osd-dimensions")
        .filter(|v| v.is_object())
        .ok_or("player_blur_rect: mpv has not reported osd-dimensions (nothing playing?)")?;
    let get = |key: &str| -> Result<f64, String> {
        dims.get(key)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("player_blur_rect: osd-dimensions.{key} missing"))
    };
    let (w, h) = (get("w")?, get("h")?);
    let (ml, mr, mt, mb) = (get("ml")?, get("mr")?, get("mt")?, get("mb")?);
    let (win_w, win_h) = (ml + w + mr, mt + h + mb);
    if !(w > 0.0 && h > 0.0 && win_w > 0.0 && win_h > 0.0) {
        return Err(format!(
            "player_blur_rect: degenerate osd-dimensions ({w}x{h} video in a {win_w}x{win_h} window)"
        ));
    }
    Ok((ml, mt, w, h, win_w, win_h))
}

/// Render `req` as an mpv `glsl-shader-opts` string, mapping the web's panel
/// rects into the shader's coordinate space.
///
/// THE MAPPING, end to end. The web measures panels in CSS px against its own
/// viewport. The shader's OUTPUT hook works in coords normalized over the VIDEO
/// RECTANGLE - which is NOT the window: letterbox/pillarbox bars are not part of
/// the OUTPUT texture at all. Three steps bridge that:
///
///   1. CSS px -> the window's physical px, by `win_size / viewport_size`. This
///      works because mpv's child window and the WebView are both sized to the
///      SAME main-window client area (shell.rs only ever re-orders that child,
///      never moves or resizes it), so the two describe one rectangle at two
///      scales. Deriving the scale by measurement rather than trusting
///      `devicePixelRatio` keeps them in step through a display-scaling change.
///   2. window px -> output px, by subtracting the video rect's origin. Lengths
///      need no conversion at this step: the output rect is a sub-rect of the same
///      window, in the same physical pixels.
///   3. output px -> normalized, by dividing by the video rect's size.
///
/// Nothing is clamped to the video rect: a panel overhanging into the black bars
/// simply maps outside [0,1], and the shader's SDF intersects it with the texture
/// for free. Values ARE clamped to each parameter's declared range, because mpv
/// rejects the whole option string over a single out-of-range value.
fn blur_shader_opts(ctrl: &Controller, req: &BlurRequest) -> Result<String, String> {
    blur_opts_string(video_rect(ctrl)?, req)
}

/// The pure half of [`blur_shader_opts`]: the geometry, with mpv's video rect
/// (`(x, y, w, h, window_w, window_h)` in physical px) already read.
fn blur_opts_string(
    video: (f64, f64, f64, f64, f64, f64),
    req: &BlurRequest,
) -> Result<String, String> {
    let (video_x, video_y, video_w, video_h, win_w, win_h) = video;
    if !(req.viewport.width > 0.0 && req.viewport.height > 0.0) {
        return Err(format!("player_blur_rect: bad viewport {:?}", req.viewport));
    }
    let scale_x = win_w / req.viewport.width;
    let scale_y = win_h / req.viewport.height;
    // A length only needs the CSS->physical scale (step 2 above is a translation).
    // x and y scale identically under any real display scaling; average them
    // rather than silently picking one and being wrong if they ever diverge.
    let length_scale = (scale_x + scale_y) * 0.5;

    let mut opts: Vec<String> = Vec::with_capacity(4 + MAX_BLUR_RECTS * 4);
    opts.push(format!("enabled={}", u8::from(!req.rects.is_empty())));
    opts.push(format!("count={}", req.rects.len()));
    opts.push(format!("radius={:.4}", (BLUR_RADIUS_CSS_PX * length_scale).clamp(0.0, 512.0)));
    for i in 0..MAX_BLUR_RECTS {
        // Unused slots are zeroed rather than left stale: `count` already gates
        // them, but the whole string is written every time, so it should describe
        // the whole state.
        let (x, y, w, h, c) = match req.rects.get(i) {
            Some(r) => (
                (r.x * scale_x - video_x) / video_w,
                (r.y * scale_y - video_y) / video_h,
                r.width * scale_x / video_w,
                r.height * scale_y / video_h,
                r.corner * length_scale,
            ),
            None => (0.0, 0.0, 0.0, 0.0, 0.0),
        };
        opts.push(format!("r{i}x={:.6}", x.clamp(-32.0, 32.0)));
        opts.push(format!("r{i}y={:.6}", y.clamp(-32.0, 32.0)));
        opts.push(format!("r{i}w={:.6}", w.clamp(0.0, 64.0)));
        opts.push(format!("r{i}h={:.6}", h.clamp(0.0, 64.0)));
        opts.push(format!("r{i}c={:.4}", c.clamp(0.0, 512.0)));
    }
    Ok(opts.join(","))
}

/// Carry one outbound IPC message to mpv. `args` is the web client's argument
/// list; each method takes a single argument at `args[0]` (an array for
/// commands/prop-sets, a string for prop-observes, a bool for the gpu toggle).
#[tauri::command]
pub fn shell_send(
    app: AppHandle,
    state: State<ShellState>,
    method: String,
    args: Vec<Value>,
) -> Result<(), String> {
    let arg0 = args.first().cloned().unwrap_or(Value::Null);
    match method.as_str() {
        "mpv-command" => {
            let list = arg0.as_array().ok_or("mpv-command: expected an array")?;
            let strs: Vec<String> = list.iter().map(json_to_mpv_str).collect();
            let refs: Vec<&str> = strs.iter().map(String::as_str).collect();
            // SECURITY (S1/S5): validate against the allowlist BEFORE the native
            // player is even loaded, so hostile input is rejected (and logged
            // loudly) even if mpv is unavailable, and never reaches mpv.command().
            check_mpv_command(&refs)?;
            let player = state.ensure(&app)?;
            tracing::debug!("mpv command {refs:?}");
            player.command(&refs)
        }
        "mpv-set-prop" => {
            let list = arg0.as_array().ok_or("mpv-set-prop: expected [name, value]")?;
            let name = list
                .first()
                .and_then(Value::as_str)
                .ok_or("mpv-set-prop: missing property name")?;
            // SECURITY (S5): only properties the player legitimately sets. Reject
            // (and log) anything else before touching the native player.
            check_mpv_setprop(name)?;
            let value = json_to_mpv_str(list.get(1).unwrap_or(&Value::Null));
            let player = state.ensure(&app)?;
            tracing::debug!("mpv set-prop {name} = {value}");
            if let Err(e) = player.set_property(name, &value) {
                // A minimal build may reject a prop (e.g. vo=gpu-next); log but
                // don't fail the whole load sequence.
                tracing::warn!("mpv set-prop {name}={value} failed: {e}");
            }
            Ok(())
        }
        "mpv-observe-prop" => {
            let name = arg0.as_str().ok_or("mpv-observe-prop: expected a name")?;
            let player = state.ensure(&app)?;
            // ALWAYS observe - never de-dupe. The web client builds a fresh
            // ShellVideo per playback and relies on mpv re-emitting each
            // property's initial value; `mpv-version` in particular gates the
            // `loadfile` (waitForMPVVersion). De-duping made the 2nd+ playback in
            // a session never receive mpv-version → loadfile never fired → the
            // title silently "wouldn't resume/play". (mpv fires an initial value
            // per observe registration; duplicates are cheap and HF props are
            // throttled downstream.)
            player.observe_property(name)
        }
        // GPU video processing (mpv shaders) - not wired yet; accept silently so
        // the web client's load sequence isn't interrupted.
        "mpv-set-gpu-video-processing" => Ok(()),
        // Capability handshake for the `rillio://` byte plane. ShellVideo asks
        // once per instance, right where it registers its property observers,
        // and only emits a `rillio://` loadfile once the answer comes back true.
        //
        // Answered as a `shell-signal` rather than a return value because that
        // is the channel ShellVideo already listens on (it never reads
        // shell_send's result), and it creates the player the same way an
        // observe does, so the answer reflects the instance that will actually
        // play. A shell too old to know this method falls through to the
        // catch-all below, no answer arrives, and ShellVideo's timeout keeps it
        // on the HTTP url.
        "rillio-stream-cb-query" => {
            let available = state.ensure(&app).map(|p| p.stream_cb()).unwrap_or(false);
            emit(&app, "rillio-stream-cb", serde_json::json!({ "available": available }));
            Ok(())
        }
        // Fullscreen: the web player drives it through the shell when active
        // (FullscreenProvider). Toggle the native window and echo the state back
        // as `win-visibility-changed` so the UI updates (and can exit again).
        "win-set-visibility" => {
            use tauri::Manager;
            let fullscreen = arg0.get("fullscreen").and_then(Value::as_bool).unwrap_or(false);
            // Desktop toggles the OS window's fullscreen; on Android the app is
            // already immersive and the API does not exist. The web UI still gets
            // its echo below either way.
            #[cfg(desktop)]
            if let Some(window) = app.get_webview_window("main") {
                use std::sync::atomic::Ordering;
                if fullscreen {
                    // Drop maximize BEFORE going fullscreen so tao's maximized
                    // client-area inset (which reserves the taskbar edge on a
                    // borderless window) does not ride into fullscreen and leave
                    // the video/overlays short of the taskbar (see
                    // WAS_MAXIMIZED_BEFORE_FS). Skip the capture if already
                    // fullscreen so a repeated toggle can't forget the state.
                    if !window.is_fullscreen().unwrap_or(false) {
                        let was_max = window.is_maximized().unwrap_or(false);
                        WAS_MAXIMIZED_BEFORE_FS.store(was_max, Ordering::Relaxed);
                        if was_max {
                            let _ = window.unmaximize();
                        }
                    }
                    if let Err(e) = window.set_fullscreen(true) {
                        tracing::warn!("shell: set_fullscreen(true) failed: {e}");
                    }
                } else {
                    if let Err(e) = window.set_fullscreen(false) {
                        tracing::warn!("shell: set_fullscreen(false) failed: {e}");
                    }
                    // tao restores the (unmaximized) placement we saved above, so
                    // re-maximize explicitly to return to the prior state.
                    if WAS_MAXIMIZED_BEFORE_FS.swap(false, Ordering::Relaxed) {
                        let _ = window.maximize();
                    }
                }
            }
            emit(
                &app,
                "win-visibility-changed",
                serde_json::json!({ "visible": true, "visibility": 1, "isFullscreen": fullscreen }),
            );
            Ok(())
        }
        // The web client has mounted and registered its listeners. Flush any OS
        // deep links (stremio:// / rillio://) that arrived during startup, before
        // the `deep-link-open` listener existed (see crate::DeepLinkState).
        "app-ready" => {
            crate::mark_web_ready_and_flush(&app);
            Ok(())
        }
        other => {
            tracing::debug!("shell_send: unhandled method {other}");
            Ok(())
        }
    }
}

/// Render a JSON argument the way mpv's string setters expect: booleans as
/// `yes`/`no`, numbers as their decimal text, strings verbatim, null as empty.
fn json_to_mpv_str(v: &Value) -> String {
    match v {
        Value::Bool(b) => if *b { "yes" } else { "no" }.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// SECURITY (S1): gate an `mpv-command` argv against [`MPV_COMMAND_ALLOWLIST`]
/// and, for `loadfile`, its target URL. Returns a loud `Err` (surfaced to the
/// web caller and logged at error) for anything off the allowlist, so an XSS in
/// addon-driven content can never reach `run`/`subprocess`/`load-script` etc.
fn check_mpv_command(refs: &[&str]) -> Result<(), String> {
    let name = *refs.first().ok_or("mpv-command: empty command")?;
    if !MPV_COMMAND_ALLOWLIST.contains(&name) {
        tracing::error!("shell: BLOCKED disallowed mpv-command {name:?} (argv {refs:?})");
        return Err(format!("mpv-command {name:?} is not allowed"));
    }
    if name == "loadfile" {
        check_loadfile_argv(refs)?;
    }
    Ok(())
}

/// SECURITY: `loadfile`'s FULL argv shape is validated, not only its URL.
///
/// mpv's signature is `loadfile <url> [<flags> [<index>] [<options>]]`, and
/// the trailing options argument is a comma-separated list of per-file
/// property assignments that mpv applies with NO further vetting:
/// `stream-record=<path>` writes an attacker-chosen file, `sub-file=` /
/// `audio-file=` read one, `vf=` reaches lavfi. All of those are properties
/// [`MPV_SETPROP_ALLOWLIST`] deliberately excludes, so letting them ride in
/// argv[3]/argv[4] would bypass that allowlist entirely. Only the exact
/// shapes ShellVideo.js emits pass (packages/video/src/ShellVideo, `load`):
///
///   [loadfile, url]
///   [loadfile, url, "replace"]
///   [loadfile, url, "replace", "start=+N"]        (mpv < 0.39: flags, options)
///   [loadfile, url, "replace", "-1", "start=+N"]  (mpv >= 0.39: flags, index, options)
fn check_loadfile_argv(refs: &[&str]) -> Result<(), String> {
    validate_stream_url(refs.get(1).copied().unwrap_or_default())?;
    let refuse = |what: &str| {
        tracing::error!("shell: BLOCKED loadfile with {what} (argv {refs:?})");
        Err(format!("loadfile: {what} is not allowed"))
    };
    if refs.len() > 5 {
        return refuse("extra arguments");
    }
    if let Some(&flags) = refs.get(2) {
        if flags != "replace" {
            return refuse("flags other than \"replace\"");
        }
    }
    match refs.len() {
        2 | 3 => Ok(()),
        4 if is_safe_loadfile_options(refs[3]) => Ok(()),
        4 => refuse("an options argument outside start=<seconds>"),
        5 if refs[3] != "-1" => refuse("a playlist index other than -1"),
        5 if is_safe_loadfile_options(refs[4]) => Ok(()),
        5 => refuse("an options argument outside start=<seconds>"),
        _ => unreachable!("lengths 0..=1 cannot reach here and >5 was refused"),
    }
}

/// The one per-file options string ShellVideo legitimately sends: empty, or
/// exactly `start=` + an unsigned decimal number with an optional `+` sign and
/// an optional fraction (`^start=\+?\d+(\.\d+)?$`). No commas, so a second
/// option can never be smuggled alongside a valid start.
fn is_safe_loadfile_options(opts: &str) -> bool {
    if opts.is_empty() {
        return true;
    }
    let Some(value) = opts.strip_prefix("start=") else {
        return false;
    };
    let value = value.strip_prefix('+').unwrap_or(value);
    let (int, frac) = match value.split_once('.') {
        Some((int, frac)) => (int, Some(frac)),
        None => (value, None),
    };
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match frac {
        Some(frac) => !frac.is_empty() && frac.bytes().all(|b| b.is_ascii_digit()),
        None => true,
    }
}

/// SECURITY (S5): gate an `mpv-set-prop` name against [`MPV_SETPROP_ALLOWLIST`].
fn check_mpv_setprop(name: &str) -> Result<(), String> {
    if MPV_SETPROP_ALLOWLIST.contains(&name) {
        return Ok(());
    }
    tracing::error!("shell: BLOCKED disallowed mpv-set-prop {name:?}");
    Err(format!("mpv-set-prop {name:?} is not allowed"))
}

/// SECURITY (S5): a `loadfile` target must be a plain http(s) stream URL (the
/// local streaming server at http://127.0.0.1:<port>/… or a direct addon URL),
/// or our own `rillio://` byte plane. mpv would otherwise happily open a local
/// path, `file://`, or one of its protocol handlers (`av://`, `edl://`,
/// `memory://`, pipe/subprocess-style inputs) - data-exfil / code-execution
/// vectors reachable from web content.
///
/// `rillio://` is the ONE widening, and it is narrow by construction: the
/// scheme resolves to the in-process torrent engine and nothing else, and
/// [`crate::stream_cb::parse_url`] accepts exactly `rillio://<40 lowercase
/// hex>/<u32>`. No filename, no query, no traversal, no second spelling of the
/// same stream - so the only thing web content can ask for is "bytes of a file
/// inside a torrent", the same authority the HTTP stream route already grants.
/// Unlike the http(s) check, this one is CASE-SENSITIVE, because the parser's
/// grammar is (an uppercase hash is a different string, therefore refused).
fn validate_stream_url(url: &str) -> Result<(), String> {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(());
    }
    if lower.starts_with("rillio://") {
        return match crate::stream_cb::parse_url(trimmed) {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::error!("shell: BLOCKED loadfile with malformed rillio:// URL {url:?}: {e}");
                Err(format!("loadfile: refusing malformed rillio:// URL {url:?}: {e}"))
            }
        };
    }
    tracing::error!("shell: BLOCKED loadfile with non-http(s) URL {url:?}");
    Err(format!("loadfile: refusing non-http(s) URL {url:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command ShellVideo actually sends over the bridge must pass.
    #[test]
    fn allows_the_commands_shellvideo_sends() {
        check_mpv_command(&["stop"]).unwrap();
        // The three loadfile shapes ShellVideo emits: bare, mpv < 0.39
        // (flags + options), mpv >= 0.39 (flags + index + options).
        check_mpv_command(&["loadfile", "http://127.0.0.1:11470/abc/0"]).unwrap();
        check_mpv_command(&["loadfile", "https://cdn.example/stream.mkv", "replace", "start=+90"])
            .unwrap();
        check_mpv_command(&["loadfile", "https://cdn.example/stream.mkv", "replace", "-1", "start=+90"])
            .unwrap();
        // Shapes nothing emits today but that carry no options payload.
        check_mpv_command(&["loadfile", "https://cdn.example/a.mkv", "replace"]).unwrap();
        check_mpv_command(&["loadfile", "https://cdn.example/a.mkv", "replace", ""]).unwrap();
        check_mpv_command(&["loadfile", "https://cdn.example/a.mkv", "replace", "-1", ""]).unwrap();
        check_mpv_command(&["loadfile", "https://cdn.example/a.mkv", "replace", "-1", "start=90.5"])
            .unwrap();
    }

    /// loadfile's options argument is mpv's per-file property list, applied
    /// with no vetting: it must never carry anything but `start=<seconds>`.
    /// This is the MPV_SETPROP_ALLOWLIST bypass (stream-record et al.).
    #[test]
    fn loadfile_argv_shape_is_enforced() {
        const URL: &str = "https://cdn.example/stream.mkv";
        for argv in [
            // property smuggling through the options slot (5-arg form)
            vec!["loadfile", URL, "replace", "-1", "stream-record=C:/evil.mkv"],
            vec!["loadfile", URL, "replace", "-1", "sub-file=C:/secrets.txt"],
            vec!["loadfile", URL, "replace", "-1", "audio-file=C:/secrets.wav"],
            vec!["loadfile", URL, "replace", "-1", "vf=lavfi=[movie=C:/x]"],
            // ...and through the legacy 4-arg options slot
            vec!["loadfile", URL, "replace", "stream-record=C:/evil.mkv"],
            // a second option riding along a valid start
            vec!["loadfile", URL, "replace", "-1", "start=+90,stream-record=C:/evil.mkv"],
            // malformed start values
            vec!["loadfile", URL, "replace", "-1", "start=+90x"],
            vec!["loadfile", URL, "replace", "-1", "start=-90"],
            vec!["loadfile", URL, "replace", "-1", "start="],
            vec!["loadfile", URL, "replace", "-1", "start=90."],
            // flags other than replace, a playlist index other than -1
            vec!["loadfile", URL, "append-play"],
            vec!["loadfile", URL, "append", "-1", "start=+90"],
            vec!["loadfile", URL, "replace", "0", "start=+90"],
            // extra arguments past mpv's documented arity
            vec!["loadfile", URL, "replace", "-1", "start=+90", "vf=lavfi=[x]"],
        ] {
            assert!(check_mpv_command(&argv).is_err(), "should block {argv:?}");
        }
    }

    /// The dangerous mpv commands (the S1 RCE surface) must be rejected.
    #[test]
    fn blocks_dangerous_commands() {
        for argv in [
            vec!["run", "cmd.exe", "/c", "calc"],
            vec!["subprocess"],
            vec!["load-script", "C:/evil.lua"],
            vec!["set", "script-opts", "x=1"],
            vec!["loadlist", "playlist.txt"],
            vec!["quit"],
        ] {
            assert!(check_mpv_command(&argv).is_err(), "should block {argv:?}");
        }
        assert!(check_mpv_command(&[]).is_err());
    }

    /// loadfile accepts http(s) and `rillio://`; local paths and mpv protocols
    /// are refused.
    #[test]
    fn loadfile_url_scheme_is_enforced() {
        validate_stream_url("http://127.0.0.1:11470/abc/0").unwrap();
        validate_stream_url("https://example.com/a.mkv").unwrap();
        validate_stream_url("  https://example.com/a.mkv  ").unwrap(); // trimmed
        validate_stream_url("HTTPS://EXAMPLE.COM/A").unwrap(); // scheme case-insensitive
        for bad in [
            "",
            "av://lavfi:testsrc",
            "file:///etc/passwd",
            "C:/Windows/System32/calc.exe",
            "/etc/passwd",
            "\\\\server\\share\\x",
            "edl://!;...",
            "javascript:alert(1)",
        ] {
            assert!(validate_stream_url(bad).is_err(), "should reject {bad:?}");
        }
        // A loadfile command with a bad URL is rejected as a whole.
        assert!(check_mpv_command(&["loadfile", "av://lavfi:testsrc"]).is_err());
    }

    /// The in-process byte plane is allowed, but ONLY in the one spelling the
    /// strict parser accepts. Everything mpv could otherwise be talked into
    /// (a filename segment, a query, an uppercase or wrong-length hash) is a
    /// hard reject, so widening the allowlist did not widen the reachable set.
    const TEST_IH: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn loadfile_accepts_a_well_formed_rillio_url() {
        validate_stream_url(&format!("rillio://{TEST_IH}/0")).unwrap();
        validate_stream_url(&format!("rillio://{TEST_IH}/12")).unwrap();
        validate_stream_url(&format!("  rillio://{TEST_IH}/3  ")).unwrap(); // trimmed
        check_mpv_command(&["loadfile", &format!("rillio://{TEST_IH}/0")]).unwrap();
        check_mpv_command(&["loadfile", &format!("rillio://{TEST_IH}/0"), "replace", "-1", "start=+90"])
            .unwrap();
    }

    #[test]
    fn loadfile_rejects_a_malformed_rillio_url() {
        for bad in [
            // uppercase hex: the grammar is case-sensitive, one spelling only
            format!("rillio://{}/0", TEST_IH.to_ascii_uppercase()),
            // wrong hash length
            format!("rillio://{}/0", &TEST_IH[..39]),
            format!("rillio://{TEST_IH}f/0"),
            // non-hex character
            format!("rillio://{}g/0", &TEST_IH[..39]),
            // extra path, query, fragment
            format!("rillio://{TEST_IH}/0/movie.mkv"),
            format!("rillio://{TEST_IH}/0?tr=http%3A%2F%2Fevil"),
            format!("rillio://{TEST_IH}/0#x"),
            // no file index at all, or not a bare non-negative integer
            format!("rillio://{TEST_IH}"),
            format!("rillio://{TEST_IH}/"),
            format!("rillio://{TEST_IH}/-1"),
            format!("rillio://{TEST_IH}/01"),
            format!("rillio://{TEST_IH}/1a"),
            // scheme lookalikes
            format!("rillio:/{TEST_IH}/0"),
            format!("RILLIO://{TEST_IH}/0"),
            format!("rillio://evil.com/{TEST_IH}/0"),
        ] {
            assert!(validate_stream_url(&bad).is_err(), "should reject {bad:?}");
            assert!(
                check_mpv_command(&["loadfile", &bad]).is_err(),
                "loadfile should reject {bad:?}"
            );
        }
    }

    /// Every property ShellVideo actually sets over the bridge must pass.
    #[test]
    fn allows_the_props_shellvideo_sets() {
        for name in [
            "pause", "speed", "time-pos", "volume", "mute", "aid", "sid", "hwdec", "vo", "osc",
            "input-default-bindings", "input-vo-keyboard", "keepaspect", "panscan",
            "sub-ass-override", "sub-scale", "sub-pos", "sub-delay", "sub-color", "sub-back-color",
            "sub-border-color", "audio-delay",
        ] {
            check_mpv_setprop(name).unwrap_or_else(|e| panic!("{name} should be allowed: {e}"));
        }
    }

    /// Pull one `name=value` out of a `glsl-shader-opts` string.
    fn opt(opts: &str, name: &str) -> f64 {
        opts.split(',')
            .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("{name} missing from {opts:?}"))
            .parse()
            .unwrap()
    }

    fn request(rects: Vec<BlurRect>) -> BlurRequest {
        BlurRequest { rects, viewport: BlurViewport { width: 1600.0, height: 900.0 } }
    }

    /// A 1600x900 CSS viewport on a 2x display, video filling the window: CSS px
    /// map straight through the DPR to normalized output coords.
    #[test]
    fn blur_maps_a_full_window_video() {
        // No letterbox: the video rect IS the 3200x1800 physical window.
        let video = (0.0, 0.0, 3200.0, 1800.0, 3200.0, 1800.0);
        let opts = blur_opts_string(
            video,
            &request(vec![BlurRect {
                x: 800.0,
                y: 450.0,
                width: 400.0,
                height: 225.0,
                corner: 12.0,
            }]),
        )
        .unwrap();
        assert_eq!(opt(&opts, "enabled"), 1.0);
        assert_eq!(opt(&opts, "count"), 1.0);
        // A panel at the viewport's centre lands at the output's centre.
        assert!((opt(&opts, "r0x") - 0.5).abs() < 1e-6);
        assert!((opt(&opts, "r0y") - 0.5).abs() < 1e-6);
        assert!((opt(&opts, "r0w") - 0.25).abs() < 1e-6);
        assert!((opt(&opts, "r0h") - 0.25).abs() < 1e-6);
        // Lengths are CSS px scaled by the measured DPR, not passed through raw.
        assert!((opt(&opts, "radius") - BLUR_RADIUS_CSS_PX * 2.0).abs() < 1e-3);
        assert!((opt(&opts, "r0c") - 24.0).abs() < 1e-3);
        // Unused slots are zeroed, not stale.
        assert_eq!(opt(&opts, "r3w"), 0.0);
    }

    /// The load-bearing case: OUTPUT is the VIDEO rect, so the letterbox bars
    /// have to be subtracted out or every panel sits too low.
    #[test]
    fn blur_accounts_for_letterbox_bars() {
        // 1600x900 CSS at 1x; a 2.39:1 film leaves 180px bars top and bottom.
        let video = (0.0, 180.0, 1600.0, 540.0, 1600.0, 900.0);
        let req = BlurRequest {
            rects: vec![BlurRect { x: 0.0, y: 180.0, width: 1600.0, height: 540.0, corner: 0.0 }],
            viewport: BlurViewport { width: 1600.0, height: 900.0 },
        };
        let opts = blur_opts_string(video, &req).unwrap();
        // A panel covering exactly the video rect maps to the whole output.
        assert!((opt(&opts, "r0x") - 0.0).abs() < 1e-6);
        assert!((opt(&opts, "r0y") - 0.0).abs() < 1e-6);
        assert!((opt(&opts, "r0w") - 1.0).abs() < 1e-6);
        assert!((opt(&opts, "r0h") - 1.0).abs() < 1e-6);

        // A panel up in the top bar maps ABOVE the output (negative), which the
        // shader's SDF intersects away. Deliberately not clamped.
        let opts = blur_opts_string(
            video,
            &BlurRequest {
                rects: vec![BlurRect { x: 0.0, y: 0.0, width: 100.0, height: 100.0, corner: 0.0 }],
                ..req
            },
        )
        .unwrap();
        assert!(opt(&opts, "r0y") < 0.0, "a panel over the top bar must map above the video");
    }

    /// An empty rect list is "stop blurring", and must switch the shader's stages
    /// off rather than blur a zero-sized panel.
    #[test]
    fn blur_disables_on_an_empty_rect_list() {
        let video = (0.0, 0.0, 1600.0, 900.0, 1600.0, 900.0);
        let opts = blur_opts_string(video, &request(vec![])).unwrap();
        assert_eq!(opt(&opts, "enabled"), 0.0);
        assert_eq!(opt(&opts, "count"), 0.0);
    }

    /// Every value must stay inside the range its //!PARAM block declares, or mpv
    /// rejects the whole option string and the blur silently stops updating.
    #[test]
    fn blur_opts_stay_inside_the_declared_param_ranges() {
        // A pathological sliver of video in a huge window drives the normalized
        // coords far out; they must be clamped, not emitted raw.
        let video = (0.0, 0.0, 1.0, 1.0, 4000.0, 4000.0);
        let opts = blur_opts_string(
            video,
            &request(vec![BlurRect {
                x: 1500.0,
                y: 800.0,
                width: 100.0,
                height: 100.0,
                corner: 9999.0,
            }]),
        )
        .unwrap();
        assert!(opt(&opts, "r0x") <= 32.0 && opt(&opts, "r0x") >= -32.0);
        assert!(opt(&opts, "r0y") <= 32.0 && opt(&opts, "r0y") >= -32.0);
        assert!(opt(&opts, "r0w") <= 64.0 && opt(&opts, "r0w") >= 0.0);
        assert!(opt(&opts, "r0h") <= 64.0 && opt(&opts, "r0h") >= 0.0);
        assert!(opt(&opts, "r0c") <= 512.0 && opt(&opts, "r0c") >= 0.0);
        assert!(opt(&opts, "radius") <= 512.0);
    }

    /// A zero-sized viewport (a hidden/collapsed WebView) is nonsense in, not a
    /// division by zero out.
    #[test]
    fn blur_rejects_a_degenerate_viewport() {
        let video = (0.0, 0.0, 1600.0, 900.0, 1600.0, 900.0);
        let req = BlurRequest {
            rects: vec![BlurRect { x: 0.0, y: 0.0, width: 10.0, height: 10.0, corner: 0.0 }],
            viewport: BlurViewport { width: 0.0, height: 900.0 },
        };
        assert!(blur_opts_string(video, &req).is_err());
    }

    /// The shader and the shell must agree on how many rects exist, and on the
    /// parameter names the option string is built from.
    #[test]
    fn blur_shader_matches_the_shell() {
        assert!(
            BLUR_SHADER.contains("#define MAX_RECTS 4") && MAX_BLUR_RECTS == 4,
            "MAX_BLUR_RECTS and the shader's MAX_RECTS have drifted apart"
        );
        for name in ["enabled", "count", "radius"] {
            assert!(BLUR_SHADER.contains(&format!("//!PARAM {name}")), "{name} is not a shader param");
        }
        for i in 0..MAX_BLUR_RECTS {
            for axis in ['x', 'y', 'w', 'h', 'c'] {
                assert!(
                    BLUR_SHADER.contains(&format!("//!PARAM r{i}{axis}")),
                    "r{i}{axis} is not a shader param"
                );
            }
        }
        // Count DIRECTIVES, not substrings: the file's header comment discusses
        // both of these by name, and matching loosely would count the prose too.
        let directives = |d: &str| BLUR_SHADER.lines().filter(|line| line.trim_end() == d).count();
        // Every stage must be skippable, or "loaded but closed" would not be free
        // and the load-once-then-toggle choice would be wrong. Four stages: the
        // quarter-res chain is downsample -> horizontal -> vertical -> composite.
        assert_eq!(directives("//!WHEN enabled 0 >"), 4);
        // The hook the whole HDR argument rests on: OUTPUT is downstream of the
        // colour conversion, so no hook here can disturb passthrough.
        assert_eq!(directives("//!HOOK OUTPUT"), 4);
        // The chain's intermediates: each stage saves what the next one reads,
        // and only the composite touches the real OUTPUT.
        for save in ["//!SAVE RB_DS", "//!SAVE RB_H", "//!SAVE RB_V"] {
            assert_eq!(directives(save), 1, "{save} missing");
        }
        for bind in ["//!BIND RB_DS", "//!BIND RB_H", "//!BIND RB_V"] {
            assert_eq!(directives(bind), 1, "{bind} missing");
        }
    }

    /// Properties outside the player's set (including anything that could reach
    /// scripting/recording/filesystem) are rejected.
    #[test]
    fn blocks_dangerous_props() {
        for name in [
            "start", // shell sets this itself, never over the bridge
            "script-opts",
            "input-conf",
            "stream-record",
            "screenshot-directory",
            "ytdl",
            "sub-file",
            "vf",
        ] {
            assert!(check_mpv_setprop(name).is_err(), "should block {name}");
        }
    }
}
