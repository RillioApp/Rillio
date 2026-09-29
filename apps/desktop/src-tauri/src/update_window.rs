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
//!
//! RETRY (the back-channel, 2026-09-29). "Try again" in a failed window must
//! make the MAIN process run the install flow again, and the main process is
//! not always the one that started the update: after a failed install it was
//! relaunched, and the user may have quit it. So the window does not talk to
//! a process, it LAUNCHES the app: `rillio-desktop --retry-update <attempt>`.
//! If Rillio is running, the single-instance plugin hands those args to it
//! (the same proven path a second launch already takes); if not, the new
//! process is Rillio and starts the flow itself once its window is up. Either
//! way the main process runs the flow WITHOUT spawning another window
//! ([`UpdateRun::request`]), and every frame it writes carries the attempt id.
//! The window only follows frames of the attempt it is waiting for, so a stale
//! "failed" from an earlier attempt can never land on a working retry, and a
//! retry nobody answers within [`RETRY_ACK_TIMEOUT`] becomes a failure on
//! screen instead of a spinner. The pure state machine is [`WindowMachine`].

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error_chain::{FailureKind, Stage, UpdateFailure};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpdateProgress {
    pub phase: String, // checking | downloading | installing | error
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default)]
    pub total: u64,
    /// Set in the `error` phase: what happened, what to check, the kind and
    /// the report "Copy error" copies; the same object the web layer gets
    /// from the update commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<UpdateFailure>,
    /// Which run of the install flow wrote this frame (epoch ms when it was
    /// requested). A window follows one attempt at a time; see the module docs.
    #[serde(default)]
    pub attempt: u64,
}

impl UpdateProgress {
    pub fn phase(phase: &str) -> Self {
        UpdateProgress { phase: phase.into(), ..Default::default() }
    }

    pub fn failed(failure: UpdateFailure) -> Self {
        UpdateProgress { phase: "error".into(), failure: Some(failure), ..Default::default() }
    }
}

/// The arg the window launches Rillio with to ask for a retry.
pub const RETRY_ARG: &str = "--retry-update";

/// How long a window waits for the main process to answer a retry (the first
/// frame of the new attempt) before saying so. Covers a cold Rillio boot: the
/// answer is written before the network check, so no request time counts.
pub const RETRY_ACK_TIMEOUT: Duration = Duration::from_secs(45);

/// The installer gets this long after `installing` to replace the app (its
/// success deletes the progress file); longer means NSIS aborted silently.
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(120);

/// No new frame for this long while downloading means the update stalled.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(600);

/// The attempt id in a launch's args (`--retry-update <attempt>`).
pub fn retry_arg<I: IntoIterator<Item = S>, S: AsRef<str>>(args: I) -> Option<u64> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg.as_ref() == RETRY_ARG {
            return args.next().and_then(|id| id.as_ref().parse().ok());
        }
    }
    None
}

/// What the main process does with a retry request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryDecision {
    /// Nothing running: start the install flow for this attempt.
    Start,
    /// A flow is already running (a second click, or a request that raced the
    /// previous one): it now reports under this attempt, and its latest frame
    /// was rewritten so the window hears back at once.
    Adopt,
    /// This exact attempt is already the one running.
    Duplicate,
}

/// The main process's side of the protocol: one install flow at a time, its
/// attempt id, and the frame writer (every frame carries the attempt).
#[derive(Default)]
pub struct UpdateRun {
    busy: AtomicBool,
    attempt: AtomicU64,
    last: Mutex<Option<UpdateProgress>>,
}

impl UpdateRun {
    /// The flow is starting from the web UI (toast, Settings). False when one
    /// is already running: the caller reports that instead of starting two.
    pub fn begin(&self, attempt: u64) -> bool {
        if self.busy.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.attempt.store(attempt, Ordering::SeqCst);
        *self.last.lock().unwrap() = None;
        true
    }

    /// A window asked for a retry. `write` is where frames go (the progress
    /// file in production, a recorder in tests).
    pub fn request(&self, attempt: u64, write: impl FnOnce(&UpdateProgress)) -> RetryDecision {
        if !self.busy.swap(true, Ordering::SeqCst) {
            self.attempt.store(attempt, Ordering::SeqCst);
            *self.last.lock().unwrap() = None;
            return RetryDecision::Start;
        }
        if self.attempt.swap(attempt, Ordering::SeqCst) == attempt {
            return RetryDecision::Duplicate;
        }
        let mut last = self.last.lock().unwrap();
        if let Some(frame) = last.as_mut() {
            frame.attempt = attempt;
            write(frame);
        }
        RetryDecision::Adopt
    }

    /// Stamp a frame with the current attempt, remember it (for adoption) and
    /// hand it to `write`.
    pub fn frame(&self, mut frame: UpdateProgress, write: impl FnOnce(&UpdateProgress)) {
        frame.attempt = self.attempt.load(Ordering::SeqCst);
        write(&frame);
        *self.last.lock().unwrap() = Some(frame);
    }

    /// The flow failed and returned: the next request starts a new one.
    pub fn end(&self) {
        self.busy.store(false, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub fn attempt(&self) -> u64 {
        self.attempt.load(Ordering::SeqCst)
    }
}

/// The progress file both processes agree on. Temp dir, not the app config dir:
/// it must be writable before install and irrelevant after (the new app boot
/// deletes it; a leftover from a crashed run is harmless and overwritten).
pub fn progress_path() -> std::path::PathBuf {
    std::env::temp_dir().join("rillio-update-progress.json")
}

/// Atomically-enough write (temp + rename would be overkill for a single small
/// line read by a tolerant poller; a torn read just shows the previous frame).
/// The main process writes through [`UpdateRun::frame`] so frames carry the
/// attempt.
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

/// What the window process does in response to what it sees.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Drive the page: `window.__updateState(<frame>)`.
    Show(UpdateProgress),
    /// The install finished (the new app deleted the file): "Starting
    /// Rillio", then exit.
    Done,
    /// Put the user back in the app they had (after a failed install).
    RelaunchApp,
    /// Launch Rillio with `--retry-update <attempt>`.
    LaunchRetry(u64),
    /// A line for the boot journal (the shell's only log file).
    Journal(String),
}

impl PartialEq for UpdateProgress {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    /// Showing the frames of `WindowMachine::attempt`.
    Following,
    /// Showing a failure and WAITING ON THE USER: no timer closes, relaunches
    /// or changes anything, and stale frames and file deletions are ignored.
    /// Only "Try again", "Close", or a newer attempt (started from the app's
    /// own UI) move it.
    Failed,
    /// "Try again" was pressed: waiting for the first frame of `attempt`.
    Retrying { attempt: u64, since: Instant },
}

/// The update window's logic, free of windows and files so it can be tested:
/// feed it what the progress file holds ([`WindowMachine::observe`]), the
/// clock ([`WindowMachine::tick`]) and the buttons ([`WindowMachine::retry`]),
/// and apply the [`Effect`]s it returns.
pub struct WindowMachine {
    mode: Mode,
    attempt: Option<u64>,
    last_raw: String,
    last_change: Instant,
    installing_since: Option<Instant>,
    missing_since: Option<Instant>,
    version: String,
}

/// A missing progress file is a verdict only once it STAYS missing: a second
/// Rillio launch deletes it at boot, and the running flow rewrites it within
/// a frame.
const MISSING_GRACE: Duration = Duration::from_secs(3);

impl WindowMachine {
    pub fn new(version: &str, now: Instant) -> Self {
        WindowMachine {
            mode: Mode::Following,
            attempt: None,
            last_raw: String::new(),
            last_change: now,
            installing_since: None,
            missing_since: None,
            version: version.into(),
        }
    }

    #[cfg(test)]
    pub fn is_waiting_on_user(&self) -> bool {
        self.mode == Mode::Failed
    }

    /// Follow `attempt` from its first frame on.
    fn follow(&mut self, attempt: u64, now: Instant) {
        self.mode = Mode::Following;
        self.attempt = Some(attempt);
        self.last_raw.clear();
        self.last_change = now;
        self.installing_since = None;
        self.missing_since = None;
    }

    /// `raw` is the progress file's content, `None` when it does not exist.
    pub fn observe(&mut self, raw: Option<&str>, now: Instant) -> Vec<Effect> {
        let Some(raw) = raw else {
            return self.observe_missing(now);
        };
        self.missing_since = None;
        let Ok(frame) = serde_json::from_str::<UpdateProgress>(raw) else {
            return Vec::new(); // torn write; the next poll gets a full frame
        };
        match (self.mode, self.attempt) {
            (Mode::Retrying { attempt, .. }, _) if frame.attempt >= attempt => self.follow(frame.attempt, now),
            (Mode::Retrying { .. }, _) => return Vec::new(),
            // A newer run started from the app's own UI (toast, Settings):
            // its window could not be staged (this one holds the copy), so
            // this one shows it.
            (Mode::Failed, Some(current)) if frame.attempt > current => self.follow(frame.attempt, now),
            (Mode::Failed, _) => return Vec::new(),
            (Mode::Following, None) => self.follow(frame.attempt, now),
            (Mode::Following, Some(current)) if frame.attempt > current => self.follow(frame.attempt, now),
            (Mode::Following, Some(current)) if frame.attempt < current => return Vec::new(),
            (Mode::Following, Some(_)) => {}
        }
        if raw == self.last_raw {
            return Vec::new();
        }
        self.last_raw = raw.to_string();
        self.last_change = now;
        if frame.phase == "installing" && self.installing_since.is_none() {
            self.installing_since = Some(now);
        }
        if frame.phase == "error" {
            self.mode = Mode::Failed;
        }
        vec![Effect::Show(frame)]
    }

    fn observe_missing(&mut self, now: Instant) -> Vec<Effect> {
        if self.mode != Mode::Following {
            return Vec::new();
        }
        // During install, the file's deletion is THE success signal: the
        // relaunched new app removes it on boot.
        if self.installing_since.is_some() {
            return vec![Effect::Done];
        }
        let since = *self.missing_since.get_or_insert(now);
        if now.duration_since(since) < MISSING_GRACE {
            return Vec::new();
        }
        self.fail(
            UpdateFailure::classified(
                Stage::Download,
                FailureKind::Other,
                "The update stopped before the download finished: its progress file disappeared (Rillio exited or restarted).",
                &self.version,
            ),
            "download",
        )
    }

    /// The timers. None of them acts while the window waits on the user.
    pub fn tick(&mut self, now: Instant) -> Vec<Effect> {
        match self.mode {
            Mode::Failed => Vec::new(),
            Mode::Retrying { attempt, since } => {
                if now.duration_since(since) < RETRY_ACK_TIMEOUT {
                    return Vec::new();
                }
                let failure = UpdateFailure::plain(
                    Stage::InstallCheck,
                    "Rillio didn't respond when asked to try again.",
                    "Open Rillio and try again from Settings.",
                    format!("No answer to {RETRY_ARG} {attempt} within {}s.", RETRY_ACK_TIMEOUT.as_secs()),
                    &self.version,
                );
                self.fail(failure, "retry")
            }
            Mode::Following => {
                if let Some(t0) = self.installing_since {
                    // ★ The install did NOT complete. Success deletes the
                    // file (the relaunched new app clears it on boot), so
                    // still being here means the installer failed or aborted,
                    // and NSIS aborts SILENTLY in quiet mode (see
                    // updater_copy_path). Presenting that as success once
                    // gave users the old version back with no error. Say it,
                    // and put them back in the working app they had.
                    if now.duration_since(t0) < INSTALL_TIMEOUT {
                        return Vec::new();
                    }
                    let failure = UpdateFailure::classified(
                        Stage::Install,
                        FailureKind::Other,
                        "The installer did not finish within 120 seconds (NSIS quiet mode aborts without a message).",
                        &self.version,
                    );
                    let mut effects = self.fail_journaled(
                        failure,
                        "update-failed stage=installer error=\"the installer did not finish within 120s (NSIS quiet mode aborts silently)\"".into(),
                    );
                    effects.push(Effect::RelaunchApp);
                    return effects;
                }
                if self.attempt.is_some() && now.duration_since(self.last_change) >= STALL_TIMEOUT {
                    let failure = UpdateFailure::classified(
                        Stage::Download,
                        FailureKind::Other,
                        "No download progress for 10 minutes.",
                        &self.version,
                    );
                    return self.fail(failure, "download");
                }
                Vec::new()
            }
        }
    }

    /// "Try again". Only a failed window retries; while a retry is running a
    /// second click does nothing (the button is also disabled on the page).
    pub fn retry(&mut self, now_ms: u64, now: Instant) -> Vec<Effect> {
        if self.mode != Mode::Failed {
            return Vec::new();
        }
        let attempt = now_ms.max(self.attempt.map_or(0, |a| a + 1));
        self.mode = Mode::Retrying { attempt, since: now };
        self.installing_since = None;
        self.missing_since = None;
        self.last_raw.clear();
        let checking = UpdateProgress { attempt, ..UpdateProgress::phase("checking") };
        vec![
            Effect::Journal(format!("update-retry attempt={attempt}")),
            Effect::Show(checking),
            Effect::LaunchRetry(attempt),
        ]
    }

    /// Launching Rillio for the retry failed outright.
    pub fn retry_launch_failed(&mut self, attempt: u64, error: &str) -> Vec<Effect> {
        let failure = UpdateFailure::plain(
            Stage::InstallCheck,
            "Rillio couldn't be started to try again.",
            "Open Rillio and try again from Settings.",
            format!("Launching Rillio with {RETRY_ARG} {attempt} failed: {error}"),
            &self.version,
        );
        self.fail(failure, "retry")
    }

    fn fail(&mut self, failure: UpdateFailure, label: &str) -> Vec<Effect> {
        let line = format!("update-failed stage={label} error={:?}", failure.message);
        self.fail_journaled(failure, line)
    }

    fn fail_journaled(&mut self, failure: UpdateFailure, journal: String) -> Vec<Effect> {
        self.mode = Mode::Failed;
        let frame = UpdateProgress { attempt: self.attempt.unwrap_or(0), ..UpdateProgress::failed(failure) };
        vec![Effect::Journal(journal), Effect::Show(frame)]
    }
}

/// Everything the window's commands and its poll thread share.
struct WindowShared {
    machine: Mutex<WindowMachine>,
    window: Mutex<Option<tauri::WebviewWindow>>,
    app_exe: Option<std::path::PathBuf>,
    identifier: String,
}

impl WindowShared {
    fn apply(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Show(frame) => {
                    if let (Ok(json), Some(window)) = (serde_json::to_string(&frame), self.window.lock().unwrap().as_ref()) {
                        let _ = window.eval(&format!("window.__updateState({json})"));
                    }
                }
                Effect::Done => {
                    if let Some(window) = self.window.lock().unwrap().as_ref() {
                        let _ = window.eval("window.__updateState({phase:'restarting'})");
                    }
                    std::thread::sleep(Duration::from_secs(2));
                    std::process::exit(0);
                }
                Effect::RelaunchApp => {
                    if let Some(app_exe) = self.app_exe.as_ref() {
                        if let Err(e) = std::process::Command::new(app_exe).spawn() {
                            tracing::error!("update-window: could not relaunch Rillio: {e}");
                        }
                    }
                }
                Effect::LaunchRetry(attempt) => {
                    let launched = match self.app_exe.as_ref() {
                        None => Err("the app's path was not passed to the update window".to_string()),
                        Some(exe) => std::process::Command::new(exe)
                            .arg(RETRY_ARG)
                            .arg(attempt.to_string())
                            .spawn()
                            .map(|_| ())
                            .map_err(|e| format!("{}: {e}", exe.display())),
                    };
                    if let Err(e) = launched {
                        let effects = self.machine.lock().unwrap().retry_launch_failed(attempt, &e);
                        self.apply(effects);
                    }
                }
                Effect::Journal(line) => crate::boot_journal_append(&self.identifier, &line),
            }
        }
    }
}

/// The page's "Try again".
#[tauri::command]
fn update_window_retry(shared: tauri::State<'_, std::sync::Arc<WindowShared>>) {
    let effects = shared
        .machine
        .lock()
        .unwrap()
        .retry(crate::error_chain::now_ms(), Instant::now());
    shared.apply(effects);
}

/// The page's "Close": the only way a failed window goes away.
#[tauri::command]
fn update_window_close() {
    std::process::exit(0);
}

/// The `--update-window` process entry: a minimal Tauri app (no plugins, no
/// server) with one small frameless window on its OWN WebView2 profile,
/// polling the progress file through [`WindowMachine`], plus the two commands
/// its page calls (Try again, Close).
pub fn run(ctx: tauri::Context<tauri::Wry>) {
    // The installed app's path, handed over by spawn_update_window. This
    // process is a renamed COPY, so current_exe() would relaunch the updater
    // rather than Rillio.
    let app_exe: Option<std::path::PathBuf> = std::env::args_os()
        .skip_while(|arg| arg != "--update-window")
        .nth(1)
        .map(std::path::PathBuf::from);
    let shared = std::sync::Arc::new(WindowShared {
        machine: Mutex::new(WindowMachine::new(&ctx.package_info().version.to_string(), Instant::now())),
        window: Mutex::new(None),
        app_exe,
        identifier: ctx.config().identifier.clone(),
    });
    let poll_shared = shared.clone();
    let result = tauri::Builder::default()
        .manage(shared)
        .invoke_handler(tauri::generate_handler![update_window_retry, update_window_close])
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
            *poll_shared.window.lock().unwrap() = Some(window);

            // Poll the progress file and drive the page. WebviewWindow is Send:
            // eval marshals onto the right thread internally. There is no
            // overall time cap any more: a failed window waits for the user
            // (Try again / Close), and every other state has its own timer in
            // WindowMachine::tick.
            let shared = poll_shared.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(150));
                let raw = std::fs::read_to_string(progress_path()).ok();
                let now = Instant::now();
                let effects = {
                    let mut machine = shared.machine.lock().unwrap();
                    let mut effects = machine.observe(raw.as_deref(), now);
                    effects.extend(machine.tick(now));
                    effects
                };
                shared.apply(effects);
            });
            Ok(())
        })
        .run(ctx);
    if let Err(e) = result {
        tracing::error!("update-window: failed to run: {}", crate::error_chain::error_chain(&e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(phase: &str, attempt: u64) -> String {
        serde_json::to_string(&UpdateProgress { attempt, ..UpdateProgress::phase(phase) }).unwrap()
    }

    fn failed(attempt: u64) -> String {
        let failure = UpdateFailure::classified(Stage::Download, FailureKind::Reset, "reset (os error 10054)", "0.1.45");
        serde_json::to_string(&UpdateProgress { attempt, ..UpdateProgress::failed(failure) }).unwrap()
    }

    fn shown_phases(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Show(f) => Some(f.phase.clone()),
                _ => None,
            })
            .collect()
    }

    fn failed_machine(t0: Instant) -> WindowMachine {
        let mut m = WindowMachine::new("0.1.45", t0);
        m.observe(Some(&frame("downloading", 100)), t0);
        assert_eq!(shown_phases(&m.observe(Some(&failed(100)), t0)), vec!["error"]);
        assert!(m.is_waiting_on_user());
        m
    }

    #[test]
    fn retry_arg_parses_only_its_own_flag() {
        assert_eq!(retry_arg(["rillio-desktop.exe", "--retry-update", "1790000000000"]), Some(1_790_000_000_000));
        assert_eq!(retry_arg(["rillio-desktop.exe"]), None);
        assert_eq!(retry_arg(["x", "--retry-update"]), None);
        assert_eq!(retry_arg(["x", "--retry-update", "soon"]), None);
    }

    #[test]
    fn a_failed_window_waits_on_the_user() {
        let t0 = Instant::now();
        let mut m = failed_machine(t0);
        let later = t0 + Duration::from_secs(3600);
        // No timer acts, the file disappearing (a Rillio relaunch deletes it)
        // does nothing, and stale frames of the failed attempt are ignored.
        assert!(m.tick(later).is_empty());
        assert!(m.observe(None, later).is_empty());
        assert!(m.observe(None, later + MISSING_GRACE * 2).is_empty());
        assert!(m.observe(Some(&frame("downloading", 100)), later).is_empty());
        assert!(m.observe(Some(&frame("downloading", 99)), later).is_empty());
        assert!(m.is_waiting_on_user());
    }

    #[test]
    fn try_again_launches_rillio_once_and_follows_the_answer() {
        let t0 = Instant::now();
        let mut m = failed_machine(t0);
        let effects = m.retry(5_000, t0);
        assert_eq!(effects.last(), Some(&Effect::LaunchRetry(5_000)));
        assert_eq!(shown_phases(&effects), vec!["checking"], "the page leaves the failed state at once");
        assert!(effects.contains(&Effect::Journal("update-retry attempt=5000".into())));
        // A second click while that runs does nothing.
        assert!(m.retry(6_000, t0).is_empty());
        // The old attempt's error frame (still in the file) is not a verdict.
        assert!(m.observe(Some(&failed(100)), t0).is_empty());
        // The main process answers under the new attempt: followed.
        assert_eq!(shown_phases(&m.observe(Some(&frame("checking", 5_000)), t0)), vec!["checking"]);
        assert_eq!(shown_phases(&m.observe(Some(&frame("downloading", 5_000)), t0)), vec!["downloading"]);
        assert!(!m.is_waiting_on_user());
    }

    #[test]
    fn an_unanswered_retry_becomes_a_failure_on_screen() {
        let t0 = Instant::now();
        let mut m = failed_machine(t0);
        m.retry(5_000, t0);
        assert!(m.tick(t0 + RETRY_ACK_TIMEOUT - Duration::from_secs(1)).is_empty());
        let effects = m.tick(t0 + RETRY_ACK_TIMEOUT);
        let Some(Effect::Show(frame)) = effects.iter().find(|e| matches!(e, Effect::Show(_))) else { panic!("{effects:?}") };
        assert_eq!(frame.phase, "error");
        assert_eq!(frame.failure.as_ref().unwrap().summary, "Rillio didn't respond when asked to try again.");
        assert!(m.is_waiting_on_user());
        // ...from which the user can try again.
        assert_eq!(m.retry(9_000, t0).last(), Some(&Effect::LaunchRetry(9_000)));
    }

    #[test]
    fn a_launch_that_fails_is_a_failure_on_screen() {
        let t0 = Instant::now();
        let mut m = failed_machine(t0);
        m.retry(5_000, t0);
        let effects = m.retry_launch_failed(5_000, "access denied");
        assert_eq!(shown_phases(&effects), vec!["error"]);
        assert!(m.is_waiting_on_user());
    }

    #[test]
    fn a_failed_retry_shows_its_own_failure_and_a_success_finishes() {
        let t0 = Instant::now();
        let mut m = failed_machine(t0);
        m.retry(5_000, t0);
        m.observe(Some(&frame("downloading", 5_000)), t0);
        assert_eq!(shown_phases(&m.observe(Some(&failed(5_000)), t0)), vec!["error"]);
        m.retry(7_000, t0);
        m.observe(Some(&frame("installing", 7_000)), t0);
        // The new app deleting the file during install is success.
        assert_eq!(m.observe(None, t0 + Duration::from_secs(20)), vec![Effect::Done]);
    }

    #[test]
    fn a_silent_installer_abort_fails_and_reopens_rillio_once() {
        let t0 = Instant::now();
        let mut m = WindowMachine::new("0.1.45", t0);
        m.observe(Some(&frame("installing", 100)), t0);
        assert!(m.tick(t0 + INSTALL_TIMEOUT - Duration::from_secs(1)).is_empty());
        let effects = m.tick(t0 + INSTALL_TIMEOUT);
        assert!(effects.contains(&Effect::RelaunchApp));
        assert_eq!(shown_phases(&effects), vec!["error"]);
        // ...then waits: the relaunched app deleting the file, and more time,
        // change nothing (no second relaunch, no exit).
        assert!(m.observe(None, t0 + INSTALL_TIMEOUT * 2).is_empty());
        assert!(m.tick(t0 + INSTALL_TIMEOUT * 10).is_empty());
    }

    #[test]
    fn a_newer_run_started_from_the_app_takes_over_a_failed_window() {
        let t0 = Instant::now();
        let mut m = failed_machine(t0);
        assert_eq!(shown_phases(&m.observe(Some(&frame("downloading", 200)), t0)), vec!["downloading"]);
        assert!(!m.is_waiting_on_user());
    }

    #[test]
    fn a_vanished_or_stalled_download_fails_instead_of_closing() {
        let t0 = Instant::now();
        let mut m = WindowMachine::new("0.1.45", t0);
        m.observe(Some(&frame("downloading", 100)), t0);
        // A brief absence (a second Rillio launch deletes the file) is not a verdict.
        assert!(m.observe(None, t0).is_empty());
        assert!(m.observe(Some(&frame("downloading", 100)), t0 + Duration::from_secs(1)).is_empty());
        assert!(m.observe(None, t0 + Duration::from_secs(2)).is_empty());
        assert_eq!(shown_phases(&m.observe(None, t0 + Duration::from_secs(6))), vec!["error"]);

        let mut stalled = WindowMachine::new("0.1.45", t0);
        stalled.observe(Some(&frame("downloading", 100)), t0);
        assert!(stalled.tick(t0 + STALL_TIMEOUT - Duration::from_secs(1)).is_empty());
        assert_eq!(shown_phases(&stalled.tick(t0 + STALL_TIMEOUT)), vec!["error"]);
    }

    fn recorder() -> (std::sync::Arc<Mutex<Vec<UpdateProgress>>>, impl Fn(&UpdateProgress) + Clone) {
        let frames = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = frames.clone();
        (frames, move |f: &UpdateProgress| sink.lock().unwrap().push(f.clone()))
    }

    #[test]
    fn a_retry_request_starts_the_flow_once() {
        let run = UpdateRun::default();
        let (frames, write) = recorder();
        assert_eq!(run.request(5_000, write.clone()), RetryDecision::Start);
        assert_eq!(run.attempt(), 5_000);
        // The flow's frames carry the attempt the window waits for.
        run.frame(UpdateProgress::phase("checking"), write.clone());
        assert_eq!(frames.lock().unwrap().last().unwrap().attempt, 5_000);
        // The same request again (a double launch) starts nothing.
        assert_eq!(run.request(5_000, write.clone()), RetryDecision::Duplicate);
        // A web-UI install while it runs is refused, not doubled.
        assert!(!run.begin(6_000));
        // After the flow fails, the next request starts a new one.
        run.end();
        assert_eq!(run.request(7_000, write), RetryDecision::Start);
    }

    #[test]
    fn a_request_during_a_running_flow_is_answered_at_once() {
        let run = UpdateRun::default();
        let (frames, write) = recorder();
        assert!(run.begin(100));
        run.frame(UpdateProgress { downloaded: 10, total: 40, ..UpdateProgress::phase("downloading") }, write.clone());
        assert_eq!(run.request(5_000, write.clone()), RetryDecision::Adopt);
        let last = frames.lock().unwrap().last().unwrap().clone();
        assert_eq!((last.phase.as_str(), last.attempt, last.downloaded), ("downloading", 5_000, 10));
        // Later frames carry the adopted attempt too.
        run.frame(UpdateProgress::phase("installing"), write);
        assert_eq!(frames.lock().unwrap().last().unwrap().attempt, 5_000);
    }
}

