//! Rillio desktop shell (Tauri v2).
//!
//! S0: a WebView2 window hosting the `apps/web` client.
//! S1: the Rust streaming server runs in-process (no container/sidecar) - the
//! web client reaches it at http://127.0.0.1:11470 exactly as before.

mod autosync;
pub mod mpv;
pub mod platform;
mod shell;
pub mod stream_cb;
mod surface;
mod thumbs;
#[cfg(desktop)]
mod update_window;

use std::sync::Mutex;

use tauri::Manager;

/// The embedded mpv instance (S3). `None` until playback starts.
#[derive(Default)]
struct MpvState(Mutex<Option<mpv::Mpv>>);

/// Set while [`install_update`] is tearing the webview down to run the
/// installer. Destroying the last window fires `RunEvent::ExitRequested`
/// (code None), which must NOT exit the app then: the update task still has to
/// wait for WebView2 to release the profile and hand off to the installer.
#[derive(Default)]
struct UpdateInFlight(std::sync::atomic::AtomicBool);

/// Buffer for OS deep links (stremio:// / rillio://). A link can arrive before
/// the WebView has mounted its listener (cold start: the app is launched BY the
/// link) or while the app is already running (warm: forwarded by the
/// single-instance + deep-link plugin integration). We forward each URL to the
/// web client as the `deep-link-open` signal; until the web reports it is ready
/// (`app-ready` over the shell bridge) we buffer them so a cold-start link is
/// not dropped on the floor.
#[derive(Default)]
struct DeepLinkQueue {
    web_ready: bool,
    pending: Vec<String>,
}

#[derive(Default)]
struct DeepLinkState(Mutex<DeepLinkQueue>);

/// Whether to embed mpv inside the app window (S4 compositing: video renders
/// into the main window behind the transparent WebView, controls overlaid) vs a
/// separate mpv output window. Embedded is the default; opt out with
/// `RILLIO_EMBED_MPV=0` (e.g. if a GPU/driver mishandles the transparent
/// overlay) to get a separate mpv window.
pub(crate) fn mpv_embed_enabled() -> bool {
    !matches!(std::env::var("RILLIO_EMBED_MPV").as_deref(), Ok("0") | Ok("false"))
}

/// Chromium/WebView2 command-line switches for the main window.
///
/// Setting `additional_browser_args` REPLACES wry's defaults, so we re-include
/// them, then turn on DNS-over-HTTPS so the web UI's hostname lookups (addons,
/// image/subtitle CDNs, the update server) are encrypted instead of going out as
/// plaintext DNS. `secure` mode = DoH only, no plaintext fallback. Override the
/// resolver with `RILLIO_DOH_TEMPLATE=<url>`, or disable with `=off` (e.g. if a
/// network blocks the DoH endpoint and breaks resolution). NOTE: DoH does not
/// hide your IP from torrent peers (that needs a VPN/proxy) - see
/// memory/compositing-dcomp-plan sibling notes.
fn browser_args() -> String {
    // NOTE: Tracking Prevention (the 2026-08-16 dead-storage incident suspect)
    // is NOT controllable from here. It is governed by
    // ICoreWebView2EnvironmentOptions5::EnableTrackingPrevention (hard-defaulted
    // to true by webview2-com, never set by wry, so no browser arg can reach it)
    // and by the per-profile level, which disable_tracking_prevention() below
    // sets to None at runtime - the supported switch per Microsoft's
    // TrackingPrevention spec. The ms* feature names v0.1.30 shipped here were
    // inert guesses and have been removed.
    let base = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";
    let dns = match std::env::var("RILLIO_DOH_TEMPLATE") {
        Ok(v) if v == "0" || v.eq_ignore_ascii_case("off") => base.to_string(),
        Ok(v) if !v.trim().is_empty() => {
            format!("{base} --dns-over-https-mode=secure --dns-over-https-templates={}", v.trim())
        }
        _ => format!(
            "{base} --dns-over-https-mode=secure --dns-over-https-templates=https://cloudflare-dns.com/dns-query"
        ),
    };
    // Debug knob: opens a CDP endpoint on the WebView so the running shell's real
    // DOM/console can be inspected from outside. The shell's own DOM is the only
    // way to settle bugs that reproduce here but not in a browser. OFF unless the
    // env var is set: an open CDP port is local code execution in the page.
    match std::env::var("RILLIO_DEVTOOLS_PORT") {
        Ok(p) if !p.trim().is_empty() => format!("{dns} --remote-debugging-port={}", p.trim()),
        _ => dns,
    }
}

/// Schemes the shell will hand to the OS default handler (S2). This is a strict
/// allowlist: `open::that` is a shell-execute, so passing an arbitrary
/// webview-supplied string would let addon-driven content launch local programs
/// (`file:///C:/...exe`, a UNC `\\server\share\x.exe`, or any registered
/// protocol handler like `ms-msdt:`). Only the schemes the web client's
/// `openExternal` and custom-scheme navigations legitimately produce are
/// allowed; everything else is refused.
///
/// Inventory (evidence): `apps/web` calls `platform.openExternal` with http/https
/// (addon configure, addon directory, Trakt/Facebook/Apple login, password reset,
/// calendar .ics, data export, stream download + subtitle URLs) and `webcal`
/// (iOS calendar, Settings/General). `magnet:` and the external-player deep-link
/// schemes come from `crates/core/src/deep_links` (ExternalPlayerLink /
/// OpenPlayerLink): mpv, iina, infuse, vidhub, outplayer, moonplayer, VLC's
/// x-callback and android `intent://`. `mailto:` is a standard safe handoff.
/// (On the Windows shell only http/https/magnet are exercised today; the rest
/// keep the cross-platform openExternal contract intact and fail closed.)
fn is_allowed_external_scheme(scheme: &str) -> bool {
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http"
            | "https"
            | "magnet"
            | "mailto"
            | "webcal"
            // external media players (crates/core deep_links)
            | "mpv"
            | "iina"
            | "infuse"
            | "open-vidhub"
            | "outplayer"
            | "moonplayer"
            | "vlc-x-callback"
            | "intent"
    )
}

/// Validate a raw external-open URL: it must parse as an absolute URL and carry
/// an allowed scheme. Fails CLOSED, a string that does not parse (schemeless /
/// relative paths, a bare `C:\...` path, or a `\\server\share` UNC path) is
/// refused rather than passed to the OS.
fn validate_external_url(url: &str) -> Result<(), String> {
    let parsed = tauri::Url::parse(url)
        .map_err(|e| format!("open_external: refusing unparseable url {url:?}: {e}"))?;
    if is_allowed_external_scheme(parsed.scheme()) {
        Ok(())
    } else {
        Err(format!(
            "open_external: refusing disallowed scheme {:?} ({url:?})",
            parsed.scheme()
        ))
    }
}

/// Open a URL in the OS default handler / native app (S2).
///
/// This is the desktop implementation of the web client's
/// `platform.openExternal`. Running in the trusted shell, it opens the target
/// directly (external player, torrent client, browser) instead of the browser's
/// `window.open` + safety-warning redirect. The scheme is checked against
/// [`is_allowed_external_scheme`] first so hostile webview content cannot use
/// this to shell-execute a local file or an arbitrary protocol handler.
#[tauri::command]
fn open_external(url: String) -> Result<(), String> {
    validate_external_url(&url)?;
    open::that(&url).map_err(|e| format!("open_external({url}): {e}"))
}

/// The only schemes we accept from the OS. The deep-link plugin is configured
/// for exactly these (see tauri.conf.json), but deep links are untrusted input,
/// so we re-check the scheme at the boundary before forwarding anything.
fn is_deep_link_scheme(scheme: &str) -> bool {
    matches!(scheme.to_ascii_lowercase().as_str(), "stremio" | "rillio")
}

/// Forward one deep-link URL to the web client. We reuse the existing shell
/// signal bus (`shell-signal` carries `{event, payload}`; useShell re-emits it
/// as the named event), so the web listens with `shell.on('deep-link-open')`.
/// The web side (DeepLinkOpenHandler) validates + routes it.
fn emit_deep_link(app: &tauri::AppHandle, url: &str) {
    use tauri::Emitter;
    if let Err(e) = app.emit(
        "shell-signal",
        serde_json::json!({ "event": "deep-link-open", "payload": url }),
    ) {
        tracing::warn!("deep-link: emit failed for {url}: {e}");
    }
}

/// Accept a deep link from the OS: emit it now if the web client is ready,
/// otherwise buffer it until `app-ready`. The scheme is assumed pre-checked by
/// the caller (via [`is_deep_link_scheme`]).
fn queue_or_emit_deep_link(app: &tauri::AppHandle, url: &str) {
    let ready = {
        let state = app.state::<DeepLinkState>();
        let mut q = state.0.lock().unwrap();
        if q.web_ready {
            true
        } else {
            tracing::info!("deep-link: buffering {url} until web is ready");
            q.pending.push(url.to_string());
            false
        }
    };
    if ready {
        tracing::info!("deep-link: forwarding {url}");
        emit_deep_link(app, url);
    }
}

/// Called when the web client reports it has mounted its listeners (`app-ready`
/// over the shell bridge, see `shell::shell_send`). Marks the web ready and
/// flushes any deep links that arrived during startup.
pub(crate) fn mark_web_ready_and_flush(app: &tauri::AppHandle) {
    let pending: Vec<String> = {
        let state = app.state::<DeepLinkState>();
        let mut q = state.0.lock().unwrap();
        q.web_ready = true;
        std::mem::take(&mut q.pending)
    };
    for url in pending {
        tracing::info!("deep-link: flushing buffered {url}");
        emit_deep_link(app, &url);
    }
}

/// Register the OS-deep-link handlers. `get_current()` captures a cold-start
/// launch URL (the app was opened BY the link); `on_open_url` fires for links
/// opened while running (delivered here by the single-instance `deep-link`
/// integration on Windows). Both funnel through [`queue_or_emit_deep_link`].
///
/// NOTE: we deliberately do NOT call `register_all()` here. That writes the
/// scheme handlers into the Windows registry at runtime, which would hijack the
/// machine's real `stremio://` handler to point at a dev build. Production
/// registration is done by the NSIS installer from the `plugins.deep-link`
/// config in tauri.conf.json.
/// Android build: OS deep links arrive through the manifest intent-filter, not
/// this desktop plugin (which is gated out). Wired in Phase 2.
#[cfg(not(desktop))]
fn setup_deep_links(_app: &tauri::App) {}

#[cfg(desktop)]
fn setup_deep_links(app: &tauri::App) {
    use tauri_plugin_deep_link::DeepLinkExt;

    if let Ok(Some(urls)) = app.deep_link().get_current() {
        for url in urls {
            if is_deep_link_scheme(url.scheme()) {
                queue_or_emit_deep_link(app.handle(), url.as_str());
            }
        }
    }

    let handle = app.handle().clone();
    app.deep_link().on_open_url(move |event| {
        for url in event.urls() {
            if is_deep_link_scheme(url.scheme()) {
                queue_or_emit_deep_link(&handle, url.as_str());
            } else {
                tracing::warn!("deep-link: ignoring non-stremio/rillio url {url}");
            }
        }
    });
}

/// Build and run the Tauri application. On mobile there is no `main()`: the
/// Android Activity loads this `.so` via JNI and calls the entry point the
/// `mobile_entry_point` macro generates. On desktop `main.rs` calls it directly.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,rillio_desktop_lib=debug".into()),
        )
        .try_init();

    // Must run BEFORE any WebView2 initialization (i.e. before Builder::run),
    // otherwise the running WebView2 holds its own cache dirs open and they
    // cannot be deleted. Uses the context (available before .run()) for the
    // identifier + version.
    //
    // Host-lifecycle seam: this whole stale-cache sweep is a Windows-WebView2
    // concern (a platform without a persistent WebView2 profile - Android's
    // System WebView, updated by the store - has nothing to sweep). Gated on the
    // capability rather than `#cfg` so the seam is one readable switch; on
    // Windows the cap is true and this runs exactly as before.
    let ctx = tauri::generate_context!();

    // Dev/installed profile separation (2026-08-17): a cargo-run dev shell and
    // the installed app used to share one identifier and therefore ONE WebView2
    // profile and browser-process pool. Interleaving them multiplied the
    // rapid-relaunch churn behind the dead-storage sessions (see the profile
    // lock wait in setup). Debug builds now get their own identifier: own
    // profile dir (%LOCALAPPDATA%\com.rillio.desktop.dev), own single-instance
    // mutex, zero contact with the user's real data. RILLIO_SHARED_PROFILE=1
    // deliberately opts a dev build back into the real profile when a bug only
    // reproduces against real data - run it strictly INSTEAD of the installed
    // app, never alongside it. Release builds are untouched.
    #[cfg(debug_assertions)]
    let ctx = {
        let mut ctx = ctx;
        let shared = std::env::var("RILLIO_SHARED_PROFILE")
            .map(|v| v.trim() == "1")
            .unwrap_or(false);
        if !shared {
            ctx.config_mut().identifier = "com.rillio.desktop.dev".into();
        }
        ctx
    };

    // Process-birth journal line: written for EVERY rillio process (main app,
    // update splash, second instances that single-instance will kill) BEFORE
    // any branching, so a boot that dies pre-setup still leaves a trace. Four
    // field refusals (2026-08-17/18) left zero journal lines while healthy
    // boots always journaled; whatever distinguishes those processes, this
    // line records it: how it was launched (args), whether the environment is
    // intact, and where it started from.
    #[cfg(desktop)]
    boot_journal_append(
        &ctx.config().identifier,
        &format!(
            "run-start v{} pid={} args={:?} localappdata={} cwd={:?}",
            ctx.package_info().version,
            std::process::id(),
            std::env::args().skip(1).collect::<Vec<_>>(),
            std::env::var_os("LOCALAPPDATA").is_some(),
            std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_else(|e| format!("<{e}>"))
        ),
    );

    // STALE-IMAGE SELF-HEAL (2026-08-19): the v0.1.38 update relaunch produced
    // a process that ran for minutes with UI yet left ZERO journal lines in
    // any location, including the unconditional run-start above - only
    // possible if the running IMAGE predates the journal code even though the
    // file on disk is current (checks against the disk file, like
    // MainModule.FileVersionInfo, cannot see this). Whatever race hands the
    // update relaunch an old image, the old image can notice and hand over:
    // compare the compiled-in version against the version resource of the exe
    // file on disk; on mismatch, relaunch the disk binary and exit. Matching
    // versions (every normal boot) cost one version-resource read. Errors
    // reading the resource boot normally - never brick startup over the check.
    #[cfg(all(desktop, windows))]
    if std::env::args().all(|arg| arg != "--update-window") {
        if let Ok(exe) = std::env::current_exe() {
            // Compare against the CANONICAL install-path name, not current_exe
            // itself: if the installer renamed the running old exe aside,
            // current_exe follows the rename (and would read the old file, a
            // false match); the canonical name in the same directory is what
            // the next launch runs. For a normally-named process the two paths
            // are the same file.
            let canonical = exe.with_file_name("rillio-desktop.exe");
            if canonical.exists() {
                if let Some(disk_version) = read_disk_file_version(&canonical) {
                    let built_version = ctx.package_info().version.to_string();
                    if disk_version != built_version {
                        boot_journal_append(
                            &ctx.config().identifier,
                            &format!(
                                "stale-image built=v{built_version} disk=v{disk_version}; relaunching {}",
                                canonical.display()
                            ),
                        );
                        let _ = std::process::Command::new(&canonical).spawn();
                        std::process::exit(0);
                    }
                }
            }
        }
    }

    // The detached update-window process mode (docs/update-window): a minimal
    // one-window app that shows update.html while the real update runs. It must
    // branch BEFORE the stale-cache sweep below - that sweep touches the MAIN
    // app's WebView2 profile, which is alive and in use when this spawns.
    #[cfg(desktop)]
    if std::env::args().any(|arg| arg == "--update-window") {
        update_window::run(ctx);
        return;
    }
    // Normal launch: remove any update progress file. During an update this is
    // the "new version is up" signal the update window waits for; any other
    // leftover is stale.
    #[cfg(desktop)]
    let _ = std::fs::remove_file(update_window::progress_path());
    // ...and the staged updater copy that ran the splash (see update_window).
    #[cfg(desktop)]
    update_window::cleanup_updater_copy();

    if platform::PlatformCaps::current().webview2_cache {
        clear_stale_webview_cache(
            ctx.config().identifier.clone(),
            ctx.package_info().version.to_string(),
        );
    }

    let builder = tauri::Builder::default();
    // Desktop-only plugins (single-instance, deep-link, updater). On Android
    // none apply: the app store handles updates, launchMode=singleTask handles
    // single-instance, and deep links are manifest-driven. Gated so the Android
    // build has no reference to the (non-existent there) plugin crates.
    #[cfg(desktop)]
    let builder = builder
        // Single-instance MUST be the first plugin registered (Tauri requirement).
        // On a second launch, focus the running window instead of starting a
        // second shell that would fail to bind :11470 and clobber the WebView2
        // profile the first one is using.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        // Deep-link plugin MUST be registered after single-instance (above) so
        // the single-instance `deep-link` feature can forward a warm-launch URL
        // into this plugin's on_open_url. See setup_deep_links.
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build());
    builder
        .manage(MpvState::default())
        .manage(UpdateInFlight::default())
        .manage(shell::ShellState::default())
        .manage(thumbs::ThumbsState::default())
        .manage(DeepLinkState::default())
        // Filled in once the server binds (see start_streaming_server). Managed
        // up front so `streaming_server_url` can answer "not yet" instead of
        // erroring while the web client polls.
        .manage(ServerHandle::default())
        .setup(|app| {
            // The streaming server is cross-platform (loopback HTTP); runs on
            // every target.
            start_streaming_server(app.handle());
            // Desktop builds the frameless/transparent main window in code (the
            // builder options below are desktop-only). On Android the WebView is
            // created by Tauri from the mobile config; that window setup is a
            // Phase 2 runtime task (needs a device to exercise).
            #[cfg(desktop)]
            {
                // ROOT-CAUSE FIX for the dead-storage sessions (2026-08-17,
                // reproduced deterministically by holding the leveldb LOCK
                // during boot): when a new browser process starts while the
                // previous one is still tearing down and holding the Local
                // Storage LOCK, Chromium's storage service fails to open the
                // database and silently serves an EMPTY in-memory map for the
                // whole session - reads empty, writes dropped, looks like a
                // wiped profile. localStorage has no error channel, so nothing
                // ever surfaces. Wait for the previous session to release the
                // profile BEFORE creating the webview; on timeout boot anyway
                // (the storage guard remains the safety net).
                let identifier = app.config().identifier.clone();
                let profile = std::env::var_os("LOCALAPPDATA").map(|local| {
                    std::path::Path::new(&local).join(&identifier).join("EBWebView")
                });
                let (lockfile_held, ldb_held) = profile
                    .map(|p| {
                        (
                            lock_held(&p.join("lockfile")),
                            lock_held(&p.join("Default").join("Local Storage").join("leveldb").join("LOCK")),
                        )
                    })
                    .unwrap_or((false, false));
                let waited = std::time::Instant::now();
                let released = wait_for_webview_profile_release(&identifier);
                if !released {
                    tracing::warn!("previous browser session still holds the profile after 10s; booting anyway");
                } else if waited.elapsed().as_millis() > 300 {
                    tracing::info!(
                        "waited {:?} for the previous browser session to release the profile",
                        waited.elapsed()
                    );
                }
                boot_journal_append(
                    &identifier,
                    &format!(
                        "boot v{} lockfile_held={} ldb_lock_held={} waited_ms={} released={}",
                        app.package_info().version,
                        lockfile_held,
                        ldb_held,
                        waited.elapsed().as_millis(),
                        released
                    ),
                );
                let window = build_main_window(app)?;
                // S3 part-2 render proof: RILLIO_MPV_TEST=<url|"test"> embeds mpv
                // in the window and plays it. "test" = a generated color pattern.
                if let Ok(src) = std::env::var("RILLIO_MPV_TEST") {
                    if let Err(e) = start_mpv_embedded(app.handle(), &window, &src) {
                        tracing::error!("mpv embed test failed: {e}");
                    }
                }
            }
            // Mobile: with `app.windows` empty in the config, Tauri creates no
            // window on its own (desktop builds one in build_main_window, which is
            // cfg(desktop)-only). Without this the Android Activity shows only a
            // black surface. See build_mobile_window.
            #[cfg(mobile)]
            {
                build_mobile_window(app)?;
            }
            spawn_update_check(app.handle().clone());
            setup_deep_links(app);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            open_external,
            install_update,
            check_for_update,
            storage_health,
            storage_guard_report,
            restart_app,
            snapshot_storage,
            streaming_server_url,
            server_request,
            shell::shell_init,
            shell::shell_send,
            shell::shell_mpv_stats,
            shell::player_snapshot,
            shell::player_blur_rect,
            thumbs::player_thumb,
            thumbs::player_thumb_stop,
            thumbs::player_scene_cuts,
            autosync::subtitles_autosync
        ])
        .build(ctx)
        .expect("error while building the Rillio desktop shell")
        // The run callback exists for exactly one reason: during an update,
        // install_update destroys the main window BEFORE running the installer
        // (see the incident note there), and destroying the last window
        // requests an exit (code None). Exiting then would kill the update
        // task mid-handoff, so it is prevented while UpdateInFlight is set.
        // Programmatic exits (code Some, e.g. app.restart()) always proceed.
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { code, api, .. } = &event {
                let updating = app_handle
                    .state::<UpdateInFlight>()
                    .0
                    .load(std::sync::atomic::Ordering::SeqCst);
                if code.is_none() && updating {
                    api.prevent_exit();
                }
            }
        });
}

/// Load mpv, embed it into the window (`wid`), and play `source`. Stores the
/// instance in state so it isn't dropped. Windows-only for now.
#[cfg(windows)]
fn start_mpv_embedded(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    source: &str,
) -> Result<(), String> {
    let hwnd = window.hwnd().map_err(|e| e.to_string())?;
    let wid = hwnd.0 as isize;

    let dll = mpv::default_dll_path();
    let mpv = mpv::Mpv::load(&dll)?;
    mpv.set_option("wid", &wid.to_string())?; // render into our window
    mpv.set_option("hwdec", "auto")?; // hardware decode (HDR/perf)
    mpv.initialize()?;

    let file = if source == "test" {
        "av://lavfi:testsrc=size=1280x720:rate=30"
    } else {
        source
    };
    mpv.command(&["loadfile", file])?;

    *app.state::<MpvState>().0.lock().unwrap() = Some(mpv);
    tracing::info!("mpv embedded (wid={wid}), playing {file}");
    Ok(())
}

#[cfg(not(windows))]
fn start_mpv_embedded(
    _app: &tauri::AppHandle,
    _window: &tauri::WebviewWindow,
    _source: &str,
) -> Result<(), String> {
    Err("mpv embedding is Windows-only for now".into())
}

/// Create the main window in code so we can intercept navigation: custom-scheme
/// links (`vlc://`, `mpv://`, `magnet:`, external-player deep links) are opened
/// in the OS/native app and the in-app navigation is cancelled (S2). Normal
/// http(s)/tauri navigations proceed in the WebView.
/// Root fix for the "update installs but the UI is still the old one" bug: the
/// embedded web bundle registers a cache-first service worker, and its asset
/// path is prefixed with the commit hash, so after the native updater swaps in a
/// new bundle the stale SW + HTTP cache keep serving the OLD UI. Before the
/// WebView loads, if the shell's version changed since last run (fresh install
/// or a just-applied update), delete WebView2's service-worker + HTTP caches so
/// the fresh embedded assets always win. Best-effort: any error just logs and
/// the web-side self-heal (apps/web index.js) remains as a backstop.
///
/// INCIDENT NOTE (2026-07-13): when the 0.1.16 -> 0.1.17 update wiped a user's
/// profile/library, this function was the first suspect and was proven INNOCENT
/// (the real cause was the updater's hard process exit, see install_update).
/// Keep it innocent: ALL user data lives in this same profile ("Local Storage"
/// is the only copy of profile/library/settings; also "IndexedDB",
/// "WebStorage", "Session Storage"). NEVER add a user-data directory to the
/// list below, and never delete `Default` or `EBWebView` wholesale. The three
/// entries below are pure caches and are the ONLY safe deletions.
fn clear_stale_webview_cache(identifier: String, current: String) {
    // %LOCALAPPDATA%\<identifier> is where WebView2 keeps its EBWebView profile.
    let local = match std::env::var_os("LOCALAPPDATA") {
        Some(dir) => dir,
        None => return,
    };
    let base = std::path::Path::new(&local).join(&identifier);
    let marker = base.join("web-bundle-version");
    // `current` is the tauri.conf.json version (bumped every release), not
    // Cargo.toml's CARGO_PKG_VERSION (a constant 0.1.0), so it changes on update.
    let previous = std::fs::read_to_string(&marker).unwrap_or_default();
    if previous.trim() == current {
        return;
    }
    let default_profile = base.join("EBWebView").join("Default");
    // NOTE: this dir list is duplicated in CLAUDE.md's dev-loop cache-clear
    // snippet ("Service Worker", "Cache", "Code Cache"); keep both in sync.
    let mut all_cleared = true;
    for sub in ["Service Worker", "Cache", "Code Cache"] {
        let path = default_profile.join(sub);
        if !path.exists() {
            continue;
        }
        // std::fs::remove_dir_all refuses to traverse the reparse points inside
        // WebView2's cache dirs (os error 4395), so shell out to `rmdir /S /Q`,
        // which removes them the way Explorer/PowerShell do. Windows-only shell.
        #[cfg(windows)]
        let removed = std::process::Command::new("cmd")
            .args(["/C", "rmdir", "/S", "/Q"])
            .arg(&path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        #[cfg(not(windows))]
        let removed = std::fs::remove_dir_all(&path).is_ok();
        if removed && !path.exists() {
            tracing::info!("cache-clear: removed {}", path.display());
        } else {
            all_cleared = false;
            tracing::warn!("cache-clear: could not remove {}", path.display());
        }
    }
    // Only advance the version marker once the stale caches are actually gone. If
    // any removal failed, leave the marker unwritten so the next launch retries
    // the clear, writing it now would strand a half-deleted cache serving the old
    // bundle forever.
    if !all_cleared {
        tracing::warn!(
            "cache-clear: INCOMPLETE for version {current}; leaving marker unwritten to retry next launch"
        );
        return;
    }
    let _ = std::fs::create_dir_all(&base);
    if let Err(e) = std::fs::write(&marker, &current) {
        tracing::warn!("cache-clear: could not write version marker: {e}");
    }
}

/// Mobile only: create the WebView that hosts the shared web UI. The config's
/// `app.windows` is empty (desktop makes its frameless window in code), so on
/// Android nothing would be shown otherwise. None of the desktop chrome applies
/// here (the OS owns the frame, sizing and back button), so this is deliberately
/// minimal: a single fullscreen WebView pointed at the bundled index, with the
/// same navigation allowlist as desktop so a custom-scheme link in addon content
/// can never navigate the WebView to `file:`/unknown protocols. External-scheme
/// launching (magnet:, external players) is a later Android task (needs an OS
/// intent, not the `open` crate), so for now non-web schemes are just blocked.
#[cfg(mobile)]
fn build_mobile_window(app: &tauri::App) -> tauri::Result<tauri::WebviewWindow> {
    tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::default())
        .title("Rillio")
        // Transparent WebView background (wry setBackgroundColor(0)): the video
        // SurfaceView sits UNDERNEATH the WebView (MainActivity.kt) and must show
        // through during playback, mirroring the Windows transparent-window
        // embedding. The app's own dark theme keeps pages opaque otherwise.
        .transparent(true)
        .on_navigation(|url| match url.scheme() {
            "http" | "https" | "tauri" | "data" | "blob" | "about" => true,
            scheme => {
                tracing::warn!("blocked navigation to non-web scheme {scheme:?}: {url}");
                false
            }
        })
        .build()
}

/// Desktop only: builds the frameless, transparent main window in code. The
/// builder options here (decorations, transparent, additional_browser_args) are
/// desktop-specific; on Android the WebView comes from the mobile config.
#[cfg(desktop)]
fn build_main_window(app: &tauri::App) -> tauri::Result<tauri::WebviewWindow> {
    // In-window mpv compositing (S4) is opt-in behind RILLIO_EMBED_MPV: on
    // Windows a transparent (layered) top-level window does NOT display child
    // windows that render with the GPU, so mpv's gpu-next output embedded via
    // `wid` renders to an invisible surface. Until that's solved properly, the
    // default is a non-transparent window + mpv in its own output window (which
    // works). See mpv_embed_enabled().
    // RILLIO_START_URL overrides the initial page (debug hook - e.g. point it at
    // a DoH check page to verify DNS encryption is active).
    let start_url = std::env::var("RILLIO_START_URL")
        .ok()
        .and_then(|u| tauri::Url::parse(&u).ok())
        .map(tauri::WebviewUrl::External)
        .unwrap_or_default();
    let window = tauri::WebviewWindowBuilder::new(app, "main", start_url)
        // Version in the native title (taskbar/alt-tab only - the window is
        // frameless): makes the RUNNING image's version externally readable
        // (Get-Process MainWindowTitle), which the stale-image incident showed
        // nothing derived from the on-disk file can be trusted to reveal.
        .title(format!("Rillio {}", app.package_info().version))
        .inner_size(1280.0, 800.0)
        .resizable(true)
        // Frameless: the web app draws its own window controls + drag region
        // (apps/web WindowControls), gated to the shell. Edge-resize still works.
        .decorations(false)
        // Created hidden; the web loading screen calls window.show() once it has
        // painted (index.html), so the transparent window never flashes the
        // desktop through. A Rust fallback below reveals it if that never fires.
        .visible(false)
        .transparent(mpv_embed_enabled())
        .additional_browser_args(&browser_args())
        .on_navigation(|url| {
            match url.scheme() {
                // In-WebView navigations (app pages, data/blob assets).
                "http" | "https" | "tauri" | "data" | "blob" | "about" => true,
                // A custom-scheme link (external player, magnet, ...). Hand it to
                // the OS ONLY if the scheme is allowlisted; otherwise block it so
                // a file:/unknown-protocol link in addon content cannot
                // shell-execute a local program. Never navigate the WebView to it.
                scheme => {
                    if is_allowed_external_scheme(scheme) {
                        if let Err(e) = open::that(url.as_str()) {
                            tracing::error!("failed to open external {url}: {e}");
                        }
                    } else {
                        tracing::warn!("blocked navigation to disallowed scheme {scheme:?}: {url}");
                    }
                    false
                }
            }
        })
        .build()?;

    // Self-heal a stuck fullscreen state: if a previous session died while
    // fullscreen, the OS can restore the window fullscreen, which silently
    // disables resizing and Windows' drag-to-top (Aero snap) maximize. The app
    // always starts windowed; fullscreen is only entered via its header button.
    let _ = window.set_fullscreen(false);

    #[cfg(windows)]
    disable_tracking_prevention(&window);

    // Fallback reveal: if the web layer never calls show() (e.g. a startup error
    // before the loading screen paints), don't leave an invisible window. show()
    // is idempotent, so racing the JS path is harmless.
    let fallback = window.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let _ = fallback.show();
    });

    Ok(window)
}

/// Turns Edge Tracking Prevention OFF for the app's WebView2 profile - the
/// prime suspect in the 2026-08-16 dead-storage incident ("Tracking Prevention
/// blocked access to storage" x12 in the stuck console; localStorage reads
/// answered empty, writes dropped, leveldb never opened). The app IS the site
/// here; TP has nothing to protect and can classify the app's own origin.
///
/// Why this shape: the feature-level switch
/// (ICoreWebView2EnvironmentOptions5::EnableTrackingPrevention) is set at
/// environment creation inside wry, which never exposes it, and webview2-com
/// defaults it to true - no browser argument reaches it (v0.1.30's ms*
/// --disable-features names were inert). The per-profile level IS reachable
/// post-creation and, per Microsoft's TrackingPrevention spec, level None
/// fully disables TP even with the environment option left enabled, applies
/// immediately, and is PERSISTED in the user data folder - so from the second
/// boot on the profile starts with TP already off, closing the small window
/// where this call races the page's first storage reads (the storage guard's
/// auto-retry covers that first boot).
///
/// Failure here is logged loudly and never fatal: the cast to
/// ICoreWebView2_13/Profile3 only fails on a runtime older than ~108 (we ship
/// against much newer), and the app must still boot for the guard to handle a
/// bad session.
#[cfg(windows)]
fn disable_tracking_prevention(window: &tauri::WebviewWindow) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Profile3, ICoreWebView2_13, COREWEBVIEW2_TRACKING_PREVENTION_LEVEL_NONE,
    };
    use windows::core::Interface;

    let result = window.with_webview(|webview| {
        let attempt = unsafe {
            (|| -> windows::core::Result<()> {
                let core = webview.controller().CoreWebView2()?;
                let core13: ICoreWebView2_13 = core.cast()?;
                let profile: ICoreWebView2Profile3 = core13.Profile()?.cast()?;
                profile
                    .SetPreferredTrackingPreventionLevel(COREWEBVIEW2_TRACKING_PREVENTION_LEVEL_NONE)
            })()
        };
        match attempt {
            Ok(()) => tracing::info!(
                "tracking prevention set to None for the profile (persisted across sessions)"
            ),
            Err(e) => tracing::error!("failed to disable tracking prevention: {e}"),
        }
    });
    if let Err(e) = result {
        tracing::error!("with_webview failed while disabling tracking prevention: {e}");
    }
}

/// On launch, ask GitHub Releases (see `plugins.updater.endpoints` in
/// tauri.conf.json) whether a newer signed build exists. If so, emit
/// `update-available` with the version so the web UI can surface a toast (see
/// apps/web ServicesToaster); the user installs it from there via
/// [`install_update`]. Runs on every startup, so the toast reappears until the
/// update is taken. Fails quietly: no release yet / offline / an unconfigured
/// signing key all just log at debug and leave the running app untouched.
/// Android build: updates come from the app store, not the in-app updater
/// (gated out). No-op.
#[cfg(not(desktop))]
fn spawn_update_check(_app: tauri::AppHandle) {}

#[cfg(desktop)]
fn spawn_update_check(app: tauri::AppHandle) {
    use tauri::Emitter;
    use tauri_plugin_updater::UpdaterExt;

    tauri::async_runtime::spawn(async move {
        let updater = match app.updater() {
            Ok(updater) => updater,
            // A missing/invalid pubkey surfaces here as Err, not a panic.
            Err(e) => {
                tracing::debug!("updater unavailable: {e}");
                return;
            }
        };

        match updater.check().await {
            Ok(Some(update)) => {
                tracing::info!("update {} available", update.version);
                let _ = app.emit("update-available", update.version.clone());
            }
            Ok(None) => tracing::debug!("rillio is up to date"),
            Err(e) => tracing::debug!("update check failed: {e}"),
        }
    });
}

/// Ask the updater whether a newer build exists, ON DEMAND (Settings ->
/// "Check for updates"). [`spawn_update_check`] only runs at launch and only
/// speaks through a toast, which is easy to miss and impossible to summon
/// again without restarting - this is the deliberate path.
///
/// Returns the available version, or `None` when up to date. Errors are
/// returned rather than swallowed: a user who explicitly asked has to be told
/// that the check itself failed (offline, no release yet), otherwise "nothing
/// happened" reads as "up to date".
#[tauri::command]
async fn check_for_update(app: tauri::AppHandle) -> Result<Option<String>, String> {
    // Same shape as install_update: one entry in the handler list, desktop-only
    // body (Android takes updates from the app store).
    #[cfg(not(desktop))]
    {
        let _ = app;
        return Ok(None);
    }
    #[cfg(desktop)]
    {
        use tauri_plugin_updater::UpdaterExt;

        let update = app
            .updater()
            .map_err(|e| e.to_string())?
            .check()
            .await
            .map_err(|e| e.to_string())?;
        Ok(update.map(|update| update.version.clone()))
    }
}

/// Download, verify (minisign) and install the pending update, then relaunch.
/// Invoked from the web UI's update toast and from Settings. Re-checks so it
/// never installs a stale handle.
///
/// INCIDENT (2026-07-13, the 0.1.16 -> 0.1.17 auto-update WIPED a user's
/// profile/library/settings): tauri-plugin-updater's install step launches the
/// NSIS installer and then calls `std::process::exit(0)` immediately, with the
/// WebView2 child processes still alive and possibly mid-write. The abandoned
/// browser process then shuts down asynchronously, racing the installer and the
/// relaunched app over the EBWebView profile; Chromium "recovered" the profile's
/// Local Storage leveldb (the ONLY copy of all user data, see
/// crates/core-web env.rs local_storage_*) by destroying and recreating it
/// EMPTY. Forensics: leveldb LOG showed the db reopening as a fresh generation
/// ("Recovering log #3") while orphaned old-generation tables (000140-000144)
/// survived only because their handles were still open when the destroy ran.
///
/// The fix: download first, then DESTROY the webview window (graceful WebView2
/// shutdown -> storage service flushes and exits) and WAIT until the browser
/// releases the Local Storage lock, and only then hand off to the installer.
/// The webview must never be alive when the process exits for an update.
#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    // Stays in the invoke_handler list on every target (the generate_handler
    // macro can't cfg an entry), but the body is desktop-only: Android takes
    // updates from the app store, so the web toast is never wired there.
    #[cfg(not(desktop))]
    {
        let _ = app;
        return Err("in-app updates are unavailable on this platform".into());
    }
    #[cfg(desktop)]
    {
    use tauri_plugin_updater::UpdaterExt;

    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no update available".to_string())?;

    // The custom update window (docs/update-window): a detached process that
    // shows the liquid progress page through download AND install (this process
    // exits at install handoff, so an in-process window could not). The main
    // window hides immediately - the small window IS the update experience.
    // Presentation only: if the splash fails to spawn, the update still
    // proceeds (headless) and ends in the relaunch either way.
    update_window::write_progress(&update_window::UpdateProgress {
        phase: "downloading".into(),
        downloaded: 0,
        total: 0,
        message: None,
    });
    update_window::spawn_update_window();
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }

    // Stream download progress to the update window's progress file (throttled;
    // the file poller runs at 150ms). The update window is the ONLY progress
    // surface - the main window is hidden and the web UI's overlay is gone.
    // `content_len` is the total size when known.
    let mut downloaded: u64 = 0;
    let mut last_file_write = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let download_result = update
        .download(
            move |chunk_len, content_len| {
                downloaded += chunk_len as u64;
                if last_file_write.elapsed() >= std::time::Duration::from_millis(100) {
                    last_file_write = std::time::Instant::now();
                    update_window::write_progress(&update_window::UpdateProgress {
                        phase: "downloading".into(),
                        downloaded,
                        total: content_len.unwrap_or(0),
                        message: None,
                    });
                }
            },
            || {},
        )
        .await;
    let bytes = match download_result {
        Ok(bytes) => bytes,
        Err(e) => {
            // Failed download: tell the update window (it shows the error and
            // exits), bring the main window back, and surface the error to the
            // web UI's toast as before.
            update_window::write_progress(&update_window::UpdateProgress {
                phase: "error".into(),
                downloaded: 0,
                total: 0,
                message: Some(e.to_string()),
            });
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
            return Err(e.to_string());
        }
    };
    update_window::write_progress(&update_window::UpdateProgress {
        phase: "installing".into(),
        downloaded: 0,
        total: 0,
        message: None,
    });

    // From here on the webview goes away, so this command's JS response will
    // never be delivered - that's fine, the next thing the user sees is the
    // updated app relaunching (NSIS passive mode relaunches it).
    app.state::<UpdateInFlight>()
        .0
        .store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(window) = app.get_webview_window("main") {
        if let Err(e) = window.destroy() {
            tracing::warn!("update: could not destroy the main window: {e}");
        }
    }
    let identifier = app.config().identifier.clone();
    let released =
        tauri::async_runtime::spawn_blocking(move || wait_for_webview_profile_release(&identifier))
            .await
            .unwrap_or(false);
    if !released {
        // Fail loud but proceed: blocking the update forever on a wedged
        // browser process would strand the user on the old version, and the
        // destroyed window already stopped all new writes.
        tracing::warn!("update: WebView2 did not release the profile in time, installing anyway");
    }

    // Safety snapshot of the profile's user data (Local Storage + IndexedDB)
    // before the installer touches anything - see snapshot_profile_storage.
    let version = app.package_info().version.to_string();
    match snapshot_profile_storage(&app.config().identifier, &version) {
        Ok(dest) => tracing::info!("update: storage snapshot at {}", dest.display()),
        Err(e) => tracing::error!("update: STORAGE SNAPSHOT FAILED ({e}); proceeding with the update without one"),
    }
    // Journal the handoff: the next journal line after this one should be the
    // relaunched app's run-start. Its absence after an update is itself data
    // (the 2026-08-17 21:03 update relaunch refused with a silent journal).
    boot_journal_append(
        &app.config().identifier,
        &format!("update-handoff from=v{version} spawning the installer"),
    );

    // Hands off to the installer and exits this process, so this normally
    // never returns. NSIS runs QUIET (installMode in tauri.conf.json): the
    // update window is the only thing on screen through the install.
    if let Err(e) = update.install(bytes) {
        // The webview is already gone: without a relaunch this process would be
        // a headless zombie. Tell the update window, then restart the (still
        // old) app; the update toast will re-offer the update on next launch
        // (whose boot also deletes the progress file).
        tracing::error!("update: install failed, relaunching the current version: {e}");
        update_window::write_progress(&update_window::UpdateProgress {
            phase: "error".into(),
            downloaded: 0,
            total: 0,
            message: Some(format!("Install failed: {e}")),
        });
        app.restart();
    }
    Ok(())
    }
}

/// Wait (up to 10s) for the WebView2 browser process to shut down and release
/// this app's Local Storage database. Chromium holds the leveldb `LOCK` file
/// open with no sharing, so an exclusive open attempt fails with a sharing
/// violation exactly as long as the storage service is still alive. Returns
/// true once the lock is free (or never existed).
#[cfg(windows)]
fn wait_for_webview_profile_release(identifier: &str) -> bool {
    use std::os::windows::fs::OpenOptionsExt;

    let local = match std::env::var_os("LOCALAPPDATA") {
        Some(dir) => dir,
        None => return true,
    };
    let profile = std::path::Path::new(&local).join(identifier).join("EBWebView");
    // TWO locks, and both matter (2026-08-17, learned in the field):
    // - `EBWebView\lockfile` is Chromium's user-data-dir lock, held by EVERY
    //   browser process for its whole life - healthy or broken. Waiting on it
    //   serializes against ANY predecessor. v0.1.36 waited only on the
    //   leveldb LOCK, which a dead-storage-plane predecessor never holds
    //   (it never opened the database), so a relaunch right after closing a
    //   dead session raced the dying browser and inherited the dead state.
    // - `Default\Local Storage\leveldb\LOCK` is held only while the storage
    //   database is open; waiting on it lets a HEALTHY predecessor finish
    //   flushing before we boot.
    let locks = [
        profile.join("lockfile"),
        profile
            .join("Default")
            .join("Local Storage")
            .join("leveldb")
            .join("LOCK"),
    ];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut contended = false;
    'outer: loop {
        for lock in &locks {
            if !lock.exists() {
                continue;
            }
            match std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0) // exclusive: fails while ANY other handle is open
                .open(lock)
            {
                Ok(_) => {}
                Err(e) => {
                    contended = true;
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!("webview profile still locked after 10s ({}): {e}", lock.display());
                        return false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    continue 'outer;
                }
            }
        }
        // Grace period for the rest of the browser teardown - but only when we
        // actually saw contention; a normal boot with free locks pays ~0ms.
        if contended {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        return true;
    }
}

#[cfg(not(windows))]
fn wait_for_webview_profile_release(_identifier: &str) -> bool {
    true
}

/// Product version (major.minor.patch) from the version RESOURCE of the file
/// on disk - unlike anything derived from a loaded module, this reads what a
/// fresh launch of `path` would actually run. None on any failure (no
/// resource, unreadable file): the caller must boot normally rather than
/// guess.
#[cfg(windows)]
fn read_disk_file_version(path: &std::path::Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, buf.as_mut_ptr() as *mut _).ok()?;
        let mut fixed: *mut VS_FIXEDFILEINFO = std::ptr::null_mut();
        let mut len = 0u32;
        let sub: Vec<u16> = "\\".encode_utf16().chain(std::iter::once(0)).collect();
        if !VerQueryValueW(
            buf.as_ptr() as *const _,
            PCWSTR(sub.as_ptr()),
            &mut fixed as *mut _ as *mut *mut core::ffi::c_void,
            &mut len,
        )
        .as_bool()
            || fixed.is_null()
        {
            return None;
        }
        let info = &*fixed;
        Some(format!(
            "{}.{}.{}",
            (info.dwProductVersionMS >> 16) & 0xffff,
            info.dwProductVersionMS & 0xffff,
            (info.dwProductVersionLS >> 16) & 0xffff
        ))
    }
}

/// Append one line to the boot journal (`%LOCALAPPDATA%\<identifier>\
/// boot-journal.log`). Pure observability for the dead-storage incident: every
/// boot records the lock states it found and every empty-read guard probe
/// records what the disk said, so a field incident arrives with data instead
/// of a screenshot. Epoch-ms timestamps; append-only; capped by truncating to
/// the newest half above ~256KB; never fails the caller.
fn boot_journal_append(identifier: &str, line: &str) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let entry = format!("{now_ms} {line}\n");

    // Primary target: %LOCALAPPDATA%\<identifier>\boot-journal.log.
    let primary_error: Option<String> = match std::env::var_os("LOCALAPPDATA") {
        None => Some("LOCALAPPDATA missing from the environment".into()),
        Some(local) => {
            let dir = std::path::Path::new(&local).join(identifier);
            let path = dir.join("boot-journal.log");
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.len() > 256 * 1024 {
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        let lines: Vec<&str> = text.lines().collect();
                        let keep = lines.len() / 2;
                        let _ = std::fs::write(&path, format!("{}\n", lines[keep..].join("\n")));
                    }
                }
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| std::io::Write::write_all(&mut f, entry.as_bytes()))
                .err()
                .map(|e| e.to_string())
        }
    };

    // A refusal boot that also fails this append used to be SILENT - four
    // observed field refusals left zero journal lines while the same
    // directory accepted writes minutes later (2026-08-18). The failure
    // itself is the datum we need, so on primary failure the entry rides to
    // fallback locations with the primary error embedded. Only on failure:
    // the fallbacks must not become a second journal to reconcile.
    if let Some(err) = primary_error {
        tracing::error!("boot journal primary write failed: {err}");
        let fallback_entry = format!(
            "{} {line} primary_error={:?}\n",
            now_ms,
            err
        );
        let mut targets = vec![std::env::temp_dir().join("rillio-boot-journal.log")];
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                targets.push(dir.join("boot-journal.log"));
            }
        }
        for target in targets {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&target)
                .and_then(|mut f| std::io::Write::write_all(&mut f, fallback_entry.as_bytes()));
        }
    }
}

/// True when the file exists and is exclusively held by another process.
fn lock_held(path: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        if !path.exists() {
            return false;
        }
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .is_err()
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// The WebView2 profile's Local Storage database directory - the ONLY copy of
/// the user's profile/library/settings.
fn local_storage_leveldb_dir(identifier: &str) -> Option<std::path::PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(
        std::path::Path::new(&local)
            .join(identifier)
            .join("EBWebView")
            .join("Default")
            .join("Local Storage")
            .join("leveldb"),
    )
}

/// Answer for the web's boot storage guard (apps/web common/storageGuard).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StorageHealth {
    /// Total bytes of the Local Storage leveldb on disk. ~0 on a fresh
    /// install; anything substantial means user data exists, so a session
    /// whose localStorage reads come back empty is UNREADABLE, not new
    /// (incident 2026-08-16: a WebView2 session booted with its DOM-storage
    /// plane dead and the app looked wiped while the data sat intact here).
    local_storage_bytes: u64,
}

/// The web guard reports its verdict here on every boot (fire-and-forget), so
/// the journal shows what the WEB side concluded next to what the shell saw -
/// including the `readable=false` branch (localStorage THROWING), which never
/// calls storage_health and was otherwise invisible.
#[tauri::command]
fn storage_guard_report(
    app: tauri::AppHandle,
    verdict: String,
    readable: bool,
    sentinel_present: bool,
    user_data_present: bool,
) {
    boot_journal_append(
        &app.config().identifier,
        &format!(
            "guard verdict={} readable={} sentinel={} userdata={}",
            verdict, readable, sentinel_present, user_data_present
        ),
    );
}

#[tauri::command]
fn storage_health(app: tauri::AppHandle) -> StorageHealth {
    let identifier = app.config().identifier.clone();
    let bytes = local_storage_leveldb_dir(&identifier)
        .map(|dir| dir_size(&dir))
        .unwrap_or(0);
    // This command only runs when the web guard saw EMPTY localStorage reads,
    // so its presence in the journal marks an empty-read boot; the bytes tell
    // whether the verdict became first-run (~0) or unreadable (data on disk).
    boot_journal_append(&identifier, &format!("guard-probe bytes={bytes}"));
    StorageHealth { local_storage_bytes: bytes }
}

/// Total size of the files directly inside `dir` (leveldb keeps everything
/// flat, so no recursion is needed; a missing dir is simply 0).
fn dir_size(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Full app restart, for the storage guard's refusal screen.
///
/// A page reload reuses the broken browser session. And a plain app.restart()
/// is NOT enough either (v0.1.30, observed live): the WebView2 BROWSER process
/// can outlive the app process, and the relaunched app re-attaches to that
/// same broken-storage browser - the refusal screen came back on every
/// restart, a lockout. So: destroy the window first, WAIT for the browser to
/// release the profile (the updater's own teardown), and only then relaunch,
/// guaranteeing the next session gets a fresh browser process.
#[tauri::command]
async fn restart_app(app: tauri::AppHandle) {
    #[cfg(desktop)]
    {
        // Destroying the last window fires ExitRequested(code None), and the
        // run-callback guard only prevents that exit while UpdateInFlight is
        // set - without this the process died here and the relaunch below
        // never ran ("restart closes the app but doesn't reopen", v0.1.31).
        app.state::<UpdateInFlight>()
            .0
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.destroy();
        }
        let identifier = app.config().identifier.clone();
        let _ = tauri::async_runtime::spawn_blocking(move || {
            wait_for_webview_profile_release(&identifier)
        })
        .await;
    }
    app.restart();
}

/// On-demand storage snapshot (the refusal screen's "continue anyway" path
/// snapshots before booting over storage it cannot read). Same store copier as
/// the pre-update snapshot, labeled distinctly.
#[tauri::command]
fn snapshot_storage(app: tauri::AppHandle) -> Result<String, String> {
    snapshot_profile_storage(&app.config().identifier, "bypass")
        .map(|path| path.display().to_string())
}

/// How many pre-update storage snapshots to keep (the newest N survive).
const STORAGE_SNAPSHOTS_KEPT: usize = 3;

/// Pre-install safety net: copy the WebView2 profile's user-data stores to
/// `%LOCALAPPDATA%\<identifier>\storage-backup\<version>-<timestamp>\` before
/// handing off to the installer, so no update can ever be the last copy's
/// death (incident 2026-08-16: a post-update session came up unable to read
/// Local Storage and LOOKED wiped; with a snapshot this whole class of loss is
/// recoverable by copying the snapshot back). Runs after the webview is down
/// (wait_for_webview_profile_release), so the leveldb files are quiescent.
///
/// Best-effort by policy: a failed snapshot logs loudly but does not block the
/// update (stranding users on an old version forever would be worse); the
/// stores it copies are small (KBs to low MBs), so the handoff cost is noise.
fn snapshot_profile_storage(identifier: &str, version: &str) -> Result<std::path::PathBuf, String> {
    let local = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?;
    let base = std::path::Path::new(&local).join(identifier);
    let profile = base.join("EBWebView").join("Default");
    let backups = base.join("storage-backup");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dest = backups.join(format!("{version}-{stamp}"));

    let mut copied_any = false;
    for store in ["Local Storage", "IndexedDB"] {
        let src = profile.join(store);
        if !src.is_dir() {
            continue;
        }
        copy_dir_recursive(&src, &dest.join(store))
            .map_err(|e| format!("copying {store:?} failed: {e}"))?;
        copied_any = true;
    }
    if !copied_any {
        return Err(format!("no storage directories found under {}", profile.display()));
    }
    prune_snapshots(&backups, STORAGE_SNAPSHOTS_KEPT);
    Ok(dest)
}

/// Plain recursive copy. WebView2 is shut down when this runs, so there are no
/// locked files; any IO error aborts the copy and surfaces to the caller.
fn copy_dir_recursive(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Keep the newest `keep` snapshot directories (by directory name, which
/// starts `<version>-<unix seconds>`; modified time breaks name ties in
/// practice never, and name order is what the stamp encodes). Best-effort.
fn prune_snapshots(backups: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(backups) else {
        return;
    };
    let mut dirs: Vec<std::path::PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    // Newest last by the embedded unix-seconds suffix; fall back to the whole
    // name so a malformed dir still sorts deterministically.
    let stamp_of = |p: &std::path::Path| -> u64 {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.rsplit('-').next())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    };
    dirs.sort_by_key(|p| (stamp_of(p), p.file_name().map(|n| n.to_owned())));
    let excess = dirs.len().saturating_sub(keep);
    for dir in dirs.into_iter().take(excess) {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            tracing::warn!("storage snapshot: could not prune {}: {e}", dir.display());
        }
    }
}

#[cfg(test)]
mod storage_snapshot_tests {
    use super::{copy_dir_recursive, prune_snapshots};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rillio-snap-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn copies_nested_files_byte_for_byte() {
        let src = temp_dir("src");
        let dest = temp_dir("dest").join("out");
        std::fs::create_dir_all(src.join("leveldb")).unwrap();
        std::fs::write(src.join("leveldb").join("000005.ldb"), b"table-bytes").unwrap();
        std::fs::write(src.join("LOG"), b"log-line").unwrap();

        copy_dir_recursive(&src, &dest).unwrap();

        assert_eq!(std::fs::read(dest.join("leveldb").join("000005.ldb")).unwrap(), b"table-bytes");
        assert_eq!(std::fs::read(dest.join("LOG")).unwrap(), b"log-line");
        std::fs::remove_dir_all(&src).unwrap();
        std::fs::remove_dir_all(dest.parent().unwrap()).unwrap();
    }

    #[test]
    fn prunes_to_the_newest_n_by_stamp() {
        let backups = temp_dir("prune");
        for (name, marker) in [
            ("0.1.26-1000", "a"),
            ("0.1.27-2000", "b"),
            ("0.1.27-3000", "c"),
            ("0.1.28-4000", "d"),
        ] {
            let dir = backups.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("marker"), marker).unwrap();
        }

        prune_snapshots(&backups, 3);

        let mut left: Vec<String> = std::fs::read_dir(&backups)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, vec!["0.1.27-2000", "0.1.27-3000", "0.1.28-4000"]);
        std::fs::remove_dir_all(&backups).unwrap();
    }

    #[test]
    fn pruning_a_missing_dir_is_a_noop() {
        prune_snapshots(std::path::Path::new("Z:/definitely/not/here"), 3);
    }
}

/// True when we can create `dir` and write a file inside it.
fn dir_writable(dir: &std::path::Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The torrent cache root. The cache lives IN THE APP'S FOLDER (`<app>\cache`):
/// the user already chose where the app lives via the installer's directory
/// picker, so the (potentially huge) cache inherits that choice instead of
/// silently filling the system drive's appdata. The one place that can't work
/// is a non-writable install dir (e.g. an elevated install under Program
/// Files), where we fall back to the app data dir and say so in the log.
fn default_cache_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(app_dir) = exe.parent() {
            let cache = app_dir.join("cache");
            if dir_writable(&cache) {
                return cache;
            }
            tracing::warn!(
                "cache dir {cache:?} is not writable, falling back to the app data dir"
            );
        }
    }
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("streaming-server")
}

/// One-time migration from the pre-0.1.17 cache location (appdata) to the
/// in-app-folder default. Same volume: a rename, instant regardless of size.
/// Cross volume: keep using the legacy dir instead (grandfathered) - silently
/// copying hundreds of GB at startup or abandoning the data are both worse.
/// Returns the cache root to actually use.
///
/// CRITICAL detail: librqbit's `session/session.json` stores each torrent's
/// `output_folder` as an ABSOLUTE path. After a move those still point at the
/// legacy root, and the engine happily keeps downloading THERE (recreating the
/// old dir on the old drive). Verified live 2026-07-12: a moved cache without
/// the rewrite re-downloaded ~30 GB onto the full system drive. So after a
/// successful move the session file gets its prefixes rewritten.
fn migrate_legacy_cache(legacy: &std::path::Path, new_root: &std::path::Path) -> bool {
    let has_content = |d: &std::path::Path| {
        d.read_dir().map(|mut i| i.next().is_some()).unwrap_or(false)
    };
    if !has_content(legacy) {
        return true; // nothing to migrate
    }
    if has_content(new_root) {
        // Both populated (e.g. a failed half-migration): prefer the new root,
        // and say loudly that the legacy data is orphaned.
        tracing::warn!(
            "both {legacy:?} and {new_root:?} contain cache data; using the new root. Delete the legacy dir to reclaim space."
        );
        return true;
    }
    // fs::rename moves a directory instantly on the same volume and fails
    // cross-volume (or while files are open) - exactly the split we want.
    let _ = std::fs::remove_dir(new_root); // rename target must not exist
    match std::fs::rename(legacy, new_root) {
        Ok(()) => {
            rewrite_session_output_folders(new_root, legacy, new_root);
            tracing::info!("migrated torrent cache {legacy:?} -> {new_root:?}");
            true
        }
        Err(e) => {
            tracing::info!(
                "cache stays at {legacy:?} (move to {new_root:?} not possible: {e})"
            );
            false
        }
    }
}

/// Rebase absolute `output_folder` paths inside librqbit's session.json from
/// `old_root` to `new_root`. Best-effort: a malformed or missing session file
/// is left alone (librqbit will rebuild it), but a rewrite failure after a
/// successful data move is loud, because the engine would then re-download to
/// the old location.
fn rewrite_session_output_folders(
    cache_root: &std::path::Path,
    old_root: &std::path::Path,
    new_root: &std::path::Path,
) {
    let session = cache_root.join("session").join("session.json");
    let raw = match std::fs::read_to_string(&session) {
        Ok(raw) => raw,
        Err(_) => return, // no session yet - nothing to rewrite
    };
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        tracing::warn!("session.json is not valid JSON, leaving it untouched");
        return;
    };
    let (old_s, new_s) = (old_root.to_string_lossy(), new_root.to_string_lossy());
    let mut changed = false;
    if let Some(torrents) = json.get_mut("torrents").and_then(|t| t.as_object_mut()) {
        for torrent in torrents.values_mut() {
            if let Some(folder) = torrent.get_mut("output_folder") {
                if let Some(path) = folder.as_str() {
                    if path.starts_with(old_s.as_ref()) {
                        *folder = serde_json::Value::String(path.replacen(old_s.as_ref(), new_s.as_ref(), 1));
                        changed = true;
                    }
                }
            }
        }
    }
    if changed {
        match serde_json::to_string(&json) {
            Ok(out) => {
                if let Err(e) = std::fs::write(&session, out) {
                    tracing::error!("session.json rewrite failed ({e}): torrents will keep downloading to {old_root:?}");
                }
            }
            Err(e) => tracing::error!("session.json re-serialize failed: {e}"),
        }
    }
}

/// The embedded streaming server, once it has actually bound.
///
/// Holding the [`Engine`] and the [`Router`] (not just "a server is running
/// somewhere on port N") is what makes the socket optional: mpv reads bytes
/// straight off the engine through `rillio://` (see [`crate::stream_cb`]) and
/// the web client dispatches control-plane requests into the router in-process
/// through [`server_request`]. The HTTP listener stays up for the things that
/// genuinely need a socket - casting, external players, the subtitle routes'
/// loopback self-fetch - but nothing on the playback or cache path depends on
/// it any more.
pub struct ServerState {
    pub engine: rillio_streaming_server::Engine,
    /// The SAME router the socket serves, cloned. Dispatching into it is
    /// indistinguishable from an HTTP request except that no bytes cross a
    /// socket, so every route, extractor and middleware behaves identically.
    pub router: rillio_streaming_server::axum::Router,
    /// What the server advertises to clients, with the port it really got.
    pub base_url: String,
    pub port: u16,
    /// The runtime the engine lives on. The mpv stream callbacks run on mpv's
    /// own threads and `block_on` through this handle; see `stream_cb`.
    pub runtime: tokio::runtime::Handle,
}

/// `None` until the bind completes. Every reader clones the `Arc` out and drops
/// the lock immediately, so nothing ever holds it across an await.
#[derive(Default)]
pub struct ServerHandle(Mutex<Option<std::sync::Arc<ServerState>>>);

impl ServerHandle {
    pub fn get(&self) -> Option<std::sync::Arc<ServerState>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, state: ServerState) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::sync::Arc::new(state));
    }
}

/// Where the streaming server actually is, or `None` while it is still binding.
///
/// The web client polls this (apps/web/src/common/serverAddress) instead of
/// assuming the default port: the profile setting stays symbolic, so a boot that
/// had to fall back to an ephemeral port is invisible to everything persisted.
#[tauri::command]
fn streaming_server_url(state: tauri::State<'_, ServerHandle>) -> Option<String> {
    state.get().map(|s| s.base_url.clone())
}

/// One in-process streaming-server response, shaped for `new Response(...)` on
/// the web side.
#[derive(Debug, serde::Serialize)]
pub struct ServerResponse {
    pub status: u16,
    pub headers: std::collections::HashMap<String, String>,
    pub body: String,
}

/// Longest response body the IPC transport will carry. The control plane
/// answers JSON (the biggest by far is `/cache/list` with its per-torrent
/// metadata); media bytes never come through here, so anything past this is a
/// bug and fails loud rather than silently truncating.
const IPC_BODY_LIMIT: usize = 32 * 1024 * 1024;

/// The web client's control-plane transport: run one request against the
/// in-process router instead of the socket.
///
/// SECURITY. This is a NEW trust boundary - the request never passes through
/// the network stack, so the browser guarantees the HTTP path relies on (an
/// unforgeable `Origin`, the CORS preflight) do not apply here. What replaces
/// them:
///
/// - The caller is the app's own webview: this command is only reachable from
///   the page Tauri itself serves, which is exactly the origin the HTTP guard
///   allowlists. We stamp that origin on the synthesized request so
///   `security::origin_guard` sees the same thing either way rather than taking
///   the no-Origin exemption meant for native media loads.
/// - Only GET and POST are accepted. Every mutating route is registered POST
///   only, and dispatching into the real router means those method filters
///   still decide (a GET at `/cache/delete` is a 405 here as well). Forwarding
///   an arbitrary method string would hand web content a way to probe methods
///   no HTTP client of ours ever sends.
/// - The raw media route is refused outright. It is the one route that streams
///   unbounded bytes and implements the Range/HEAD contract, none of which
///   survives a string round trip; playback uses `rillio://` or the socket.
#[tauri::command]
async fn server_request(
    state: tauri::State<'_, ServerHandle>,
    method: String,
    path: String,
    body: Option<String>,
) -> Result<ServerResponse, String> {
    // Clone the router out and drop the lock BEFORE awaiting: a std Mutex guard
    // held across an await would make this future non-Send and can deadlock.
    let server = state
        .get()
        .ok_or_else(|| "server_request: the streaming server has not bound yet".to_string())?;
    let router = server.router.clone();
    dispatch_server_request(router, method, path, body).await
}

/// The body of [`server_request`], separated from Tauri's state extraction so
/// the boundary rules can be exercised against a real router.
async fn dispatch_server_request(
    router: rillio_streaming_server::axum::Router,
    method: String,
    path: String,
    body: Option<String>,
) -> Result<ServerResponse, String> {
    use rillio_streaming_server::axum::body::Body;
    use rillio_streaming_server::axum::http::{header, Request};
    use rillio_streaming_server::tower::ServiceExt;

    let method = method.to_ascii_uppercase();
    if method != "GET" && method != "POST" {
        tracing::error!("server_request: BLOCKED method {method:?} for {path:?}");
        return Err(format!("server_request: method {method:?} is not allowed"));
    }
    if !path.starts_with('/') {
        return Err(format!("server_request: path {path:?} must start with '/'"));
    }
    if let Some(seg) = traversal_segment(&path) {
        tracing::error!("server_request: BLOCKED path traversal in {path:?}");
        return Err(format!(
            "server_request: {path:?} contains a path traversal segment ({seg:?})"
        ));
    }
    if is_raw_stream_path(&path) {
        tracing::error!("server_request: BLOCKED the raw stream route {path:?}");
        return Err(format!("server_request: {path:?} is the media stream route, not a control-plane call"));
    }

    let has_body = body.is_some();
    let mut req = Request::builder()
        .method(method.as_str())
        .uri(&path)
        // Same origin the webview sends over HTTP, so origin_guard applies the
        // identical rule to both transports.
        .header(header::ORIGIN, "http://tauri.localhost");
    if has_body {
        // Every payload the web sends this way is JSON (the routes that take a
        // body all use the Json extractor, which requires the header). Bodies
        // that are not JSON would have to go over the socket.
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let req = req
        .body(Body::from(body.unwrap_or_default()))
        .map_err(|e| format!("server_request: building the request failed: {e}"))?;

    let response = router
        .oneshot(req)
        .await
        .map_err(|e| format!("server_request: the router failed: {e}"))?;

    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value.to_str().ok().map(|v| (name.as_str().to_owned(), v.to_owned()))
        })
        .collect();
    let bytes = rillio_streaming_server::axum::body::to_bytes(response.into_body(), IPC_BODY_LIMIT)
        .await
        .map_err(|e| format!("server_request: reading the {path} body failed: {e}"))?;
    let body = String::from_utf8(bytes.to_vec())
        .map_err(|_| format!("server_request: {path} answered bytes that are not utf-8"))?;

    Ok(ServerResponse { status, headers, body })
}

/// Decode `%XX` escapes in one raw path segment, to BYTES (the router's `Path`
/// extractor decodes to bytes and only then utf-8-checks). Invalid escapes
/// pass through literally, matching how axum's decoder treats a lone `%`.
fn percent_decode_segment(seg: &str) -> Vec<u8> {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// The first path segment whose DECODED form smuggles a `..` component, if
/// any. axum happily routes `/../settings` into the router (where it 500s),
/// and an encoded `%2e%2e` or `..%2F` survives raw segment splitting only to
/// decode into a traversal downstream, so all of it is refused before the
/// router ever sees the request. Components are re-split on decoded `/` and
/// `\` precisely so an encoded separator cannot hide the dot-dot; a legit
/// encoded URL segment (`/tracks/{url}`) decodes to components like `http:`
/// and passes untouched.
fn traversal_segment(path: &str) -> Option<String> {
    let path = path.split(['?', '#']).next().unwrap_or_default();
    for seg in path.split('/') {
        let decoded = percent_decode_segment(seg);
        if decoded
            .split(|&b| b == b'/' || b == b'\\')
            .any(|component| component == b"..")
        {
            return Some(String::from_utf8_lossy(&decoded).into_owned());
        }
    }
    None
}

/// Does this path route to the media stream? The stream routes are
/// `/{info_hash}/{idx}` and `/{info_hash}/{idx}/{*rest}`, but `{idx}` is NOT
/// only a number: `stream.rs::resolve_index` also resolves it by FILE NAME,
/// a `?f=` query selects the file regardless of the segment, and the `Path`
/// extractor percent-decodes what the raw route match let through. So the
/// rule is by exclusion: ANY path whose first segment decodes to a 40-hex
/// info hash is the byte plane, EXCEPT the exact non-stream routes registered
/// under an info hash (verify against the router table in
/// crates/streaming-server/src/lib.rs):
///
///   /{info_hash}/create        (POST, control)
///   /{info_hash}/remove        (POST, control)
///   /{info_hash}/stats.json    (GET, metadata)
///   /{info_hash}/{idx}/stats.json  (GET, metadata)
///
/// Those are matched on the RAW segments, exactly as the router matches its
/// literal segments: an encoded `cre%61te` does not match the literal
/// `/create` route, it falls through to the stream route, so it must count as
/// the byte plane here too.
fn is_raw_stream_path(path: &str) -> bool {
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return false;
    }
    let first = percent_decode_segment(segments[0]);
    if first.len() != 40 || !first.iter().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    !matches!(
        (segments.len(), segments.get(1).copied(), segments.get(2).copied()),
        (2, Some("create" | "remove" | "stats.json"), _) | (3, _, Some("stats.json"))
    )
}

/// Spawn the embedded streaming server on Tauri's async (tokio) runtime. It
/// binds 127.0.0.1:11470 (an ephemeral port if that is refused) and owns the
/// torrent cache (see `default_cache_dir`).
fn start_streaming_server(app: &tauri::AppHandle) {
    // RILLIO_STREAMING_CACHE_DIR overrides the cache/session root. Use it to run
    // a dev build against an ISOLATED cache so it never opens (or evicts from) the
    // installed app's real torrent cache. Unset => `<app>\cache`.
    let mut cache_dir = match std::env::var_os("RILLIO_STREAMING_CACHE_DIR") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => {
            let new_root = default_cache_dir(app);
            match app.path().app_data_dir().map(|d| d.join("streaming-server")) {
                Ok(legacy) if legacy != new_root => {
                    if migrate_legacy_cache(&legacy, &new_root) {
                        new_root
                    } else {
                        legacy // cross-volume: grandfathered in place
                    }
                }
                _ => new_root,
            }
        }
    };
    if let Err(e) = std::fs::create_dir_all(&cache_dir) {
        tracing::error!("cannot create cache dir {cache_dir:?}: {e}");
        // Last-ditch so the server can still come up.
        cache_dir = std::env::temp_dir().join("rillio-streaming-server");
        let _ = std::fs::create_dir_all(&cache_dir);
    }
    let config = rillio_streaming_server::Config::local(cache_dir);
    // `bind_and_serve` patches the config's port from the listener, but keeps
    // the HOST of base_url (deliberately independent of `bind`), so rebuild the
    // advertised URL the same way here rather than assuming loopback.
    let mut advertised = config.base_url.clone();
    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        use tauri::Emitter;
        // Contract: the web app listens for the Tauri event
        // "streaming-server-error" (payload: the error string) so it can toast
        // that the local streaming server failed. Without this the failure is
        // only logged and invisible to the user.
        let (addr, engine, router, serving) = match rillio_streaming_server::bind_and_serve(config).await {
            Ok(parts) => parts,
            Err(e) => {
                let msg = e.to_string();
                tracing::error!("embedded streaming server could not start: {msg}");
                let _ = app_handle.emit("streaming-server-error", msg);
                return;
            }
        };
        if advertised.set_port(Some(addr.port())).is_err() {
            tracing::error!("streaming server base url {advertised} cannot carry a port");
            let _ = app_handle.emit("streaming-server-error", "base url cannot carry a port");
            return;
        }
        let base_url = advertised.to_string();
        tracing::info!("streaming server bound at {addr} (advertising {base_url})");
        app_handle.state::<ServerHandle>().set(ServerState {
            engine,
            router,
            base_url,
            port: addr.port(),
            // The runtime this task is running on is the one the engine's
            // background work lives on; the mpv byte plane blocks on it.
            runtime: tokio::runtime::Handle::current(),
        });

        // SUPERVISION. The socket dying is no longer fatal: the router still
        // answers over IPC and mpv still reads bytes off the engine, so
        // ServerState stays in place and playback plus the cache UI keep
        // working. Say so loudly (casting and external players DO need the
        // socket) instead of pretending nothing happened.
        if let Err(e) = serving.await {
            let msg = e.to_string();
            tracing::error!(
                "embedded streaming server's HTTP listener exited: {msg} \
                 (in-process requests and playback continue)"
            );
            let _ = app_handle.emit("streaming-server-error", msg);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const IH: &str = "0123456789abcdef0123456789abcdef01234567";

    /// The IPC control plane must refuse the media stream route: it is the one
    /// route that streams unbounded bytes and carries the Range/HEAD contract,
    /// none of which survives being marshalled through a string.
    #[test]
    fn ipc_refuses_the_raw_stream_route() {
        for path in [
            format!("/{IH}/0"),
            format!("/{IH}/12"),
            format!("/{IH}/-1"),
            format!("/{IH}/0?tr=http%3A%2F%2Ftracker"),
            // the {*rest} form (an addon url that appends the filename)
            format!("/{IH}/0/Some.Movie.2026.mkv"),
            format!("/{IH}/0/deeper/still.mkv"),
            // {idx} ALSO resolves by file name (stream.rs resolve_index)
            format!("/{IH}/Some.Movie.2026.mkv"),
            // ...and a ?f= selector picks the file regardless of the segment
            format!("/{IH}/anything?f=mkv"),
            format!("/{IH}/Some.Movie.2026.mkv?f=mkv"),
            // a percent-encoded first byte: the raw path dodges a naive hash
            // check, but the Path extractor decodes it back into a valid hash
            format!("/%30{}/0", &IH[1..]),
            format!("/%30{}/anything", &IH[1..]),
            // uppercase hex reaches the stream route just the same
            format!("/{}/0", IH.to_ascii_uppercase()),
            // a fourth segment is never a control route
            format!("/{IH}/0/stats.json/x"),
            // an encoded "create" does NOT match the literal /create route;
            // it falls through to the stream route's {idx}
            format!("/{IH}/cre%61te"),
        ] {
            assert!(is_raw_stream_path(&path), "should refuse {path:?}");
        }
    }

    /// Traversal segments, encoded or not, are refused before the router.
    #[test]
    fn ipc_rejects_path_traversal() {
        for path in [
            "/../settings",
            "/%2e%2e/settings",
            "/cache/../settings",
            "/cache/..%2Fsettings",
            "/cache/%2e%2e%2fsettings",
            "/cache/..%5Csettings",
        ] {
            assert!(traversal_segment(path).is_some(), "should refuse {path:?}");
        }
        for path in [
            "/settings",
            "/cache/list",
            // an encoded URL segment decodes to slashes but no dot-dot
            "/tracks/http%3A%2F%2Fhost%2Fsub.vtt",
            // dots that are not a traversal component
            "/cache/files/some..name",
            "/{ih}/0/stats.json",
        ] {
            assert!(traversal_segment(path).is_none(), "should allow {path:?}");
        }
    }

    /// ...and nothing else. Everything the web actually fetches over IPC has to
    /// get through, including the two routes that live UNDER an info hash.
    #[test]
    fn ipc_allows_the_control_plane() {
        for path in [
            "/settings".to_owned(),
            "/torrent-settings".to_owned(),
            "/cache/list".to_owned(),
            "/cache/files/".to_owned() + IH,
            "/stats.json".to_owned(),
            // static segments that win over the {idx} stream param
            format!("/{IH}/stats.json"),
            format!("/{IH}/0/stats.json"),
            format!("/{IH}/create"),
            format!("/{IH}/remove"),
            "/opensubHash?videoUrl=x".to_owned(),
            "/hlsv2/probe?mediaURL=x".to_owned(),
            "/".to_owned(),
        ] {
            assert!(!is_raw_stream_path(&path), "should allow {path:?}");
        }
    }

    /// A real streaming-server router over a throwaway cache dir, plus the
    /// runtime to drive it. Nothing binds a socket: that is the point.
    fn test_router() -> (tokio::runtime::Runtime, rillio_streaming_server::axum::Router) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime");
        let dir = std::env::temp_dir().join(format!("rillio-ipc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create cache dir");
        let config = rillio_streaming_server::Config::local(dir);
        let engine = rt
            .block_on(rillio_streaming_server::Engine::new(config.cache_root.clone()))
            .expect("engine");
        let router = rillio_streaming_server::router(config, engine);
        (rt, router)
    }

    /// The IPC transport is a real request against the real router: same
    /// handlers, same middleware, same status codes as the socket path.
    #[test]
    fn ipc_dispatch_answers_the_control_plane() {
        let (rt, router) = test_router();
        let resp = rt
            .block_on(dispatch_server_request(
                router.clone(),
                "GET".into(),
                "/settings".into(),
                None,
            ))
            .expect("GET /settings");
        eprintln!("[ipc] GET /settings -> {} {}", resp.status, resp.body);
        assert_eq!(resp.status, 200);
        let json: serde_json::Value = serde_json::from_str(&resp.body).expect("settings json");
        // The baseUrl gate the web client keys playback off must still be there.
        assert!(
            json["baseUrl"].as_str().is_some_and(|u| !u.is_empty()),
            "settings must still advertise a baseUrl: {}",
            resp.body
        );
        assert_eq!(
            resp.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );

        // A POST with a JSON body reaches the extractor (which needs the
        // content-type this transport supplies).
        let resp = rt
            .block_on(dispatch_server_request(
                router,
                "POST".into(),
                "/cache/pin".into(),
                Some(r#"{"infoHash":"0123456789abcdef0123456789abcdef01234567","pinned":true}"#.into()),
            ))
            .expect("POST /cache/pin");
        eprintln!("[ipc] POST /cache/pin -> {} {}", resp.status, resp.body);
        assert_ne!(
            resp.status, 415,
            "the Json extractor rejected the body's content type"
        );
        assert_ne!(resp.status, 405, "a POST route must accept POST");
    }

    /// The POST-only discipline is enforced by the router itself, so it holds
    /// over IPC exactly as it does over HTTP.
    #[test]
    fn ipc_dispatch_keeps_mutations_post_only() {
        let (rt, router) = test_router();
        for path in ["/cache/delete", "/cache/pin", "/removeAll"] {
            let resp = rt
                .block_on(dispatch_server_request(
                    router.clone(),
                    "GET".into(),
                    path.into(),
                    None,
                ))
                .unwrap_or_else(|e| panic!("GET {path}: {e}"));
            eprintln!("[ipc] GET {path} -> {}", resp.status);
            assert_eq!(resp.status, 405, "GET {path} must not mutate");
        }
    }

    /// Anything outside GET/POST, a relative path, and the media stream route
    /// are refused BEFORE the router sees them.
    #[test]
    fn ipc_dispatch_refuses_what_the_boundary_forbids() {
        let (rt, router) = test_router();
        for method in ["DELETE", "PUT", "HEAD", "OPTIONS", "PATCH", "TRACE"] {
            let err = rt
                .block_on(dispatch_server_request(
                    router.clone(),
                    method.into(),
                    "/settings".into(),
                    None,
                ))
                .expect_err("must refuse {method}");
            assert!(err.contains("is not allowed"), "{method}: {err}");
        }
        assert!(rt
            .block_on(dispatch_server_request(
                router.clone(),
                "GET".into(),
                "settings".into(),
                None,
            ))
            .is_err());
        for stream_path in [
            format!("/{IH}/0"),
            // the review's bypass spellings: file-name idx, ?f= selector,
            // percent-encoded hash byte
            format!("/{IH}/Some.Movie.2026.mkv"),
            format!("/{IH}/anything?f=mkv"),
            format!("/%30{}/0", &IH[1..]),
        ] {
            let err = rt
                .block_on(dispatch_server_request(
                    router.clone(),
                    "GET".into(),
                    stream_path.clone(),
                    None,
                ))
                .expect_err("the stream route must be refused");
            eprintln!("[ipc] GET {stream_path} -> {err}");
            assert!(err.contains("media stream route"), "{err}");
        }
        let err = rt
            .block_on(dispatch_server_request(
                router,
                "GET".into(),
                "/../settings".into(),
                None,
            ))
            .expect_err("a traversal path must be refused");
        eprintln!("[ipc] GET /../settings -> {err}");
        assert!(err.contains("traversal"), "{err}");
    }

    /// Every scheme the web client's openExternal / custom-scheme navigations
    /// legitimately produce must be accepted (parsed end-to-end).
    #[test]
    fn allows_legit_external_urls() {
        for url in [
            "http://127.0.0.1:11470/abc/0",
            "https://www.strem.io/trakt/auth/x",
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
            "mailto:support@rillio.app",
            "webcal://www.strem.io/calendar/x.ics",
            "vlc-x-callback://x-callback-url/stream?url=http%3A%2F%2Fx",
            "intent://x#Intent;scheme=https;end",
        ] {
            validate_external_url(url)
                .unwrap_or_else(|e| panic!("{url} should be allowed: {e}"));
        }
    }

    /// The S2 hole: file:, UNC / bare local paths, and unknown (possibly
    /// registered) protocols must all be refused before reaching `open::that`.
    #[test]
    fn rejects_file_unc_and_unknown_urls() {
        for bad in [
            "",                                     // empty / unparseable
            "file:///C:/Windows/System32/calc.exe", // file: scheme
            "C:/Windows/System32/calc.exe",         // bare drive path (scheme "c")
            "C:\\Windows\\System32\\calc.exe",
            "\\\\server\\share\\evil.exe",          // UNC, no scheme -> parse fails
            "//server/share",                       // schemeless / relative
            "made-up-scheme://whatever",            // unknown protocol handler
            "ms-msdt:/id",                          // classic Windows URL-handler RCE
            "javascript:alert(1)",
        ] {
            assert!(validate_external_url(bad).is_err(), "should reject {bad:?}");
        }
    }

    /// The deep-link scheme boundary accepts only stremio/rillio (any case) and
    /// denies everything else, so an OS-supplied URL of another scheme is never
    /// forwarded to the web client.
    #[test]
    fn deep_link_scheme_is_allowlisted() {
        for ok in ["stremio", "rillio", "STREMIO", "Rillio"] {
            assert!(is_deep_link_scheme(ok), "should accept {ok}");
        }
        for bad in ["", "http", "https", "magnet", "file", "javascript", "ms-msdt"] {
            assert!(!is_deep_link_scheme(bad), "should reject {bad}");
        }
    }

    /// The scheme check is case-insensitive and denies by default.
    #[test]
    fn scheme_check_is_case_insensitive_and_denies_by_default() {
        assert!(is_allowed_external_scheme("HTTPS"));
        assert!(is_allowed_external_scheme("Magnet"));
        assert!(is_allowed_external_scheme("mpv"));
        assert!(!is_allowed_external_scheme(""));
        assert!(!is_allowed_external_scheme("file"));
        assert!(!is_allowed_external_scheme("smb"));
    }
}
