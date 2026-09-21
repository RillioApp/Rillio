//! Supervision of the dub pack's GPU sidecars (docs/dubbing/stage1-pipeline,
//! decisions D1/D1b): `llama-server` (translator), `llama-tts-server`
//! (VoxCPM2) and `whisper-server` (ASR), all Vulkan builds shipped with the
//! app, all loopback HTTP.
//!
//! One [`Supervisor`] owns the child processes. Each start binds a free
//! loopback port, spawns the process with its stdout/stderr in per-sidecar
//! log files, and waits for the health endpoint before returning; the dub
//! worker calls [`Supervisor::check`] before each request so a crashed
//! sidecar is reported instead of a hung request.
//!
//! Lifetime: every child is killed when its [`Sidecar`] drops, and on
//! Windows every child is also assigned to a job object with
//! KILL_ON_JOB_CLOSE, so the sidecars die with the shell even when the shell
//! is terminated or crashes (the kernel closes the job handle).
//!
//! Blocking API: the health probes run through `tauri::async_runtime::block_on`
//! (the crate's HTTP pattern, see transcribe.rs), so call this from a plain
//! thread such as the dub worker, never from inside the async runtime.

use std::fs::File;
use std::io::{BufRead as _, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long a sidecar may take from spawn to a healthy `/health`. Model loads
/// dominate: the 2B translator and VoxCPM2 read a few GB from disk each.
pub const STARTUP_DEADLINE: Duration = Duration::from_secs(120);
/// Per-request bound on one health probe (connect + response).
pub const HEALTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Pause between health probes while a sidecar is starting.
pub const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How much of a dead sidecar's stderr rides in the error.
pub const STDERR_TAIL_LINES: usize = 20;
/// A port allocated free can be taken between allocation and the child's
/// bind; the start is retried this many times with a fresh port.
const PORT_COLLISION_RETRIES: u32 = 1;
const LOOPBACK: &str = "127.0.0.1";
/// The engine name `llama-tts-server` reports; a different one means the
/// wrong binary or the wrong build answered.
const TTS_ENGINE: &str = "voxcpm2";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarKind {
    Translator,
    Tts,
    Asr,
}

impl SidecarKind {
    /// Log-file stem and error prefix.
    pub fn name(self) -> &'static str {
        match self {
            SidecarKind::Translator => "translator",
            SidecarKind::Tts => "tts",
            SidecarKind::Asr => "asr",
        }
    }

    /// The executable's file name inside the installed pack (decision D6a:
    /// the binaries travel in the pack, next to the weights).
    pub fn exe_name(self) -> &'static str {
        match self {
            SidecarKind::Translator => "llama-server.exe",
            SidecarKind::Tts => "llama-tts-server.exe",
            SidecarKind::Asr => "whisper-server.exe",
        }
    }
}

/// The model files a sidecar loads; the variant fixes the kind.
#[derive(Clone, Debug)]
pub enum SidecarModels {
    /// `llama-server -m <gguf>`.
    Translator { gguf: PathBuf },
    /// `llama-tts-server --voxcpm2-base-lm <gguf> --voxcpm2-acoustic <gguf>`.
    Tts { base_lm: PathBuf, acoustic: PathBuf },
    /// `whisper-server -m <ggml model> -nfa -dtw <preset>`: DTW token
    /// timestamps (the recognizer's token points, `dubclients::Heard`) need
    /// flash attention off (whisper.cpp disables DTW under it); the preset
    /// names the model's size, so it travels with the model file.
    Asr { model: PathBuf, dtw_preset: String },
}

impl SidecarModels {
    pub fn kind(&self) -> SidecarKind {
        match self {
            SidecarModels::Translator { .. } => SidecarKind::Translator,
            SidecarModels::Tts { .. } => SidecarKind::Tts,
            SidecarModels::Asr { .. } => SidecarKind::Asr,
        }
    }

    /// The full command line for the given port. The translator flags are
    /// exactly the G3 bench's (bench/prod parity).
    fn args(&self, port: u16) -> Vec<String> {
        let path = |p: &Path| p.to_string_lossy().into_owned();
        let mut args: Vec<String> = match self {
            SidecarModels::Translator { gguf } => vec![
                "-m".into(),
                path(gguf),
                "-ngl".into(),
                "99".into(),
                "-c".into(),
                "4096".into(),
                "--parallel".into(),
                "1".into(),
            ],
            SidecarModels::Tts { base_lm, acoustic } => vec![
                "--voxcpm2-base-lm".into(),
                path(base_lm),
                "--voxcpm2-acoustic".into(),
                path(acoustic),
            ],
            SidecarModels::Asr { model, dtw_preset } => vec!["-m".into(), path(model), "-nfa".into(), "-dtw".into(), dtw_preset.clone()],
        };
        args.extend(["--host".into(), LOOPBACK.into(), "--port".into(), port.to_string()]);
        args
    }
}

/// One running sidecar process. Dropping it kills the process.
#[derive(Debug)]
pub struct Sidecar {
    kind: SidecarKind,
    exe: PathBuf,
    args: Vec<String>,
    port: u16,
    child: Child,
    /// Kept so `restart` relaunches the same models.
    models: Option<SidecarModels>,
    stderr_log: PathBuf,
}

impl Sidecar {
    fn url(&self) -> String {
        format!("http://{LOOPBACK}:{}", self.port)
    }

    /// `Some(status)` once the process has exited.
    fn exited(&mut self) -> Result<Option<std::process::ExitStatus>, String> {
        self.child
            .try_wait()
            .map_err(|e| format!("sidecar {}: polling the process: {e}", self.kind.name()))
    }

    fn exit_error(&self, status: std::process::ExitStatus, when: &str) -> String {
        format!(
            "sidecar {}: {} exited {when} ({status}); args {:?}; stderr tail:\n{}",
            self.kind.name(),
            self.exe.display(),
            self.args,
            stderr_tail(&self.stderr_log)
        )
    }

    fn kill(&mut self) {
        // kill() on an already-exited process is an error we do not care
        // about; wait() reaps it either way.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.kill();
    }
}

pub struct Supervisor {
    log_dir: PathBuf,
    client: reqwest::Client,
    /// At most one entry per kind.
    sidecars: Vec<Sidecar>,
    #[cfg(windows)]
    job: KillOnCloseJob,
}

impl Supervisor {
    /// `log_dir` receives `<kind>.stdout.log` / `<kind>.stderr.log`, truncated
    /// on every start.
    pub fn new(log_dir: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(&log_dir)
            .map_err(|e| format!("sidecar: creating log dir {}: {e}", log_dir.display()))?;
        let client = reqwest::Client::builder()
            .timeout(HEALTH_REQUEST_TIMEOUT)
            .connect_timeout(HEALTH_REQUEST_TIMEOUT)
            .build()
            .map_err(|e| format!("sidecar: http client: {e}"))?;
        Ok(Self {
            log_dir,
            client,
            sidecars: Vec::new(),
            #[cfg(windows)]
            job: KillOnCloseJob::new()?,
        })
    }

    /// Spawn `exe` with the flags for `models` on a fresh loopback port and
    /// wait until its health endpoint answers. A sidecar of the same kind that
    /// is already running is stopped first.
    pub fn start(&mut self, exe: &Path, models: SidecarModels) -> Result<(), String> {
        let kind = models.kind();
        self.stop(kind);
        let mut attempt = 0;
        loop {
            let port = free_loopback_port()?;
            match self.launch(kind, exe, models.args(port), port) {
                Ok(mut sidecar) => {
                    sidecar.models = Some(models);
                    self.sidecars.push(sidecar);
                    return Ok(());
                }
                // The child died before health AND the port is now held by
                // someone else: it lost a race for the port, not a real fault.
                Err(e) if attempt < PORT_COLLISION_RETRIES && e.contains("exited before health") && !port_is_free(port) => {
                    attempt += 1;
                    tracing::warn!("sidecar {}: port {port} was taken at spawn time, retrying ({e})", kind.name());
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Base URL of a started sidecar (`http://127.0.0.1:<port>`), whether or
    /// not it is currently healthy.
    pub fn url(&self, kind: SidecarKind) -> Option<String> {
        self.get(kind).map(Sidecar::url)
    }

    /// Process alive and `/health` ok. The dub worker calls this before each
    /// request so a crash surfaces as a clear error, not a hung request.
    pub fn check(&mut self, kind: SidecarKind) -> Result<(), String> {
        let client = self.client.clone();
        let sidecar = self
            .get_mut(kind)
            .ok_or_else(|| format!("sidecar {}: not started", kind.name()))?;
        if let Some(status) = sidecar.exited()? {
            return Err(sidecar.exit_error(status, "while in service"));
        }
        health(&client, kind, &sidecar.url())
    }

    /// Kill the sidecar and start it again with the same exe and models (on
    /// a fresh port: read `url` again afterwards).
    pub fn restart(&mut self, kind: SidecarKind) -> Result<(), String> {
        let index = self
            .index(kind)
            .ok_or_else(|| format!("sidecar {}: cannot restart, never started", kind.name()))?;
        let mut old = self.sidecars.swap_remove(index);
        old.kill();
        let models = old
            .models
            .take()
            .ok_or_else(|| format!("sidecar {}: cannot restart a test-attached sidecar", kind.name()))?;
        let exe = old.exe.clone();
        drop(old);
        self.start(&exe, models)
    }

    /// Kill one sidecar; a no-op when it is not running.
    pub fn stop(&mut self, kind: SidecarKind) {
        if let Some(index) = self.index(kind) {
            let mut sidecar = self.sidecars.swap_remove(index);
            sidecar.kill();
            tracing::info!("sidecar {}: stopped", kind.name());
        }
    }

    pub fn stop_all(&mut self) {
        for kind in [SidecarKind::Translator, SidecarKind::Tts, SidecarKind::Asr] {
            self.stop(kind);
        }
    }

    fn index(&self, kind: SidecarKind) -> Option<usize> {
        self.sidecars.iter().position(|s| s.kind == kind)
    }

    fn get(&self, kind: SidecarKind) -> Option<&Sidecar> {
        self.index(kind).map(|i| &self.sidecars[i])
    }

    fn get_mut(&mut self, kind: SidecarKind) -> Option<&mut Sidecar> {
        self.index(kind).map(move |i| &mut self.sidecars[i])
    }

    /// Spawn with the given command line, register the child for
    /// kill-on-exit, and block until `/health` answers or the process dies or
    /// the deadline passes.
    fn launch(&self, kind: SidecarKind, exe: &Path, args: Vec<String>, port: u16) -> Result<Sidecar, String> {
        let stdout_log = self.log_dir.join(format!("{}.stdout.log", kind.name()));
        let stderr_log = self.log_dir.join(format!("{}.stderr.log", kind.name()));
        let open_log = |p: &Path| File::create(p).map_err(|e| format!("sidecar {}: creating log {}: {e}", kind.name(), p.display()));
        let stdout = open_log(&stdout_log)?;
        let stderr = open_log(&stderr_log)?;
        let child = Command::new(exe)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|e| format!("sidecar {}: spawning {}: {e}", kind.name(), exe.display()))?;
        #[cfg(windows)]
        self.job.assign(&child)?;
        tracing::info!(
            "sidecar {}: spawned {} pid {} on port {port}; logs {}",
            kind.name(),
            exe.display(),
            child.id(),
            self.log_dir.display()
        );
        let mut sidecar = Sidecar { kind, exe: exe.to_path_buf(), args, port, child, models: None, stderr_log };
        self.wait_healthy(&mut sidecar)?;
        Ok(sidecar)
    }

    fn wait_healthy(&self, sidecar: &mut Sidecar) -> Result<(), String> {
        let started = Instant::now();
        let url = sidecar.url();
        loop {
            if let Some(status) = sidecar.exited()? {
                return Err(sidecar.exit_error(status, "before health"));
            }
            match health(&self.client, sidecar.kind, &url) {
                Ok(()) => {
                    tracing::info!("sidecar {}: healthy after {:?}", sidecar.kind.name(), started.elapsed());
                    return Ok(());
                }
                Err(e) if started.elapsed() >= STARTUP_DEADLINE => {
                    sidecar.kill();
                    return Err(format!(
                        "sidecar {}: not healthy after {STARTUP_DEADLINE:?} (last probe: {e}); killed; stderr tail:\n{}",
                        sidecar.kind.name(),
                        stderr_tail(&sidecar.stderr_log)
                    ));
                }
                Err(_) => std::thread::sleep(HEALTH_POLL_INTERVAL),
            }
        }
    }

    /// Register an externally spawned process as a sidecar of `kind` on
    /// `port`, skipping spawn and the health wait: lets the tests exercise
    /// `url`/`check`/`stop` against a stand-in server without a model.
    #[cfg(test)]
    fn attach_for_test(&mut self, kind: SidecarKind, port: u16, child: Child) -> Result<(), String> {
        #[cfg(windows)]
        self.job.assign(&child)?;
        self.sidecars.push(Sidecar {
            kind,
            exe: PathBuf::from("<attached>"),
            args: Vec::new(),
            port,
            child,
            models: None,
            stderr_log: self.log_dir.join(format!("{}.stderr.log", kind.name())),
        });
        Ok(())
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // Explicit so the order is visible: children first (each Sidecar's
        // Drop kills its process), then the job handle closes and the kernel
        // reaps anything the kill missed (grandchildren, a hung TerminateProcess).
        self.sidecars.clear();
    }
}

/// GET `<base_url>/health`, requiring HTTP 200 and `status == "ok"` (both
/// llama-server and whisper-server answer 503 while a model loads), plus the
/// engine name for the TTS server.
fn health(client: &reqwest::Client, kind: SidecarKind, base_url: &str) -> Result<(), String> {
    let url = format!("{base_url}/health");
    let (status, body) = tauri::async_runtime::block_on(async {
        let response = client.get(&url).send().await.map_err(|e| format!("{url}: {e}"))?;
        let status = response.status();
        let body = response.text().await.map_err(|e| format!("{url}: reading the body: {e}"))?;
        Ok::<_, String>((status, body))
    })?;
    if status != reqwest::StatusCode::OK {
        return Err(format!("{url}: HTTP {status}: {}", body.trim()));
    }
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("{url}: not JSON ({e}): {}", body.trim()))?;
    if json.get("status").and_then(|v| v.as_str()) != Some("ok") {
        return Err(format!("{url}: status not ok: {}", body.trim()));
    }
    if kind == SidecarKind::Tts {
        let engine = json.get("engine").and_then(|v| v.as_str());
        if engine != Some(TTS_ENGINE) {
            return Err(format!("{url}: engine {engine:?}, expected {TTS_ENGINE:?}"));
        }
    }
    Ok(())
}

/// Ask the OS for a free loopback port and release it. The child binds it
/// itself; the window between release and bind is what `start` retries on.
pub fn free_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind((LOOPBACK, 0)).map_err(|e| format!("sidecar: allocating a loopback port: {e}"))?;
    let port = listener.local_addr().map_err(|e| format!("sidecar: reading the allocated port: {e}"))?.port();
    drop(listener);
    Ok(port)
}

fn port_is_free(port: u16) -> bool {
    TcpListener::bind((LOOPBACK, port)).is_ok()
}

/// The last [`STDERR_TAIL_LINES`] lines of a sidecar's stderr log, or a note
/// when the log cannot be read.
fn stderr_tail(path: &Path) -> String {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => return format!("<no stderr log at {}: {e}>", path.display()),
    };
    let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
    let skip = lines.len().saturating_sub(STDERR_TAIL_LINES);
    let tail = lines[skip..].join("\n");
    if tail.trim().is_empty() {
        "<stderr empty>".into()
    } else {
        tail
    }
}

/// A Windows job object with KILL_ON_JOB_CLOSE. Every sidecar is assigned to
/// it right after spawn; when this process dies for any reason the kernel
/// closes the handle and terminates every process in the job, so no GPU
/// sidecar outlives the shell.
#[cfg(windows)]
struct KillOnCloseJob(windows::Win32::Foundation::HANDLE);

// The handle is a kernel object usable from any thread.
#[cfg(windows)]
unsafe impl Send for KillOnCloseJob {}

#[cfg(windows)]
impl KillOnCloseJob {
    fn new() -> Result<Self, String> {
        use windows::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        unsafe {
            let handle = CreateJobObjectW(None, windows::core::PCWSTR::null())
                .map_err(|e| format!("sidecar: creating the job object: {e}"))?;
            let job = Self(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .map_err(|e| format!("sidecar: setting kill-on-close on the job object: {e}"))?;
            Ok(job)
        }
    }

    fn assign(&self, child: &Child) -> Result<(), String> {
        use std::os::windows::io::AsRawHandle as _;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::AssignProcessToJobObject;
        unsafe { AssignProcessToJobObject(self.0, HANDLE(child.as_raw_handle() as *mut _)) }
            .map_err(|e| format!("sidecar: assigning pid {} to the job object: {e}", child.id()))
    }
}

#[cfg(windows)]
impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // Closing the last handle is what triggers KILL_ON_JOB_CLOSE.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// A loopback HTTP responder that answers every request with one fixed
    /// status line and body: enough to emulate `/health` without a model.
    struct StandIn {
        port: u16,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl StandIn {
        fn serve(status_line: &'static str, body: &'static str) -> Self {
            let listener = TcpListener::bind((LOOPBACK, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_flag = stop.clone();
            let thread = std::thread::spawn(move || {
                while !stop_flag.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            // Consume the request head; the probe sends no body.
                            let mut buf = [0u8; 1024];
                            let mut head = Vec::new();
                            loop {
                                let n = stream.read(&mut buf).unwrap_or(0);
                                head.extend_from_slice(&buf[..n]);
                                if n == 0 || head.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            let response = format!(
                                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.write_all(response.as_bytes());
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("stand-in accept: {e}"),
                    }
                }
            });
            Self { port, stop, thread: Some(thread) }
        }

        fn url(&self) -> String {
            format!("http://{LOOPBACK}:{}", self.port)
        }

        fn stop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                t.join().unwrap();
            }
        }
    }

    impl Drop for StandIn {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn test_log_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rillio-sidecar-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().timeout(HEALTH_REQUEST_TIMEOUT).build().unwrap()
    }

    /// A child that stays alive until killed: `pause` blocks on a stdin pipe
    /// nothing ever writes to.
    #[cfg(windows)]
    fn keep_alive_child() -> Child {
        Command::new("cmd.exe")
            .args(["/c", "pause"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn free_port_is_bindable() {
        let port = free_loopback_port().unwrap();
        assert_ne!(port, 0);
        TcpListener::bind((LOOPBACK, port)).expect("the allocated port binds");
    }

    #[test]
    fn health_contract_per_kind() {
        let client = client();
        let ok = StandIn::serve("200 OK", r#"{"status":"ok"}"#);
        assert_eq!(health(&client, SidecarKind::Translator, &ok.url()), Ok(()));
        assert_eq!(health(&client, SidecarKind::Asr, &ok.url()), Ok(()));
        // The TTS contract also names the engine; a plain ok is the wrong binary.
        let err = health(&client, SidecarKind::Tts, &ok.url()).unwrap_err();
        assert!(err.contains("engine None") && err.contains("voxcpm2"), "{err}");

        let tts = StandIn::serve("200 OK", r#"{"status":"ok","engine":"voxcpm2"}"#);
        assert_eq!(health(&client, SidecarKind::Tts, &tts.url()), Ok(()));
        let other_engine = StandIn::serve("200 OK", r#"{"status":"ok","engine":"piper"}"#);
        let err = health(&client, SidecarKind::Tts, &other_engine.url()).unwrap_err();
        assert!(err.contains("\"piper\""), "{err}");

        // llama-server / whisper-server while loading.
        let loading = StandIn::serve("503 Service Unavailable", r#"{"error":{"message":"Loading model"}}"#);
        let err = health(&client, SidecarKind::Translator, &loading.url()).unwrap_err();
        assert!(err.contains("HTTP 503") && err.contains("Loading model"), "{err}");

        let not_json = StandIn::serve("200 OK", "<html>");
        let err = health(&client, SidecarKind::Asr, &not_json.url()).unwrap_err();
        assert!(err.contains("not JSON"), "{err}");

        // Nothing listening: the error names the url.
        let port = free_loopback_port().unwrap();
        let err = health(&client, SidecarKind::Translator, &format!("http://{LOOPBACK}:{port}")).unwrap_err();
        assert!(err.contains(&format!(":{port}/health")), "{err}");
    }

    /// Cross-witness with the pack manifest: every sidecar the supervisor
    /// expects is a file the pack installs.
    #[test]
    fn every_sidecar_exe_is_in_the_pack_manifest() {
        for kind in [SidecarKind::Translator, SidecarKind::Tts, SidecarKind::Asr] {
            assert!(
                crate::packs::MANIFEST.iter().any(|f| f.name == kind.exe_name()),
                "{} missing from packs::MANIFEST",
                kind.exe_name()
            );
        }
    }

    #[test]
    fn bogus_exe_fails_loudly() {
        let mut sup = Supervisor::new(test_log_dir("bogus")).unwrap();
        let exe = PathBuf::from(r"Z:\definitely\missing\llama-server.exe");
        let err = sup.start(&exe, SidecarModels::Translator { gguf: PathBuf::from("model.gguf") }).unwrap_err();
        assert!(err.starts_with("sidecar translator: spawning"), "{err}");
        assert!(err.contains("llama-server.exe"), "{err}");
        assert_eq!(sup.url(SidecarKind::Translator), None);
    }

    #[cfg(windows)]
    #[test]
    fn exit_before_health_carries_exit_code_and_stderr_tail() {
        let sup = Supervisor::new(test_log_dir("exit")).unwrap();
        let port = free_loopback_port().unwrap();
        let args: Vec<String> = ["/c", "echo boom 1>&2 & exit 3"].iter().map(|s| s.to_string()).collect();
        let err = sup.launch(SidecarKind::Asr, Path::new("cmd.exe"), args, port).unwrap_err();
        assert!(err.contains("exited before health"), "{err}");
        assert!(err.contains("exit code: 3"), "{err}");
        assert!(err.contains("stderr tail:\nboom"), "{err}");
        // The log survives for post-mortem reading.
        let log = std::fs::read_to_string(sup.log_dir.join("asr.stderr.log")).unwrap();
        assert_eq!(log.trim(), "boom");
    }

    #[cfg(windows)]
    #[test]
    fn attached_sidecar_reports_url_check_and_death() {
        let mut sup = Supervisor::new(test_log_dir("attached")).unwrap();
        let mut server = StandIn::serve("200 OK", r#"{"status":"ok"}"#);
        sup.attach_for_test(SidecarKind::Translator, server.port, keep_alive_child()).unwrap();
        assert_eq!(sup.url(SidecarKind::Translator), Some(server.url()));
        assert_eq!(sup.url(SidecarKind::Tts), None);
        assert_eq!(sup.check(SidecarKind::Translator), Ok(()));
        assert!(sup.check(SidecarKind::Tts).unwrap_err().contains("not started"));

        // The server goes away but the process lives: a health failure.
        server.stop();
        let err = sup.check(SidecarKind::Translator).unwrap_err();
        assert!(err.contains("/health"), "{err}");

        // The process dies: reported with its exit status, and the sidecar
        // stays registered so the caller can decide to restart.
        let watch = ProcessWatch::of(&sup.get(SidecarKind::Translator).unwrap().child);
        sup.get_mut(SidecarKind::Translator).unwrap().kill();
        let err = sup.check(SidecarKind::Translator).unwrap_err();
        assert!(err.contains("exited while in service"), "{err}");
        assert!(!watch.alive());
        // A test-attached sidecar has no models to relaunch: restart refuses.
        let err = sup.restart(SidecarKind::Translator).unwrap_err();
        assert!(err.contains("test-attached"), "{err}");
        assert_eq!(sup.url(SidecarKind::Translator), None);
    }

    #[cfg(windows)]
    #[test]
    fn stop_all_and_drop_kill_children() {
        let mut sup = Supervisor::new(test_log_dir("stop")).unwrap();
        let a = keep_alive_child();
        let b = keep_alive_child();
        let (watch_a, watch_b) = (ProcessWatch::of(&a), ProcessWatch::of(&b));
        sup.attach_for_test(SidecarKind::Tts, 1, a).unwrap();
        sup.attach_for_test(SidecarKind::Asr, 2, b).unwrap();
        assert!(watch_a.alive() && watch_b.alive());
        sup.stop_all();
        assert!(watch_a.exited_within(EXIT_WAIT) && watch_b.exited_within(EXIT_WAIT));
        assert_eq!(sup.url(SidecarKind::Tts), None);

        let c = keep_alive_child();
        let watch_c = ProcessWatch::of(&c);
        sup.attach_for_test(SidecarKind::Tts, 1, c).unwrap();
        drop(sup);
        assert!(watch_c.exited_within(EXIT_WAIT));
    }

    /// The crash guarantee: closing the job handle alone (what the kernel
    /// does when the shell dies) terminates the child, no explicit kill.
    #[cfg(windows)]
    #[test]
    fn closing_the_job_kills_an_unkilled_child() {
        let job = KillOnCloseJob::new().unwrap();
        let mut child = keep_alive_child();
        let watch = ProcessWatch::of(&child);
        job.assign(&child).unwrap();
        assert!(watch.alive());
        drop(job);
        // `pause` never exits on its own; the job closing is the only thing
        // that can end it. (The kernel reports exit code 0 for a kill-on-close
        // termination, so the code carries no signal; the exit itself does.)
        assert!(watch.exited_within(EXIT_WAIT), "child outlived the job");
        assert!(child.try_wait().unwrap().is_some());
    }

    /// Liveness of a child whose `Child` is owned by the supervisor under
    /// test, observed through a duplicated process handle. A handle pins the
    /// process object, so this cannot be fooled by PID reuse (tests run in
    /// parallel and spawn many short-lived processes; a pid lookup was flaky).
    #[cfg(windows)]
    struct ProcessWatch(windows::Win32::Foundation::HANDLE);

    #[cfg(windows)]
    impl ProcessWatch {
        fn of(child: &Child) -> Self {
            use std::os::windows::io::AsRawHandle as _;
            use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
            use windows::Win32::System::Threading::GetCurrentProcess;
            let mut dup = HANDLE::default();
            unsafe {
                let me = GetCurrentProcess();
                DuplicateHandle(me, HANDLE(child.as_raw_handle() as *mut _), me, &mut dup, 0, false, DUPLICATE_SAME_ACCESS)
                    .unwrap();
            }
            Self(dup)
        }

        fn wait(&self, ms: u32) -> windows::Win32::Foundation::WAIT_EVENT {
            unsafe { windows::Win32::System::Threading::WaitForSingleObject(self.0, ms) }
        }

        fn alive(&self) -> bool {
            self.wait(0) == windows::Win32::Foundation::WAIT_TIMEOUT
        }

        fn exited_within(&self, d: Duration) -> bool {
            self.wait(d.as_millis() as u32) == windows::Win32::Foundation::WAIT_OBJECT_0
        }
    }

    #[cfg(windows)]
    impl Drop for ProcessWatch {
        fn drop(&mut self) {
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
        }
    }

    #[cfg(windows)]
    const EXIT_WAIT: Duration = Duration::from_secs(5);
}
