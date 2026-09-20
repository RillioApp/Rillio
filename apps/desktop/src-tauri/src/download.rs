//! Parallel ranged download of one large file (Stage 1 model fetch, decision
//! D6). Hugging Face caps a single connection at roughly 0.5 MB/s and drops
//! long-lived connections, and its redirect tokens expire, so the recipe that
//! works (Stage 0, `ranged-download.ps1`) is: split the file into `PARTS`
//! ranges fetched in parallel; each part is fetched as sequential pieces of
//! `PIECE_BYTES`, appended to its own `.partN` file only once complete, so a
//! dropped connection costs one piece; every piece is requested against the
//! ORIGINAL url so the redirect re-resolves; a stalled connection is aborted
//! fast and retried with backoff. Resume keeps complete pieces in the part
//! files and discards partial ones. The parts are joined into `dest`
//! atomically and the sha256 verified.

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use sha2::{Digest as _, Sha256};

/// Parallel connections. Four is what Stage 0 measured as the sweet spot.
pub const PARTS: usize = 4;
/// Bytes per sequential piece within a part: the most a dropped connection costs.
pub const PIECE_BYTES: u64 = 64 * 1024 * 1024;
/// A connection delivering less than this over `STALL_WINDOW` is treated as
/// silently stalled by the CDN: the piece is aborted and refetched on a fresh
/// connection.
pub const STALL_MIN_BYTES_PER_SEC: u64 = 200_000;
pub const STALL_WINDOW: Duration = Duration::from_secs(15);
/// Attempts per piece before the whole fetch fails.
pub const MAX_PIECE_ATTEMPTS: u32 = 40;
pub const RETRY_BACKOFF_FIRST: Duration = Duration::from_secs(2);
pub const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Minimum spacing between progress callbacks while bytes are flowing.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
/// Read buffer for the join + hash pass.
const JOIN_BUFFER_BYTES: usize = 1024 * 1024;
const SHA256_HEX_LEN: usize = 64;

#[derive(Clone, Debug)]
pub struct Config {
    pub parts: usize,
    pub piece_bytes: u64,
    pub stall_min_bytes_per_sec: u64,
    pub stall_window: Duration,
    pub max_piece_attempts: u32,
    pub retry_backoff_first: Duration,
    pub retry_backoff_max: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            parts: PARTS,
            piece_bytes: PIECE_BYTES,
            stall_min_bytes_per_sec: STALL_MIN_BYTES_PER_SEC,
            stall_window: STALL_WINDOW,
            max_piece_attempts: MAX_PIECE_ATTEMPTS,
            retry_backoff_first: RETRY_BACKOFF_FIRST,
            retry_backoff_max: RETRY_BACKOFF_MAX,
        }
    }
}

/// Fetches `url` into `dest` with the default [`Config`]. `progress` receives
/// `(bytes done, bytes total)`; bytes already on disk from a previous run count
/// as done. Any failure leaves no `dest`; part files survive a transport
/// failure (so the next call resumes) but not a size or hash mismatch.
// Not wired to a Tauri command yet (Stage 1 lands that with the model manager).
#[allow(dead_code)]
pub async fn fetch(
    url: &str,
    dest: &Path,
    expected_bytes: u64,
    sha256_hex: &str,
    progress: impl Fn(u64, u64) + Send + Sync,
) -> Result<(), String> {
    fetch_with(&Config::default(), url, dest, expected_bytes, sha256_hex, progress).await
}

async fn fetch_with(
    cfg: &Config,
    url: &str,
    dest: &Path,
    expected_bytes: u64,
    sha256_hex: &str,
    progress: impl Fn(u64, u64) + Send + Sync,
) -> Result<(), String> {
    if cfg.parts == 0 || cfg.piece_bytes == 0 {
        return Err("download: parts and piece size must be at least 1".into());
    }
    if expected_bytes == 0 {
        return Err("download: expected size must be at least 1 byte".into());
    }
    if sha256_hex.len() != SHA256_HEX_LEN || !sha256_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("download: {sha256_hex:?} is not a sha256 hex digest"));
    }
    if dest.file_name().is_none() {
        return Err(format!("download: {} has no file name", dest.display()));
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("download: creating {}: {e}", dir.display()))?;
    }

    let client = reqwest::Client::builder()
        .user_agent(concat!("Rillio/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("download: http client: {e}"))?;

    let size = probe_size(&client, url).await?;
    if size != expected_bytes {
        return Err(format!("download: server reports {size} bytes, expected {expected_bytes}"));
    }

    let ranges = part_ranges(size, cfg.parts);
    let reporter = Reporter::new(progress, size);
    let fetches = ranges
        .iter()
        .map(|range| fetch_part(cfg, &client, url, part_path(dest, range.index), *range, &reporter));
    futures_util::future::try_join_all(fetches).await?;

    let parts: Vec<(PathBuf, u64)> = ranges.iter().map(|r| (part_path(dest, r.index), r.len())).collect();
    let dest_owned = dest.to_path_buf();
    let expected_hash = sha256_hex.to_ascii_lowercase();
    tokio::task::spawn_blocking(move || join_and_verify(&dest_owned, &parts, size, &expected_hash))
        .await
        .map_err(|e| format!("download: join task: {e}"))??;
    reporter.report_now();
    Ok(())
}

/// One part's inclusive byte range.
#[derive(Clone, Copy, Debug)]
struct PartRange {
    index: usize,
    start: u64,
    end: u64,
}

impl PartRange {
    fn len(&self) -> u64 {
        self.end - self.start + 1
    }
}

/// Splits `size` bytes into `parts` equal ranges (the last one shorter). A file
/// smaller than `parts` bytes yields fewer ranges, never an empty one.
fn part_ranges(size: u64, parts: usize) -> Vec<PartRange> {
    let chunk = size.div_ceil(parts as u64);
    (0..parts)
        .map(|index| (index, index as u64 * chunk))
        .take_while(|(_, start)| *start < size)
        .map(|(index, start)| PartRange { index, start, end: (start + chunk - 1).min(size - 1) })
        .collect()
}

fn sibling(dest: &Path, suffix: &str) -> PathBuf {
    let mut name = dest.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    dest.with_file_name(name)
}

fn part_path(dest: &Path, index: usize) -> PathBuf {
    sibling(dest, &format!(".part{index}"))
}

fn piece_path(part: &Path) -> PathBuf {
    sibling(part, ".piece")
}

fn tmp_path(dest: &Path) -> PathBuf {
    sibling(dest, ".tmp")
}

/// Establishes the size with a one-byte ranged GET: a 206 with a
/// `Content-Range: bytes 0-0/<total>` proves range support and yields the
/// total in one round trip. A 200 means the server ignores `Range`, which
/// makes the whole scheme impossible, so it is an error rather than a
/// single-connection fallback.
async fn probe_size(client: &reqwest::Client, url: &str) -> Result<u64, String> {
    let response = client
        .get(url)
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|e| format!("download: probing {url}: {e}"))?;
    let status = response.status();
    if status == StatusCode::OK {
        return Err(format!("download: {url} ignores Range requests (answered 200 to bytes=0-0); parallel ranged download needs range support"));
    }
    if status != StatusCode::PARTIAL_CONTENT {
        return Err(format!("download: probing {url}: HTTP {status}"));
    }
    let content_range = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| format!("download: {url} answered 206 without a Content-Range header"))?;
    let total = content_range
        .rsplit_once('/')
        .and_then(|(_, total)| total.trim().parse::<u64>().ok())
        .ok_or_else(|| format!("download: {url}: cannot read the total from Content-Range {content_range:?}"))?;
    Ok(total)
}

/// Throttled `(done, total)` progress shared by all part fetches.
struct Reporter<P: Fn(u64, u64) + Send + Sync> {
    progress: P,
    total: u64,
    done: AtomicU64,
    last_report: Mutex<Instant>,
}

impl<P: Fn(u64, u64) + Send + Sync> Reporter<P> {
    fn new(progress: P, total: u64) -> Self {
        Self { progress, total, done: AtomicU64::new(0), last_report: Mutex::new(Instant::now()) }
    }

    fn add(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
        let due = {
            let mut last = self.last_report.lock().unwrap_or_else(|p| p.into_inner());
            let due = last.elapsed() >= PROGRESS_INTERVAL;
            if due {
                *last = Instant::now();
            }
            due
        };
        if due {
            self.report_now();
        }
    }

    /// Un-counts the bytes of a piece that failed and will be refetched.
    fn discard(&self, bytes: u64) {
        self.done.fetch_sub(bytes, Ordering::Relaxed);
    }

    fn report_now(&self) {
        (self.progress)(self.done.load(Ordering::Relaxed), self.total);
    }
}

/// Fetches one part into its `.partN` file, piece by piece, resuming from the
/// complete pieces the file already holds.
async fn fetch_part<P: Fn(u64, u64) + Send + Sync>(
    cfg: &Config,
    client: &reqwest::Client,
    url: &str,
    part: PathBuf,
    range: PartRange,
    reporter: &Reporter<P>,
) -> Result<(), String> {
    let piece = piece_path(&part);
    // A partial piece from an interrupted run is never trusted.
    remove_if_present(&piece)?;

    let expected_len = range.len();
    let mut have = match std::fs::metadata(&part) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(format!("download: reading {}: {e}", part.display())),
    };
    if have > expected_len {
        return Err(format!(
            "download: {} holds {have} bytes but its range is {expected_len}; delete it to refetch",
            part.display()
        ));
    }
    let torn = if have == expected_len { 0 } else { have % cfg.piece_bytes };
    if torn != 0 {
        // An append that died mid-write: keep the complete pieces only.
        have -= torn;
        tracing::warn!("download: {} has a torn tail of {torn} bytes, resuming from {have}", part.display());
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&part)
            .map_err(|e| format!("download: opening {}: {e}", part.display()))?;
        file.set_len(have).map_err(|e| format!("download: truncating {}: {e}", part.display()))?;
    }
    reporter.add(have);

    let mut pos = range.start + have;
    while pos <= range.end {
        let end = range.end.min(pos + cfg.piece_bytes - 1);
        fetch_piece_with_retries(cfg, client, url, pos, end, &piece, reporter).await?;
        let (piece_owned, part_owned) = (piece.clone(), part.clone());
        tokio::task::spawn_blocking(move || append_piece(&piece_owned, &part_owned))
            .await
            .map_err(|e| format!("download: append task: {e}"))??;
        pos = end + 1;
    }
    Ok(())
}

fn append_piece(piece: &Path, part: &Path) -> Result<(), String> {
    let mut input = std::fs::File::open(piece).map_err(|e| format!("download: opening {}: {e}", piece.display()))?;
    let mut output = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(part)
        .map_err(|e| format!("download: opening {}: {e}", part.display()))?;
    std::io::copy(&mut input, &mut output).map_err(|e| format!("download: appending to {}: {e}", part.display()))?;
    output.flush().map_err(|e| format!("download: appending to {}: {e}", part.display()))?;
    drop(input);
    std::fs::remove_file(piece).map_err(|e| format!("download: removing {}: {e}", piece.display()))
}

fn remove_if_present(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("download: removing {}: {e}", path.display())),
    }
}

enum PieceError {
    /// Transport trouble: a fresh connection may succeed.
    Retry(String),
    /// The server answered wrongly: no retry can fix it.
    Fatal(String),
}

async fn fetch_piece_with_retries<P: Fn(u64, u64) + Send + Sync>(
    cfg: &Config,
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    piece: &Path,
    reporter: &Reporter<P>,
) -> Result<(), String> {
    let mut backoff = cfg.retry_backoff_first;
    for attempt in 1..=cfg.max_piece_attempts {
        let mut received = 0;
        match fetch_piece(cfg, client, url, start, end, piece, reporter, &mut received).await {
            Ok(()) => return Ok(()),
            Err(PieceError::Fatal(msg)) => {
                reporter.discard(received);
                return Err(msg);
            }
            Err(PieceError::Retry(msg)) => {
                reporter.discard(received);
                if attempt == cfg.max_piece_attempts {
                    return Err(format!("download: bytes {start}-{end} failed {attempt} times, last: {msg}"));
                }
                tracing::warn!("download: bytes {start}-{end} attempt {attempt}: {msg}; retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(cfg.retry_backoff_max);
            }
        }
    }
    Err(format!("download: bytes {start}-{end}: no attempts configured"))
}

/// One attempt at one piece: a ranged GET against the original url streamed
/// into the `.piece` file, aborted as a stall when the trailing window
/// delivers less than the minimum rate. `received` reports the bytes counted
/// into the reporter so far, so a failed attempt can be un-counted.
#[allow(clippy::too_many_arguments)]
async fn fetch_piece<P: Fn(u64, u64) + Send + Sync>(
    cfg: &Config,
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    piece: &Path,
    reporter: &Reporter<P>,
    received: &mut u64,
) -> Result<(), PieceError> {
    let expected = end - start + 1;
    let response = client
        .get(url)
        .header(RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|e| PieceError::Retry(format!("request: {e}")))?;
    let status = response.status();
    if status == StatusCode::OK {
        return Err(PieceError::Fatal(format!("download: {url} ignored Range bytes={start}-{end} (answered 200)")));
    }
    if status != StatusCode::PARTIAL_CONTENT {
        let retryable = status.is_server_error()
            || status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::TOO_MANY_REQUESTS;
        return Err(if retryable {
            PieceError::Retry(format!("HTTP {status}"))
        } else {
            PieceError::Fatal(format!("download: {url} bytes={start}-{end}: HTTP {status}"))
        });
    }
    if let Some(len) = response.content_length() {
        if len != expected {
            return Err(PieceError::Fatal(format!(
                "download: {url} answered bytes={start}-{end} with {len} bytes instead of {expected}"
            )));
        }
    }

    let mut file =
        std::fs::File::create(piece).map_err(|e| PieceError::Fatal(format!("download: creating {}: {e}", piece.display())))?;
    let mut stream = response.bytes_stream();
    let stall_floor = cfg.stall_min_bytes_per_sec * cfg.stall_window.as_secs();
    let mut window_start = Instant::now();
    let mut window_bytes = 0u64;
    loop {
        let remaining = cfg.stall_window.saturating_sub(window_start.elapsed());
        match tokio::time::timeout(remaining, stream.next()).await {
            Err(_elapsed) => {}
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(PieceError::Retry(format!("connection dropped after {received} bytes: {e}"))),
            Ok(Some(Ok(chunk))) => {
                file.write_all(&chunk)
                    .map_err(|e| PieceError::Fatal(format!("download: writing {}: {e}", piece.display())))?;
                let n = chunk.len() as u64;
                *received += n;
                window_bytes += n;
                reporter.add(n);
                if *received > expected {
                    return Err(PieceError::Fatal(format!(
                        "download: {url} sent more than the {expected} bytes asked for (bytes={start}-{end})"
                    )));
                }
            }
        }
        if window_start.elapsed() >= cfg.stall_window {
            if window_bytes < stall_floor {
                return Err(PieceError::Retry(format!(
                    "stalled: {window_bytes} bytes in {:?} (floor {stall_floor})",
                    cfg.stall_window
                )));
            }
            window_start = Instant::now();
            window_bytes = 0;
        }
    }
    file.flush().map_err(|e| PieceError::Fatal(format!("download: writing {}: {e}", piece.display())))?;
    drop(file);
    if *received != expected {
        return Err(PieceError::Retry(format!("connection closed after {received} of {expected} bytes")));
    }
    Ok(())
}

/// Streams the parts into `dest.tmp` while hashing, checks every size and the
/// digest, then renames over `dest`. Any mismatch removes the joined file AND
/// the parts: bytes that hash wrong can never become the right file.
fn join_and_verify(dest: &Path, parts: &[(PathBuf, u64)], total: u64, expected_hash: &str) -> Result<(), String> {
    let tmp = tmp_path(dest);
    let result = join_into(&tmp, parts, total, expected_hash);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        for (part, _) in parts {
            let _ = std::fs::remove_file(part);
        }
        return Err(e);
    }
    std::fs::rename(&tmp, dest).map_err(|e| format!("download: placing {}: {e}", dest.display()))?;
    for (part, _) in parts {
        std::fs::remove_file(part).map_err(|e| format!("download: removing {}: {e}", part.display()))?;
    }
    Ok(())
}

fn join_into(tmp: &Path, parts: &[(PathBuf, u64)], total: u64, expected_hash: &str) -> Result<(), String> {
    let mut output = std::io::BufWriter::new(
        std::fs::File::create(tmp).map_err(|e| format!("download: creating {}: {e}", tmp.display()))?,
    );
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let mut buf = vec![0u8; JOIN_BUFFER_BYTES];
    for (part, expected_len) in parts {
        let len = std::fs::metadata(part).map_err(|e| format!("download: reading {}: {e}", part.display()))?.len();
        if len != *expected_len {
            return Err(format!("download: size mismatch: {} is {len} bytes, expected {expected_len}", part.display()));
        }
        let mut input = std::fs::File::open(part).map_err(|e| format!("download: opening {}: {e}", part.display()))?;
        loop {
            let n = input.read(&mut buf).map_err(|e| format!("download: reading {}: {e}", part.display()))?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n]).map_err(|e| format!("download: writing {}: {e}", tmp.display()))?;
            hasher.update(&buf[..n]);
            written += n as u64;
        }
    }
    output.flush().map_err(|e| format!("download: writing {}: {e}", tmp.display()))?;
    drop(output);
    if written != total {
        return Err(format!("download: size mismatch: joined {written} bytes, expected {total}"));
    }
    let actual = hex(&hasher.finalize());
    if actual != expected_hash {
        return Err(format!("download: sha256 mismatch: got {actual}, expected {expected_hash}"));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use axum::Router;

    const TEST_BODY_BYTES: usize = 3 * 1024 * 1024 + 12_345;
    const TEST_PIECE_BYTES: u64 = 256 * 1024;

    fn test_config() -> Config {
        Config {
            parts: 4,
            piece_bytes: TEST_PIECE_BYTES,
            stall_min_bytes_per_sec: 1,
            stall_window: Duration::from_secs(5),
            max_piece_attempts: 3,
            retry_backoff_first: Duration::from_millis(10),
            retry_backoff_max: Duration::from_millis(20),
        }
    }

    /// Deterministic pseudo-random bytes (xorshift64), no rand dependency.
    fn random_body(len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
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
        hex(&Sha256::digest(bytes))
    }

    struct Served {
        body: Vec<u8>,
        honor_range: bool,
        /// Every `(start, end)` a client asked for, in arrival order.
        ranges: Mutex<Vec<(u64, u64)>>,
    }

    async fn serve(State(s): State<Arc<Served>>, headers: HeaderMap) -> Response {
        let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).filter(|_| s.honor_range);
        let Some(range) = range else {
            return (StatusCode::OK, s.body.clone()).into_response();
        };
        let (start, end) = range
            .strip_prefix("bytes=")
            .and_then(|r| r.split_once('-'))
            .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
            .expect("test client sends bytes=a-b");
        s.ranges.lock().unwrap().push((start, end));
        let slice = s.body[start as usize..=end as usize].to_vec();
        (
            StatusCode::PARTIAL_CONTENT,
            [(header::CONTENT_RANGE, format!("bytes {start}-{end}/{}", s.body.len()))],
            slice,
        )
            .into_response()
    }

    /// Starts the range-capable test server on a free port; returns the file url.
    async fn start_server(served: Arc<Served>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/file.bin", get(serve)).with_state(served);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}/file.bin")
    }

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rillio-download-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn full_fetch_is_byte_identical_with_the_right_hash() {
        let body = random_body(TEST_BODY_BYTES);
        let hash = sha256_of(&body);
        let served = Arc::new(Served { body: body.clone(), honor_range: true, ranges: Mutex::new(Vec::new()) });
        let url = start_server(served.clone()).await;
        let dir = fresh_dir("full");
        let dest = dir.join("file.bin");
        let reports: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = reports.clone();

        fetch_with(&test_config(), &url, &dest, body.len() as u64, &hash, move |done, total| {
            sink.lock().unwrap().push((done, total));
        })
        .await
        .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(leftovers(&dir), vec!["file.bin"], "parts and tmp are cleaned up");
        let reports = reports.lock().unwrap();
        assert_eq!(reports.last(), Some(&(body.len() as u64, body.len() as u64)));
        assert!(reports.windows(2).all(|w| w[0].0 <= w[1].0), "progress never goes backwards: {reports:?}");
        // Every piece was its own request, none larger than a piece.
        let ranges = served.ranges.lock().unwrap();
        assert!(ranges.iter().skip(1).all(|(a, b)| b - a < TEST_PIECE_BYTES), "{ranges:?}");
        assert_eq!(ranges[0], (0, 0), "the size probe comes first");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_keeps_complete_pieces_and_discards_partial_ones() {
        let body = random_body(TEST_BODY_BYTES);
        let hash = sha256_of(&body);
        let served = Arc::new(Served { body: body.clone(), honor_range: true, ranges: Mutex::new(Vec::new()) });
        let url = start_server(served.clone()).await;
        let dir = fresh_dir("resume");
        let dest = dir.join("file.bin");
        let cfg = test_config();
        let ranges = part_ranges(body.len() as u64, cfg.parts);
        let part0 = ranges[0];
        let part1 = ranges[1];

        // A previous run finished part 0, completed one piece of part 1, died
        // mid-append on its second piece (torn tail) and left its .piece file.
        std::fs::write(part_path(&dest, 0), &body[part0.start as usize..=part0.end as usize]).unwrap();
        let torn_tail = 1000usize;
        let kept = TEST_PIECE_BYTES as usize;
        std::fs::write(part_path(&dest, 1), &body[part1.start as usize..part1.start as usize + kept + torn_tail]).unwrap();
        std::fs::write(piece_path(&part_path(&dest, 1)), vec![0xAB; 4321]).unwrap();

        fetch_with(&cfg, &url, &dest, body.len() as u64, &hash, |_, _| {}).await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(leftovers(&dir), vec!["file.bin"]);
        let ranges = served.ranges.lock().unwrap();
        let requested: Vec<(u64, u64)> = ranges.iter().copied().filter(|r| *r != (0, 0)).collect();
        assert!(requested.iter().all(|(a, _)| *a > part0.end), "part 0 was refetched: {requested:?}");
        let part1_first = requested.iter().filter(|(a, _)| *a >= part1.start && *a <= part1.end).map(|(a, _)| *a).min();
        assert_eq!(part1_first, Some(part1.start + TEST_PIECE_BYTES), "part 1 resumes after its complete piece");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_sha256_fails_and_leaves_nothing() {
        let body = random_body(TEST_BODY_BYTES);
        let served = Arc::new(Served { body: body.clone(), honor_range: true, ranges: Mutex::new(Vec::new()) });
        let url = start_server(served).await;
        let dir = fresh_dir("badhash");
        let dest = dir.join("file.bin");
        let wrong = "0".repeat(SHA256_HEX_LEN);

        let err = fetch_with(&test_config(), &url, &dest, body.len() as u64, &wrong, |_, _| {}).await.unwrap_err();

        assert!(err.contains("sha256 mismatch"), "{err}");
        assert!(err.contains(&sha256_of(&body)), "says which hash it got: {err}");
        assert!(leftovers(&dir).is_empty(), "no dest, tmp or parts: {:?}", leftovers(&dir));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_ignoring_range_fails_loudly() {
        let body = random_body(TEST_BODY_BYTES);
        let hash = sha256_of(&body);
        let served = Arc::new(Served { body: body.clone(), honor_range: false, ranges: Mutex::new(Vec::new()) });
        let url = start_server(served).await;
        let dir = fresh_dir("norange");
        let dest = dir.join("file.bin");

        let err = fetch_with(&test_config(), &url, &dest, body.len() as u64, &hash, |_, _| {}).await.unwrap_err();

        assert!(err.contains("ignores Range"), "{err}");
        assert!(leftovers(&dir).is_empty(), "{:?}", leftovers(&dir));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn wrong_expected_size_fails_before_downloading() {
        let body = random_body(TEST_BODY_BYTES);
        let hash = sha256_of(&body);
        let served = Arc::new(Served { body: body.clone(), honor_range: true, ranges: Mutex::new(Vec::new()) });
        let url = start_server(served.clone()).await;
        let dir = fresh_dir("badsize");
        let dest = dir.join("file.bin");

        let err = fetch_with(&test_config(), &url, &dest, body.len() as u64 + 1, &hash, |_, _| {}).await.unwrap_err();

        assert!(err.contains("server reports"), "{err}");
        assert_eq!(served.ranges.lock().unwrap().len(), 1, "only the probe was sent");
        assert!(leftovers(&dir).is_empty());
    }

    #[test]
    fn part_ranges_cover_the_file_exactly_once() {
        for (size, parts) in [(1u64, 4usize), (2, 4), (7, 4), (100, 3), (TEST_BODY_BYTES as u64, 4)] {
            let ranges = part_ranges(size, parts);
            assert!(ranges.len() <= parts);
            assert_eq!(ranges[0].start, 0);
            assert_eq!(ranges.last().unwrap().end, size - 1);
            for w in ranges.windows(2) {
                assert_eq!(w[0].end + 1, w[1].start, "{size}/{parts}: {ranges:?}");
            }
            assert_eq!(ranges.iter().map(|r| r.len()).sum::<u64>(), size);
        }
    }
}
