//! The custom update window (docs/update-window/decomposition.md): a DETACHED
//! `rillio-desktop --update-window` process showing update.html (liquid
//! composition + progress) while an update downloads and installs, replacing
//! the stock NSIS progress dialog.
//!
//! Why a separate process: tauri-plugin-updater exits this process the moment
//! it hands off to the installer, so an in-process window could only cover the
//! download. And why a separate WebView2 data directory: the updater's
//! data-wipe fix (`wait_for_webview_profile_release` in lib.rs, the 0.1.17
//! incident) waits for the MAIN profile's Local Storage lock to be released -
//! a window on the same profile would hold that lock and time the wait out.
//!
//! IPC is a tiny JSON file in the temp dir (`progress_path`): `install_update`
//! writes phases/bytes, this process polls it and drives the page. The
//! relaunched (new) app deletes the file on boot - that deletion, observed
//! while in the `installing` phase, is the "update done" signal.

use std::io::Write;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct UpdateProgress {
    pub phase: String, // downloading | installing | error
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default)]
    pub total: u64,
    /// Set in the `error` phase: the plain sentence, its kind and the full
    /// chain, the same object the web layer gets from the update commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<crate::error_chain::UpdateFailure>,
}

/// The progress file both processes agree on. Temp dir, not the app config dir:
/// it must be writable before install and irrelevant after (the new app boot
/// deletes it; a leftover from a crashed run is harmless and overwritten).
pub fn progress_path() -> std::path::PathBuf {
    std::env::temp_dir().join("rillio-update-progress.json")
}

/// Atomically-enough write (temp + rename would be overkill for a single small
/// line read by a tolerant poller; a torn read just shows the previous frame).
pub fn write_progress(progress: &UpdateProgress) {
    if let Ok(json) = serde_json::to_string(progress) {
        let _ = std::fs::File::create(progress_path()).and_then(|mut f| f.write_all(json.as_bytes()));
    }
}

/// Where the splash runs from: a COPY of this exe under a DIFFERENT NAME.
///
/// ★ This is not a detail, it is the whole reason updates work. The NSIS
/// installer runs SILENTLY (`installMode: quiet` -> `/S /R`) and starts by
/// looking for a running process literally named `rillio-desktop.exe`
/// (`CheckIfAppIsRunning` in tauri-bundler's utils.nsh). Running the splash
/// from the installed binary meant the installer found "the app" still running
/// AND could not overwrite the locked exe - and its silent-mode failure path
/// writes a red line to a console a windowed installer does not have and then
/// `Abort`s, installing NOTHING and reporting NOTHING. The user was relaunched
/// onto the OLD version by the backstop below, so a failed update looked like a
/// successful one (v0.1.24 -> v0.1.25, 2026-07-21).
///
/// A copy under another name is invisible to that check and holds no lock on
/// the install directory, so the installer can replace every file while the
/// splash keeps showing progress - which is the point of having a splash.
fn updater_copy_path() -> std::path::PathBuf {
    std::env::temp_dir().join("rillio-updater.exe")
}

/// Delete a leftover updater copy (called on normal boot). Best-effort: it may
/// still be running for a moment after the relaunch, and temp is self-cleaning.
pub fn cleanup_updater_copy() {
    let _ = std::fs::remove_file(updater_copy_path());
}

/// Spawn the detached update-window process. Called by `install_update` before
/// it hides the main window.
///
/// The splash is presentation only: every failure here degrades to "no splash",
/// never to "no update".
pub fn spawn_update_window() {
    let Ok(exe) = std::env::current_exe() else {
        tracing::warn!("update-window: current_exe unavailable, skipping the splash");
        return;
    };
    // Run from a renamed copy (see updater_copy_path). If the copy fails we do
    // NOT fall back to launching the installed exe: that is the exact collision
    // that silently breaks the install. No splash is strictly better.
    let splash = updater_copy_path();
    if let Err(e) = std::fs::copy(&exe, &splash) {
        tracing::warn!("update-window: could not stage the splash copy ({e}); continuing without it");
        return;
    }
    // The app's real path rides along: the copy cannot use current_exe() to
    // relaunch Rillio (that would relaunch the updater, forever).
    match std::process::Command::new(&splash)
        .arg("--update-window")
        .arg(&exe)
        .spawn()
    {
        Ok(_) => tracing::info!("update-window: splash process spawned from {}", splash.display()),
        Err(e) => tracing::warn!("update-window: could not spawn the splash: {e}"),
    }
}

/// The `--update-window` process entry: a minimal Tauri app (no plugins, no
/// server, no state) with one small frameless window on its OWN WebView2
/// profile, polling the progress file until the update finishes.
pub fn run(ctx: tauri::Context<tauri::Wry>) {
    // The installed app's path, handed over by spawn_update_window. This
    // process is a renamed COPY, so current_exe() would relaunch the updater
    // rather than Rillio.
    let app_exe: Option<std::path::PathBuf> = std::env::args_os()
        .skip_while(|arg| arg != "--update-window")
        .nth(1)
        .map(std::path::PathBuf::from);
    let result = tauri::Builder::default()
        .setup(move |app| {
            let mut builder = tauri::WebviewWindowBuilder::new(
                app,
                "update",
                tauri::WebviewUrl::App("update.html".into()),
            )
            .title("Updating Rillio")
            .inner_size(360.0, 420.0)
            .resizable(false)
            .maximizable(false)
            .decorations(false)
            // NOT transparent: the page is an opaque full-bleed surface and
            // Windows 11 rounds + shadows the frameless window natively - a
            // transparent window + page-drawn card doubled the chrome (two
            // nested containers with mismatched radii).
            .always_on_top(true)
            .center();
            // Own WebView2 profile - see the module docs. Windows-only knob.
            #[cfg(windows)]
            {
                if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                    builder = builder.data_directory(
                        std::path::Path::new(&local)
                            .join(&app.config().identifier)
                            .join("EBWebViewUpdater"),
                    );
                }
            }
            let window = builder.build()?;
            let identifier = app.config().identifier.clone();

            // Poll the progress file and drive the page. WebviewWindow is Send:
            // eval marshals onto the right thread internally.
            std::thread::spawn(move || {
                let started = Instant::now();
                let mut last_payload = String::new();
                let mut installing_since: Option<Instant> = None;
                loop {
                    std::thread::sleep(Duration::from_millis(150));
                    // Hard cap: never outlive a wedged update by more than 10min.
                    if started.elapsed() > Duration::from_secs(600) {
                        std::process::exit(0);
                    }
                    match std::fs::read_to_string(progress_path()) {
                        Ok(raw) => {
                            let Ok(progress) = serde_json::from_str::<UpdateProgress>(&raw) else {
                                continue; // torn write; next poll gets a full frame
                            };
                            if progress.phase == "installing" && installing_since.is_none() {
                                installing_since = Some(Instant::now());
                            }
                            if progress.phase == "error" {
                                // Show the failure, then get out of the way (the
                                // main app re-shows its own window on error, or
                                // relaunches after a failed install). The page
                                // leads with a plain sentence and keeps the chain
                                // behind "Details" (the writer logs it too, see
                                // update_failure in lib.rs); 8s, the same as the
                                // failed-install path below. The page's footer
                                // says so ("in a few seconds").
                                if raw != last_payload {
                                    let _ = window.eval(&format!("window.__updateState({raw})"));
                                }
                                std::thread::sleep(Duration::from_secs(8));
                                std::process::exit(0);
                            }
                            if raw != last_payload {
                                last_payload = raw.clone();
                                let _ = window.eval(&format!("window.__updateState({raw})"));
                            }
                            // ★ The install did NOT complete.
                            //
                            // Success deletes this file (the relaunched new app
                            // clears it on boot), so still being here after a
                            // generous install window means the installer failed
                            // or aborted - and NSIS aborts SILENTLY in quiet mode
                            // (see updater_copy_path). This used to just relaunch
                            // the app and exit, which presented a failed update as
                            // a successful one: the user got the old version back
                            // with no error at all. Say it plainly instead, then
                            // put them back in the working app they had.
                            if let Some(t0) = installing_since {
                                if t0.elapsed() > Duration::from_secs(120) {
                                    tracing::error!("update-window: install did not complete; reporting failure");
                                    // tracing's stdout goes nowhere in a release
                                    // build; the boot journal is the log file.
                                    crate::boot_journal_append(
                                        &identifier,
                                        "update-failed stage=installer error=\"the installer did not finish within 120s (NSIS quiet mode aborts silently)\"",
                                    );
                                    let payload = UpdateProgress {
                                        phase: "error".into(),
                                        downloaded: 0,
                                        total: 0,
                                        failure: Some(crate::error_chain::UpdateFailure::plain(
                                            crate::error_chain::Stage::Install,
                                            crate::error_chain::summary(
                                                crate::error_chain::FailureKind::Other,
                                                crate::error_chain::Stage::Install,
                                            ),
                                            "The installer did not finish within 120 seconds. The latest installer is on rillio.app.",
                                        )),
                                    };
                                    if let Ok(json) = serde_json::to_string(&payload) {
                                        let _ = window.eval(&format!("window.__updateState({json})"));
                                    }
                                    std::thread::sleep(Duration::from_secs(8));
                                    if let Some(app_exe) = app_exe.as_ref() {
                                        let _ = std::process::Command::new(app_exe).spawn();
                                    }
                                    std::process::exit(0);
                                }
                            }
                        }
                        Err(_) => {
                            // File gone. During install that is THE success
                            // signal: the relaunched new app deletes it on boot.
                            // Before install it means a crashed/aborted updater -
                            // either way this window is done. Give the page a
                            // moment to show "Starting Rillio", then exit.
                            if installing_since.is_some() {
                                let _ = window.eval("window.__updateState({phase:'restarting'})");
                                std::thread::sleep(Duration::from_secs(2));
                            }
                            std::process::exit(0);
                        }
                    }
                }
            });
            Ok(())
        })
        .run(ctx);
    if let Err(e) = result {
        tracing::error!("update-window: failed to run: {}", crate::error_chain::error_chain(&e));
    }
}
