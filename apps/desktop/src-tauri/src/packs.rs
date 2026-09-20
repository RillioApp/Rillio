//! The opt-in "AI dubbing" weights pack (Stage 1, decision D6). A manifest
//! baked into the binary pins the pack to the app release: file name, url,
//! size, sha256. The pack installs into `<app data>/models/` through the
//! ranged downloader and `state.json` next to the files records the pack
//! version and the sha256 each file was verified against, so status is a
//! directory scan plus a hash comparison (never a re-hash of 7 GB, never a
//! re-download). A release whose manifest changes a file re-fetches only that
//! file. The sidecar binaries (the three Vulkan servers and their DLLs,
//! decision D6a) travel in the pack too, pinned by the same hashes, so the
//! installer stays small and an opt-in downloads everything the feature runs.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::download;

pub const PACK_VERSION: u32 = 1;
/// Under the app data dir. Shared with the subtitle feature's whisper model
/// (`transcribe.rs` fetches the same `ggml-small-q5_1.bin` there).
pub const MODELS_DIR: &str = "models";
pub const STATE_FILE: &str = "state.json";
pub const PROGRESS_EVENT: &str = "pack-progress";
/// Dev knob: a staged pack directory (the bench's `E:\packs\dubbing\1`) in
/// place of the installed one. Unset in production.
const PACK_DIR_ENV: &str = "RILLIO_DUB_PACK_DIR";
/// Minimum spacing between progress events while bytes are flowing.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const HASH_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pack {
    Dubbing,
}

pub struct PackFile {
    pub name: &'static str,
    pub url: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    pub part_of: Pack,
}

/// One owner of where the pack is hosted. TODO(manifest): hosting is
/// undecided (a Rillio Hugging Face org vs GitHub release assets, see the
/// Stage 1 open questions), so this is a placeholder. The staged pack that
/// these hashes were measured on is `E:\packs\dubbing\1` on the dev box.
macro_rules! pack_url {
    ($name:literal) => {
        concat!("https://TODO.rillio.app/packs/dubbing/1/", $name)
    };
}

macro_rules! dubbing_file {
    ($name:literal, $bytes:expr, $sha256:literal) => {
        PackFile { name: $name, url: pack_url!($name), bytes: $bytes, sha256: $sha256, part_of: Pack::Dubbing }
    };
}

/// Files the pipeline uses when the pack holds them and runs without when
/// it does not; not verified by the manifest because their bytes are not
/// final. `instrument.onnx` (M3, `instrument.rs`) joins [`MANIFEST`] with its
/// size and sha the day the trained projector is staged; until then a staged
/// pack (`RILLIO_DUB_PACK_DIR`) may carry it and the app picks it up by
/// presence.
pub const OPTIONAL_FILES: &[&str] = &["instrument.onnx"];

/// Sizes and hashes are the real ones (2026-09-14, `Get-FileHash -Algorithm
/// SHA256`): the Stage-0 weights, then the sidecar binaries the Stage 1 bench
/// ran (bench/prod parity): `llama-tts-server` and `whisper-server` built
/// static with Vulkan (they need only the system Vulkan loader and the
/// OpenMP runtime, `vcomp140.dll`, from the VC redistributable), and the
/// upstream prebuilt Vulkan `llama-server` b10944 with the DLLs it loads
/// (`dumpbin /DEPENDENTS` plus the `ggml-cpu-*` variants ggml picks at run
/// time). Everything lands flat in one directory: the static servers import
/// none of the DLLs, so the two sets cannot collide.
pub const MANIFEST: &[PackFile] = &[
    dubbing_file!("Qwen3.5-2B-Q8_0.gguf", 2_012_012_800, "1b04acba824817554f4ce23639bc8495ff70453b8fcb047900c731521021f2c1"),
    dubbing_file!("Qwen3.5-4B-Q8_0.gguf", 4_482_403_488, "10cc391b403021dd11c614679d2fd92f611c3681d29e29651b717316965d61e1"),
    dubbing_file!("VoxCPM2-BaseLM-F16.gguf", 3_247_980_544, "8be62e899f8f32b3c2109c99950b46671135a8f40b27b1df720f6823d36cc59e"),
    dubbing_file!("VoxCPM2-Acoustic-F16.gguf", 1_825_096_352, "5bde898488ad635ff55d24da53543768fa33d5e5cdc538ce190e5ef831038e85"),
    dubbing_file!("ggml-small-q5_1.bin", 190_085_487, "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb"),
    // The unfused fp16 export: the DirectML file (D1c; the fused MHA file has no DML kernel).
    dubbing_file!("bs_roformer_ep368_fp16.onnx", 325_476_114, "07b501c4a76d3d50d2f5b57e168991b2d7ee6cdbffb749120c1141c8f36037ef"),
    dubbing_file!("ge2e_resemblyzer.onnx", 5_700_109, "0592d111e58f058d421330813a856f64b6a53bd263db32f75fb7ad70ff4b9726"),
    // ONNX Runtime 1.24, the DirectML build (the `onnxruntime-directml` wheel's DLLs).
    dubbing_file!("onnxruntime.dll", 21_111_832, "302c69f9779d63ef4ab90316e59444c4acbaca7fe3455020d79d10bcfcb00715"),
    dubbing_file!("DirectML.dll", 18_527_776, "b73972115320e906a49602f2027a3266622881b0d325ba685e0f165a9482a8d7"),
    dubbing_file!("llama-tts-server.exe", 63_913_984, "f498ec404770f65940797c0125826a14f5bb74aeb4f3265b6226e0b7a684a0ee"),
    dubbing_file!("whisper-server.exe", 57_719_808, "928f01abb0125345655853e6646107f34d4fbd02bf6c3e50e65bf789e8102727"),
    dubbing_file!("vcomp140.dll", 193_152, "55aba23cdcd6484fbb06f4155b8ca75adfce7a881f10afd0c49457165e677164"),
    dubbing_file!("llama-server.exe", 9_216, "e12147c768d9a87710fde0ceb8e2fa0c5b0f5096018f93cb5ab034fe437f877b"),
    dubbing_file!("llama-server-impl.dll", 8_904_192, "e4b8822bf8d52a35f3b7aa34957a47897b53aef2f8cd274ac03f3682e08fc214"),
    dubbing_file!("llama-common.dll", 7_772_672, "8c707d48fe84620670eb2e97f4bb2598e068eed4fd2aa2d1a9161f157821a0ce"),
    dubbing_file!("llama.dll", 3_149_312, "a694105379ad3e2394e3cccd95a5265f9cdfaefa01d3e5fe175c3c06f3759ffa"),
    dubbing_file!("mtmd.dll", 1_772_032, "9bf7260fe990ab26ed461069e180c5ea85defb0580839fc600dd57224906d586"),
    dubbing_file!("ggml.dll", 79_872, "863724c88c1621780dcefbedadcb412745214784701f87b4ccf13f1b606fce13"),
    dubbing_file!("ggml-base.dll", 796_160, "c00ce658e4825bec7a081db13d1c9e778065a9c2b3dab05aa2171bee73e6b154"),
    dubbing_file!("ggml-vulkan.dll", 43_259_392, "b77d71e16c51eaf2f7189b0168cfa7821f13ebc9f5d27d8c61235d157c57079c"),
    dubbing_file!("libomp.dll", 768_000, "a12116ba72d1d6820407cf30be23da04ce79d6bb8a71a5ee71759c5a1faa6f1c"),
    dubbing_file!("ggml-cpu-x64.dll", 924_160, "a4bc1876fe899d53c93e068dada714b83e8eafdb9381ad4804e720060cd40591"),
    dubbing_file!("ggml-cpu-sse42.dll", 932_352, "9f90204afbd28f01cda3da242615233da6c2f670bcaa744077f4c7600d128361"),
    dubbing_file!("ggml-cpu-sandybridge.dll", 1_107_968, "90fa36aa942e504712d5ab7c3d686a7544136adee6031ad2d3e43c662ecb9c04"),
    dubbing_file!("ggml-cpu-ivybridge.dll", 1_127_424, "348931f8923c327aa6653041e2af8063efcc38b44d9b69fd4a26ab668af3eae2"),
    dubbing_file!("ggml-cpu-piledriver.dll", 1_131_520, "1cfa72d8513c1cbc5d3e8d5530fc0682db7d2278303c868028c3a05db14d7b2d"),
    dubbing_file!("ggml-cpu-haswell.dll", 1_235_968, "d7fa544ee0c9f2980e306fddc6431cba76969b5029778cae1c28434d6efb01ab"),
    dubbing_file!("ggml-cpu-alderlake.dll", 1_230_336, "3ef50bc8b589e1bae55e1ff5b3dbf2e71dc5c469e7afaa2fc8863a7f6a89f714"),
    dubbing_file!("ggml-cpu-skylakex.dll", 1_441_792, "217a5517c8333eafb2db1931ca3f86e629d42c8c17f7c405c472623fc5f1813b"),
    dubbing_file!("ggml-cpu-cascadelake.dll", 1_434_112, "1b7ba0c2d824baf5b11ab475ac96fcdbd5e718f0c783f296e624d5baf9f043a2"),
    dubbing_file!("ggml-cpu-cooperlake.dll", 1_434_624, "0908a4bfe3be0d066be688441c6b1ede47560c470effb4f52333dd97879b6d62"),
    dubbing_file!("ggml-cpu-cannonlake.dll", 1_448_448, "ffc7d31fa18d71409a880d90d07e7f02bc5757d275b3f1360f579e1ebfb4b148"),
    dubbing_file!("ggml-cpu-icelake.dll", 1_440_768, "0602dca8a715971f7811ebbdefa4755fd29856ca579841f7614aa99a4c89b3a2"),
    dubbing_file!("ggml-cpu-sapphirerapids.dll", 1_711_616, "ce343f3194f916de77a526b274cb809622c2e286349d985899b745f7aba71992"),
    dubbing_file!("ggml-cpu-zen4.dll", 1_441_280, "73439c46830a7bb35a5970b0ef3d7e1f454c2e275111baec8e0d48420817cb2b"),
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileStatus {
    pub name: String,
    pub bytes: u64,
    /// The file exists on disk.
    pub present: bool,
    /// Present with the manifest's size and recorded in `state.json` under the
    /// manifest's sha256: an install keeps it.
    pub verified: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PackStatus {
    /// The version this build ships ([`PACK_VERSION`]).
    pub version: u32,
    /// Every file verified and `state.json` at this version.
    pub installed: bool,
    pub files: Vec<FileStatus>,
    pub bytes_total: u64,
    /// Bytes of verified files: what an install does not fetch.
    pub bytes_present: u64,
}

/// One `pack-progress` event. `done` files of `total` are complete for this
/// install; `bytes_*` count the files this install fetches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Progress {
    pub file: String,
    pub done: usize,
    pub total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// `state.json`: the pack version last installed and, per file name, the
/// sha256 the file on disk was verified against.
#[derive(Debug, Default, Serialize, Deserialize)]
struct InstalledState {
    version: u32,
    files: BTreeMap<String, String>,
}

/// Set while an install runs: a second install (or a remove) refuses.
#[derive(Default)]
pub struct PackState {
    installing: AtomicBool,
}

struct InstallGuard<'a>(&'a AtomicBool);

impl<'a> InstallGuard<'a> {
    fn acquire(flag: &'a AtomicBool) -> Result<Self, String> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "already installing".to_string())?;
        Ok(Self(flag))
    }
}

impl Drop for InstallGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Where the pack lives: the models dir under app data, unless
/// `RILLIO_DUB_PACK_DIR` points a dev shell at a staged pack.
pub(crate) fn pack_dir(app: &AppHandle) -> Result<PathBuf, String> {
    match std::env::var(PACK_DIR_ENV) {
        Ok(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => models_dir(app),
    }
}

fn models_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join(MODELS_DIR))
        .map_err(|e| format!("pack: app data dir: {e}"))
}

#[tauri::command]
pub async fn pack_status(app: AppHandle) -> Result<PackStatus, String> {
    status(&pack_dir(&app)?)
}

#[tauri::command]
pub async fn pack_install(app: AppHandle, state: State<'_, PackState>) -> Result<(), String> {
    let _guard = InstallGuard::acquire(&state.installing)?;
    let dir = pack_dir(&app)?;
    install(&dir, |progress| {
        if let Err(e) = app.emit(PROGRESS_EVENT, &progress) {
            tracing::warn!("pack: emit failed: {e}");
        }
    })
    .await
}

#[tauri::command]
pub async fn pack_remove(app: AppHandle, state: State<'_, PackState>) -> Result<(), String> {
    let _guard = InstallGuard::acquire(&state.installing)?;
    remove(&pack_dir(&app)?)
}

pub fn status(dir: &Path) -> Result<PackStatus, String> {
    let mut status = status_of(PACK_VERSION, MANIFEST, Pack::Dubbing, dir)?;
    if staged_pack() {
        adopt_staged(&mut status);
    }
    Ok(status)
}

/// A dev shell pointed at a staged pack through `RILLIO_DUB_PACK_DIR`.
fn staged_pack() -> bool {
    std::env::var(PACK_DIR_ENV).map_or(false, |dir| !dir.is_empty())
}

/// A staged pack is the developer's truth: it carries rebuilt sidecars and
/// merged models whose sizes are not the manifest's, so presence is
/// verification there. Production never takes this path (the env var is
/// unset), where size and the recorded hash decide.
fn adopt_staged(status: &mut PackStatus) {
    for file in &mut status.files {
        file.verified = file.present;
    }
    status.installed = status.files.iter().all(|f| f.present);
    status.bytes_present = status.files.iter().filter(|f| f.verified).map(|f| f.bytes).sum();
}

pub async fn install(dir: &Path, on_progress: impl Fn(Progress) + Send + Sync) -> Result<(), String> {
    install_with(PACK_VERSION, MANIFEST, Pack::Dubbing, dir, on_progress).await
}

pub fn remove(dir: &Path) -> Result<(), String> {
    remove_of(MANIFEST, Pack::Dubbing, dir)
}

fn files_of(manifest: &[PackFile], pack: Pack) -> impl Iterator<Item = &PackFile> {
    manifest.iter().filter(move |f| f.part_of == pack)
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

/// A missing `state.json` is an empty state; an unreadable or malformed one is
/// an error (remove clears it).
fn load_state(dir: &Path) -> Result<InstalledState, String> {
    let path = state_path(dir);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("pack: {} is malformed: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(InstalledState::default()),
        Err(e) => Err(format!("pack: reading {}: {e}", path.display())),
    }
}

fn save_state(dir: &Path, state: &InstalledState) -> Result<(), String> {
    let path = state_path(dir);
    let tmp = dir.join(format!("{STATE_FILE}.tmp"));
    std::fs::create_dir_all(dir).map_err(|e| format!("pack: creating {}: {e}", dir.display()))?;
    let json = serde_json::to_vec_pretty(state).map_err(|e| format!("pack: encoding {}: {e}", path.display()))?;
    std::fs::write(&tmp, json).map_err(|e| format!("pack: writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("pack: placing {}: {e}", path.display()))
}

/// The file's size on disk, `None` when absent.
fn size_on_disk(path: &Path) -> Result<Option<u64>, String> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(meta.len())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("pack: reading {}: {e}", path.display())),
    }
}

fn file_status(file: &PackFile, state: &InstalledState, dir: &Path) -> Result<FileStatus, String> {
    let size = size_on_disk(&dir.join(file.name))?;
    let recorded = state.files.get(file.name).map(String::as_str) == Some(file.sha256);
    Ok(FileStatus {
        name: file.name.to_owned(),
        bytes: file.bytes,
        present: size.is_some(),
        verified: size == Some(file.bytes) && recorded,
    })
}

fn status_of(version: u32, manifest: &[PackFile], pack: Pack, dir: &Path) -> Result<PackStatus, String> {
    let state = load_state(dir)?;
    let files = files_of(manifest, pack).map(|file| file_status(file, &state, dir)).collect::<Result<Vec<_>, _>>()?;
    Ok(PackStatus {
        version,
        installed: state.version == version && files.iter().all(|f| f.verified),
        bytes_total: files.iter().map(|f| f.bytes).sum(),
        bytes_present: files.iter().filter(|f| f.verified).map(|f| f.bytes).sum(),
        files,
    })
}

fn remove_if_present(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("pack: removing {}: {e}", path.display())),
    }
}

fn sha256_of_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("pack: opening {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("pack: reading {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// A right-sized file that `state.json` does not vouch for (dropped in by
/// hand, or fetched by the subtitle feature): hashing it once is far cheaper
/// than re-downloading it, so it is adopted when the hash matches.
async fn adopt_if_matching(file: &PackFile, dir: &Path) -> Result<bool, String> {
    let path = dir.join(file.name);
    if size_on_disk(&path)? != Some(file.bytes) {
        return Ok(false);
    }
    let actual = tokio::task::spawn_blocking(move || sha256_of_file(&path))
        .await
        .map_err(|e| format!("pack: {}: hash task: {e}", file.name))??;
    Ok(actual == file.sha256)
}

struct Throttle(Mutex<Instant>);

impl Throttle {
    fn new() -> Self {
        Self(Mutex::new(Instant::now()))
    }

    fn due(&self) -> bool {
        let mut last = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let due = last.elapsed() >= PROGRESS_INTERVAL;
        if due {
            *last = Instant::now();
        }
        due
    }
}

/// Fetches every file of the pack that is not verified, one at a time,
/// recording each in `state.json` as it lands (a failure on file four must
/// not cost files one to three), deletes files an older manifest recorded
/// that this one no longer lists, and stamps the version last.
async fn install_with(
    version: u32,
    manifest: &[PackFile],
    pack: Pack,
    dir: &Path,
    on_progress: impl Fn(Progress) + Send + Sync,
) -> Result<(), String> {
    let mut state = load_state(dir)?;
    let mut stale = Vec::new();
    for file in files_of(manifest, pack) {
        if file_status(file, &state, dir)?.verified {
            continue;
        }
        if adopt_if_matching(file, dir).await? {
            tracing::info!("pack: {} already on disk with the right hash, adopted", file.name);
            state.files.insert(file.name.to_owned(), file.sha256.to_owned());
            save_state(dir, &state)?;
            continue;
        }
        stale.push(file);
    }
    let total = stale.len();
    let bytes_total: u64 = stale.iter().map(|f| f.bytes).sum();
    let throttle = Throttle::new();
    let mut bytes_before = 0u64;
    for (done, file) in stale.iter().enumerate() {
        let event = |bytes_done: u64| Progress { file: file.name.to_owned(), done, total, bytes_done, bytes_total };
        on_progress(event(bytes_before));
        download::fetch(file.url, &dir.join(file.name), file.bytes, file.sha256, |file_done, _| {
            if throttle.due() {
                on_progress(event(bytes_before + file_done));
            }
        })
        .await
        .map_err(|e| format!("pack: {}: {e}", file.name))?;
        bytes_before += file.bytes;
        state.files.insert(file.name.to_owned(), file.sha256.to_owned());
        save_state(dir, &state)?;
        on_progress(Progress { file: file.name.to_owned(), done: done + 1, total, bytes_done: bytes_before, bytes_total });
    }
    let orphans: Vec<String> =
        state.files.keys().filter(|name| !manifest.iter().any(|f| f.name == name.as_str())).cloned().collect();
    for name in orphans {
        tracing::info!("pack: {name} is no longer in the manifest, removing");
        remove_if_present(&dir.join(&name))?;
        state.files.remove(&name);
    }
    state.version = version;
    save_state(dir, &state)
}

/// Deletes the pack's files, every file `state.json` records (an older
/// manifest may have named others) and `state.json` itself. A malformed
/// `state.json` cannot name its files; it is reported and deleted anyway,
/// since remove is the way out of that state.
fn remove_of(manifest: &[PackFile], pack: Pack, dir: &Path) -> Result<(), String> {
    let mut names: Vec<String> = files_of(manifest, pack).map(|f| f.name.to_owned()).collect();
    match load_state(dir) {
        Ok(state) => names.extend(state.files.into_keys()),
        Err(e) => tracing::warn!("pack: removing without the recorded file list: {e}"),
    }
    for name in names {
        remove_if_present(&dir.join(name))?;
    }
    remove_if_present(&state_path(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::extract::{Path as UrlPath, State};
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;

    const FILE_A: &str = "a.gguf";
    const FILE_B: &str = "b.onnx";
    const FILE_A_BYTES: usize = 300 * 1024 + 11;
    const FILE_B_BYTES: usize = 500 * 1024 + 7;

    /// Deterministic pseudo-random bytes (xorshift64), seeded per file.
    fn body(seed: u64, len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ seed;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn sha256_of(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    struct Served {
        files: Mutex<HashMap<String, Vec<u8>>>,
        /// The file name of every request, in arrival order (probes included).
        requests: Mutex<Vec<String>>,
    }

    impl Served {
        fn new() -> Arc<Self> {
            Arc::new(Self { files: Mutex::new(HashMap::new()), requests: Mutex::new(Vec::new()) })
        }

        fn put(&self, name: &str, bytes: Vec<u8>) {
            self.files.lock().unwrap().insert(name.to_owned(), bytes);
        }

        fn take_requests(&self) -> Vec<String> {
            std::mem::take(&mut *self.requests.lock().unwrap())
        }
    }

    async fn serve(State(s): State<Arc<Served>>, UrlPath(name): UrlPath<String>, headers: HeaderMap) -> Response {
        s.requests.lock().unwrap().push(name.clone());
        let Some(body) = s.files.lock().unwrap().get(&name).cloned() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let Some(range) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
            return (StatusCode::OK, body).into_response();
        };
        let (start, end) = range
            .strip_prefix("bytes=")
            .and_then(|r| r.split_once('-'))
            .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
            .expect("client sends bytes=a-b");
        (
            StatusCode::PARTIAL_CONTENT,
            [(header::CONTENT_RANGE, format!("bytes {start}-{end}/{}", body.len()))],
            body[start..=end].to_vec(),
        )
            .into_response()
    }

    /// Starts the file server on a free port; returns its base url.
    async fn start_server(served: Arc<Served>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/{name}", get(serve)).with_state(served);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}")
    }

    /// A manifest entry for `bytes` served at `base/name`. Leaked: the
    /// manifest type is `'static` because the real one is baked in.
    fn entry(base: &str, name: &'static str, bytes: &[u8]) -> PackFile {
        PackFile {
            name,
            url: Box::leak(format!("{base}/{name}").into_boxed_str()),
            bytes: bytes.len() as u64,
            sha256: Box::leak(sha256_of(bytes).into_boxed_str()),
            part_of: Pack::Dubbing,
        }
    }

    struct Fixture {
        served: Arc<Served>,
        base: String,
        dir: PathBuf,
        a: Vec<u8>,
        b: Vec<u8>,
        manifest: Vec<PackFile>,
    }

    async fn fixture(name: &str) -> Fixture {
        let served = Served::new();
        let base = start_server(served.clone()).await;
        let a = body(1, FILE_A_BYTES);
        let b = body(2, FILE_B_BYTES);
        served.put(FILE_A, a.clone());
        served.put(FILE_B, b.clone());
        let manifest = vec![entry(&base, FILE_A, &a), entry(&base, FILE_B, &b)];
        let dir = std::env::temp_dir().join(format!("rillio-packs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Fixture { served, base, dir, a, b, manifest }
    }

    fn listing(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut names: Vec<String> = entries.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    fn state_json(dir: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(state_path(dir)).unwrap()).unwrap()
    }

    fn requested_names(served: &Served) -> Vec<String> {
        let mut names = served.take_requests();
        names.sort();
        names.dedup();
        names
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fresh_install_writes_every_file_and_state() {
        let f = fixture("fresh").await;
        let before = status_of(1, &f.manifest, Pack::Dubbing, &f.dir).unwrap();
        assert!(!before.installed);
        assert!(before.files.iter().all(|s| !s.present && !s.verified));
        assert_eq!(before.bytes_total, (FILE_A_BYTES + FILE_B_BYTES) as u64);
        assert_eq!(before.bytes_present, 0);

        let events: Arc<Mutex<Vec<Progress>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        install_with(1, &f.manifest, Pack::Dubbing, &f.dir, move |p| sink.lock().unwrap().push(p)).await.unwrap();

        assert_eq!(std::fs::read(f.dir.join(FILE_A)).unwrap(), f.a);
        assert_eq!(std::fs::read(f.dir.join(FILE_B)).unwrap(), f.b);
        assert_eq!(listing(&f.dir), vec![FILE_A, FILE_B, STATE_FILE], "no parts or tmp files left");
        assert_eq!(
            state_json(&f.dir),
            serde_json::json!({ "version": 1, "files": { FILE_A: sha256_of(&f.a), FILE_B: sha256_of(&f.b) } })
        );
        assert_eq!(requested_names(&f.served), vec![FILE_A, FILE_B]);

        let after = status_of(1, &f.manifest, Pack::Dubbing, &f.dir).unwrap();
        assert!(after.installed);
        assert!(after.files.iter().all(|s| s.present && s.verified));
        assert_eq!(after.bytes_present, after.bytes_total);

        let events = events.lock().unwrap();
        let total = (FILE_A_BYTES + FILE_B_BYTES) as u64;
        assert_eq!(events.first().unwrap(), &Progress { file: FILE_A.into(), done: 0, total: 2, bytes_done: 0, bytes_total: total });
        assert_eq!(events.last().unwrap(), &Progress { file: FILE_B.into(), done: 2, total: 2, bytes_done: total, bytes_total: total });
        assert!(events.windows(2).all(|w| w[0].bytes_done <= w[1].bytes_done && w[0].done <= w[1].done), "{events:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_verifies_from_state_without_the_network() {
        let f = fixture("status").await;
        install_with(1, &f.manifest, Pack::Dubbing, &f.dir, |_| {}).await.unwrap();
        f.served.take_requests();

        for _ in 0..2 {
            let status = status_of(1, &f.manifest, Pack::Dubbing, &f.dir).unwrap();
            assert!(status.installed);
        }

        assert!(f.served.take_requests().is_empty(), "status must never touch the server");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn changed_hash_marks_that_file_stale_and_install_refetches_only_it() {
        let f = fixture("stale").await;
        install_with(1, &f.manifest, Pack::Dubbing, &f.dir, |_| {}).await.unwrap();
        f.served.take_requests();

        // The next release ships a new b.onnx; a.gguf is unchanged.
        let b2 = body(3, FILE_B_BYTES + 100);
        f.served.put(FILE_B, b2.clone());
        let manifest2 = vec![entry(&f.base, FILE_A, &f.a), entry(&f.base, FILE_B, &b2)];

        let status = status_of(2, &manifest2, Pack::Dubbing, &f.dir).unwrap();
        assert!(!status.installed);
        assert_eq!(status.files[0], FileStatus { name: FILE_A.into(), bytes: f.a.len() as u64, present: true, verified: true });
        assert_eq!(status.files[1], FileStatus { name: FILE_B.into(), bytes: b2.len() as u64, present: true, verified: false });
        assert_eq!(status.bytes_present, f.a.len() as u64);

        let events: Arc<Mutex<Vec<Progress>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        install_with(2, &manifest2, Pack::Dubbing, &f.dir, move |p| sink.lock().unwrap().push(p)).await.unwrap();

        assert_eq!(requested_names(&f.served), vec![FILE_B], "only the changed file is fetched");
        assert_eq!(std::fs::read(f.dir.join(FILE_A)).unwrap(), f.a);
        assert_eq!(std::fs::read(f.dir.join(FILE_B)).unwrap(), b2);
        assert_eq!(
            state_json(&f.dir),
            serde_json::json!({ "version": 2, "files": { FILE_A: sha256_of(&f.a), FILE_B: sha256_of(&b2) } })
        );
        assert!(status_of(2, &manifest2, Pack::Dubbing, &f.dir).unwrap().installed);
        let events = events.lock().unwrap();
        assert!(events.iter().all(|e| e.file == FILE_B && e.total == 1 && e.bytes_total == b2.len() as u64), "{events:?}");

        // A version bump with no changed file: not installed until the version
        // is stamped, and stamping it fetches nothing.
        let status = status_of(3, &manifest2, Pack::Dubbing, &f.dir).unwrap();
        assert!(!status.installed);
        assert!(status.files.iter().all(|s| s.verified));
        install_with(3, &manifest2, Pack::Dubbing, &f.dir, |_| {}).await.unwrap();
        assert!(f.served.take_requests().is_empty());
        assert!(status_of(3, &manifest2, Pack::Dubbing, &f.dir).unwrap().installed);
        assert_eq!(state_json(&f.dir)["version"], 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn install_adopts_a_matching_file_already_on_disk() {
        let f = fixture("adopt").await;
        std::fs::create_dir_all(&f.dir).unwrap();
        std::fs::write(f.dir.join(FILE_A), &f.a).unwrap();
        // Right size, wrong bytes: must be fetched, not adopted.
        let mut b_wrong = f.b.clone();
        b_wrong[0] ^= 0xFF;
        std::fs::write(f.dir.join(FILE_B), &b_wrong).unwrap();

        install_with(1, &f.manifest, Pack::Dubbing, &f.dir, |_| {}).await.unwrap();

        assert_eq!(requested_names(&f.served), vec![FILE_B]);
        assert_eq!(std::fs::read(f.dir.join(FILE_B)).unwrap(), f.b);
        assert!(status_of(1, &f.manifest, Pack::Dubbing, &f.dir).unwrap().installed);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn install_removes_files_the_manifest_dropped() {
        let f = fixture("orphan").await;
        install_with(1, &f.manifest, Pack::Dubbing, &f.dir, |_| {}).await.unwrap();

        let manifest2 = vec![entry(&f.base, FILE_A, &f.a)];
        install_with(2, &manifest2, Pack::Dubbing, &f.dir, |_| {}).await.unwrap();

        assert_eq!(listing(&f.dir), vec![FILE_A, STATE_FILE]);
        assert_eq!(state_json(&f.dir), serde_json::json!({ "version": 2, "files": { FILE_A: sha256_of(&f.a) } }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_deletes_everything() {
        let f = fixture("remove").await;
        install_with(1, &f.manifest, Pack::Dubbing, &f.dir, |_| {}).await.unwrap();

        remove_of(&f.manifest, Pack::Dubbing, &f.dir).unwrap();

        assert!(listing(&f.dir).is_empty(), "{:?}", listing(&f.dir));
        let status = status_of(1, &f.manifest, Pack::Dubbing, &f.dir).unwrap();
        assert!(!status.installed);
        assert!(status.files.iter().all(|s| !s.present));
        remove_of(&f.manifest, Pack::Dubbing, &f.dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_state_fails_status_and_remove_clears_it() {
        let f = fixture("malformed").await;
        std::fs::create_dir_all(&f.dir).unwrap();
        std::fs::write(state_path(&f.dir), b"{ not json").unwrap();

        let err = status_of(1, &f.manifest, Pack::Dubbing, &f.dir).unwrap_err();
        assert!(err.contains(STATE_FILE) && err.contains("malformed"), "{err}");

        remove_of(&f.manifest, Pack::Dubbing, &f.dir).unwrap();
        assert!(listing(&f.dir).is_empty());
    }

    #[test]
    fn manifest_entries_are_well_formed() {
        for file in MANIFEST {
            assert!(file.bytes > 0, "{}", file.name);
            assert_eq!(file.sha256.len(), 64, "{}", file.name);
            assert!(file.sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')), "{}", file.name);
            assert!(file.url.ends_with(file.name), "{}", file.name);
            assert!(!file.name.contains(['/', '\\']), "{}", file.name);
        }
    }
}
