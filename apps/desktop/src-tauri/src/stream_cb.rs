//! S2 - the `rillio://` byte plane: mpv reads torrent bytes IN-PROCESS.
//!
//! Instead of `http://127.0.0.1:<port>/<info_hash>/<idx>`, the player loads
//! `rillio://<info_hash>/<idx>` and libmpv pulls the bytes through
//! `mpv_stream_cb_add_ro` callbacks that sit directly on top of the streaming
//! server's [`Engine`] (librqbit `FileStream`). No socket, no HTTP framing, no
//! loopback origin surface.
//!
//! Three pieces live here:
//!  1. the url grammar ([`format_url`] / [`parse_url`]) - deliberately STRICT,
//!     because widening what a player may open is a security decision;
//!  2. the generic callback plumbing ([`register`], [`ByteSource`]) - the raw
//!     FFI is in [`crate::mpv`], everything unsafe about lifetimes is here;
//!  3. the [`Engine`] bridge ([`EngineSources`]) - `get_or_create` + `touch` +
//!     tail prefetch, then a `FileStream` driven with `Handle::block_on`.
//!
//! THREADING RULE. mpv calls these callbacks synchronously from its demuxer /
//! stream threads, which are NOT tokio workers, and blocking in them is both
//! allowed and expected. That is what makes `Handle::block_on` legal here.
//! Calling it from a tokio worker instead would deadlock, so every entry point
//! asserts it is off-runtime and fails loud rather than hanging.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rillio_streaming_server::engine::{Engine, Handle};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::mpv::{
    Mpv, MpvStreamCbInfo, MPV_ERROR_GENERIC, MPV_ERROR_LOADING_FAILED, MPV_ERROR_UNSUPPORTED,
};

/// The scheme, without `://`. Registered with mpv under exactly this name.
pub const SCHEME: &str = "rillio";

/// Tail bytes warmed ahead of the front read, mirroring the HTTP path
/// (`crates/streaming-server/src/stream.rs`). 8 MiB fits inside librqbit's
/// 32 MiB per-stream priority window and covers the Cues (the MKV seek index,
/// usually at EOF) that mpv reaches for right after open.
const PREFETCH_TAIL_BYTES: u64 = 8 * 1024 * 1024;

/// Cap on one tail prefetch, so a stalled swarm cannot leak the task forever.
const PREFETCH_TIMEOUT: Duration = Duration::from_secs(300);

// A torrent ABSENT from the session at open time fails the load IMMEDIATELY
// (no add, no timeout). mpv cannot cancel `open_fn` - `cancel_fn` is installed
// only after open returns (see `open_cb`) - so any wait here holds mpv's
// stream thread hostage: `stop`, a replacing `loadfile` and
// `mpv_terminate_destroy` all queue behind it. Absence means the `/create`
// POST either was skipped (a stream with infoHash + finite fileIdx
// short-circuits createTorrent.js) or has not landed yet (observed live: a
// cold magnet's create raced the loadfile). Either way the right move is the
// same: error now, let ShellVideo's one-shot HTTP retry take over - the HTTP
// route adds the torrent and waits for metadata CANCELLABLY (mpv can abort a
// socket read at any time), so failing fast costs nothing and an earlier 15s
// bounded wait here was pure dead air before the same fallback.

// ---------------------------------------------------------------------------
// 1. the url grammar
// ---------------------------------------------------------------------------

/// Why a `rillio://` url was refused. Every variant is a hard reject: there is
/// no lenient path, because this grammar is what the player allowlist trusts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Missing or wrong `rillio://` prefix.
    Scheme,
    /// The infohash is not exactly 40 lowercase hex characters.
    InfoHash,
    /// The file index is not a bare decimal `u32` (no sign, no leading zeros).
    FileIndex,
    /// Anything after `<info_hash>/<idx>`: extra path, query, or fragment.
    Trailing,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ParseError::Scheme => "not a rillio:// url",
            ParseError::InfoHash => "info hash must be exactly 40 lowercase hex characters",
            ParseError::FileIndex => "file index must be a bare decimal u32",
            ParseError::Trailing => "trailing content after rillio://<info_hash>/<file_idx>",
        };
        f.write_str(s)
    }
}

/// Build the canonical url. `info_hash` must already be 40 lowercase hex; this
/// does not normalise, so a caller handing it garbage produces a url its own
/// [`parse_url`] rejects (fail loud, not silently rewritten).
pub fn format_url(info_hash: &str, file_idx: u32) -> String {
    format!("{SCHEME}://{info_hash}/{file_idx}")
}

/// Parse `rillio://<40 lowercase hex>/<u32>` and NOTHING else.
///
/// Deliberately stricter than the HTTP route, which also accepts `-1`
/// (guess-the-main-file) and url-encoded filenames: the caller resolves the
/// real index before handing a url to the player, so this grammar stays a
/// closed set of two integers. Uppercase hex, a query string, a trailing
/// `/filename.mkv`, a fragment, leading zeros and `+`/`-` signs are all
/// rejected, so there is exactly one spelling of any given stream.
pub fn parse_url(url: &str) -> Result<(String, u32), ParseError> {
    let rest = url
        .strip_prefix(SCHEME)
        .and_then(|r| r.strip_prefix("://"))
        .ok_or(ParseError::Scheme)?;
    let (info_hash, idx) = rest.split_once('/').ok_or(ParseError::Trailing)?;

    if info_hash.len() != 40
        || !info_hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ParseError::InfoHash);
    }
    // No second segment, no query, no fragment.
    if idx.contains(['/', '?', '#']) {
        return Err(ParseError::Trailing);
    }
    if idx.is_empty()
        || idx.len() > 10
        || !idx.bytes().all(|b| b.is_ascii_digit())
        || (idx.len() > 1 && idx.starts_with('0'))
    {
        return Err(ParseError::FileIndex);
    }
    let file_idx: u32 = idx.parse().map_err(|_| ParseError::FileIndex)?;
    Ok((info_hash.to_owned(), file_idx))
}

// ---------------------------------------------------------------------------
// 2. the callback plumbing
// ---------------------------------------------------------------------------

/// One open stream, as mpv sees it. `read`/`seek`/`size` are called from a
/// single mpv stream thread and never concurrently with each other; only
/// [`canceller`](ByteSource::canceller) is touched from another thread.
pub trait ByteSource: Send {
    /// Total length in bytes, or `None` when unknown (mpv gets
    /// `MPV_ERROR_UNSUPPORTED` and treats the stream as unbounded).
    fn size(&mut self) -> Option<u64>;

    /// Fill `buf`; `Ok(0)` means EOF. Blocking is expected.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, String>;

    /// Seek to an absolute offset, returning the new position. `None` means the
    /// stream is not seekable (mpv gets `MPV_ERROR_UNSUPPORTED`).
    fn seek(&mut self, pos: u64) -> Result<u64, String>;

    /// A handle mpv may trip from another thread to unblock a parked read.
    /// `None` means "not cancellable" and mpv is told so (NULL `cancel_fn`).
    fn canceller(&self) -> Option<Arc<CancelFlag>> {
        None
    }
}

/// Opens a stream for one `(info_hash, file_idx)` pair. Lives for as long as
/// the mpv instance it was registered with.
pub trait SourceFactory: Send + Sync + 'static {
    fn open(&self, info_hash: &str, file_idx: u32) -> Result<Box<dyn ByteSource>, String>;
}

impl<F> SourceFactory for F
where
    F: Fn(&str, u32) -> Result<Box<dyn ByteSource>, String> + Send + Sync + 'static,
{
    fn open(&self, info_hash: &str, file_idx: u32) -> Result<Box<dyn ByteSource>, String> {
        self(info_hash, file_idx)
    }
}

/// Sticky cancellation shared between a blocked read and mpv's `cancel_fn`.
/// Sticky on purpose: mpv cancels a stream it is about to close, so a late
/// read must not sneak past a cancel that arrived while it was starting.
#[derive(Default)]
pub struct CancelFlag {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    /// Trip the flag and wake a parked read. Safe from any thread.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        // A permit for a waiter that has not registered yet.
        self.notify.notify_one();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves once [`cancel`](Self::cancel) has been called (immediately if it
    /// already has).
    async fn cancelled(&self) {
        loop {
            let waiter = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            waiter.await;
            if self.is_cancelled() {
                return;
            }
        }
    }
}

/// Opens a stream for a full url of one registered scheme. Each scheme owns its
/// own grammar (`rillio://` is [`parse_url`]; the dub track has its own), so
/// the plumbing below never interprets urls itself.
pub trait UrlOpener: Send + Sync + 'static {
    fn open_url(&self, url: &str) -> Result<Box<dyn ByteSource>, String>;
}

impl<F> UrlOpener for F
where
    F: Fn(&str) -> Result<Box<dyn ByteSource>, String> + Send + Sync + 'static,
{
    fn open_url(&self, url: &str) -> Result<Box<dyn ByteSource>, String> {
        self(url)
    }
}

/// The `user_data` mpv keeps for one registered protocol.
struct Registration {
    scheme: &'static str,
    opener: Box<dyn UrlOpener>,
}

/// The per-stream cookie. `source` is a SEPARATE allocation reached through a
/// raw pointer, so `read`/`seek`/`size` can take `&mut` to it while `cancel_fn`
/// concurrently takes `&` to `cancel` without the two ever aliasing the same
/// object.
struct Cookie {
    scheme: &'static str,
    source: *mut Box<dyn ByteSource>,
    cancel: Option<Arc<CancelFlag>>,
}

/// Register [`SCHEME`] on `mpv`, served by `factory` after the strict
/// `rillio://` grammar has been applied.
///
/// Call BEFORE `mpv.initialize()`. Returns `Err` when the DLL has no
/// `mpv_stream_cb_add_ro` or its client API predates the ABI - the documented
/// fallback is then to keep loading the HTTP stream url.
pub fn register<S: SourceFactory>(mpv: &Mpv, factory: S) -> Result<(), String> {
    register_protocol(mpv, SCHEME, move |url: &str| {
        let (info_hash, file_idx) = parse_url(url).map_err(|e| format!("refusing {url:?}: {e}"))?;
        factory.open(&info_hash, file_idx)
    })
}

/// Register any custom `scheme` on `mpv`, served by `opener`, which receives
/// the full url and applies its own grammar. Same call-before-initialize rule
/// and the same `Err` meaning as [`register`].
pub fn register_protocol<O: UrlOpener>(mpv: &Mpv, scheme: &'static str, opener: O) -> Result<(), String> {
    let reg = Box::into_raw(Box::new(Registration { scheme, opener: Box::new(opener) }));
    // SAFETY: `reg` is a live Box leak; on success mpv takes ownership and
    // `free_registration` runs after mpv_terminate_destroy. On failure we take
    // it back and drop it here, so neither path leaks.
    let res = unsafe { mpv.add_stream_protocol(scheme, reg as *mut c_void, open_cb, free_registration) };
    if res.is_err() {
        drop(unsafe { Box::from_raw(reg) });
    }
    res
}

unsafe extern "C" fn free_registration(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    drop(Box::from_raw(ptr as *mut Registration));
}

/// `mpv_stream_cb_open_ro_fn`. `uri` is the full url including the scheme.
unsafe extern "C" fn open_cb(
    user_data: *mut c_void,
    uri: *mut c_char,
    info: *mut MpvStreamCbInfo,
) -> c_int {
    // A panic must never unwind into mpv's C frames.
    let result = catch_unwind(AssertUnwindSafe(|| {
        if user_data.is_null() || uri.is_null() || info.is_null() {
            tracing::error!("rillio://: open called with a null argument");
            return MPV_ERROR_LOADING_FAILED;
        }
        let reg = &*(user_data as *const Registration);
        let scheme = reg.scheme;
        let url = match CStr::from_ptr(uri).to_str() {
            Ok(s) => s,
            Err(_) => {
                tracing::error!("{scheme}://: url is not valid utf-8");
                return MPV_ERROR_LOADING_FAILED;
            }
        };
        let source = match reg.opener.open_url(url) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("{scheme}://: open failed for {url:?}: {e}");
                return MPV_ERROR_LOADING_FAILED;
            }
        };
        let cancel = source.canceller();
        let cookie = Box::into_raw(Box::new(Cookie {
            scheme,
            source: Box::into_raw(Box::new(source)),
            cancel: cancel.clone(),
        }));
        (*info).cookie = cookie as *mut c_void;
        (*info).read_fn = Some(read_cb);
        (*info).seek_fn = Some(seek_cb);
        (*info).size_fn = Some(size_cb);
        (*info).close_fn = Some(close_cb);
        (*info).cancel_fn = if cancel.is_some() { Some(cancel_cb) } else { None };
        tracing::debug!("{scheme}://: stream opened for {url:?}");
        0
    }));
    result.unwrap_or_else(|_| {
        tracing::error!("rillio://: panic in open callback");
        MPV_ERROR_LOADING_FAILED
    })
}

unsafe extern "C" fn read_cb(cookie: *mut c_void, buf: *mut c_char, nbytes: u64) -> i64 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if cookie.is_null() || buf.is_null() {
            return MPV_ERROR_GENERIC;
        }
        let c = &*(cookie as *const Cookie);
        let n = nbytes.min(isize::MAX as u64) as usize;
        if n == 0 {
            return 0;
        }
        let slice = std::slice::from_raw_parts_mut(buf as *mut u8, n);
        match (*c.source).read(slice) {
            Ok(read) => read as i64,
            Err(e) => {
                tracing::error!("{}://: read failed: {e}", c.scheme);
                MPV_ERROR_GENERIC
            }
        }
    }));
    result.unwrap_or_else(|_| {
        tracing::error!("rillio://: panic in read callback");
        MPV_ERROR_GENERIC
    })
}

unsafe extern "C" fn seek_cb(cookie: *mut c_void, offset: i64) -> i64 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if cookie.is_null() || offset < 0 {
            return MPV_ERROR_GENERIC;
        }
        let c = &*(cookie as *const Cookie);
        match (*c.source).seek(offset as u64) {
            Ok(pos) => pos as i64,
            Err(e) => {
                tracing::error!("{}://: seek to {offset} failed: {e}", c.scheme);
                MPV_ERROR_UNSUPPORTED
            }
        }
    }));
    result.unwrap_or_else(|_| {
        tracing::error!("rillio://: panic in seek callback");
        MPV_ERROR_GENERIC
    })
}

unsafe extern "C" fn size_cb(cookie: *mut c_void) -> i64 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if cookie.is_null() {
            return MPV_ERROR_UNSUPPORTED;
        }
        let c = &*(cookie as *const Cookie);
        match (*c.source).size() {
            Some(len) => len.min(i64::MAX as u64) as i64,
            None => MPV_ERROR_UNSUPPORTED,
        }
    }));
    result.unwrap_or_else(|_| {
        tracing::error!("rillio://: panic in size callback");
        MPV_ERROR_UNSUPPORTED
    })
}

/// Called from ANOTHER thread while a read may be parked. Only touches
/// `Cookie::cancel`, never `Cookie::source`.
unsafe extern "C" fn cancel_cb(cookie: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if cookie.is_null() {
            return;
        }
        let c = &*(cookie as *const Cookie);
        if let Some(flag) = &c.cancel {
            flag.cancel();
        }
    }));
}

/// mpv closes a stream only after its stream thread has left `read`/`seek`
/// (it trips `cancel_fn` first and waits), so taking ownership of the source
/// here never races the `&mut` those callbacks hold. The extra `cancel` below
/// is belt-and-braces for a cancel mpv skipped.
unsafe extern "C" fn close_cb(cookie: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if cookie.is_null() {
            return;
        }
        let c = Box::from_raw(cookie as *mut Cookie);
        // Unblock anything still parked before the source is dropped.
        if let Some(flag) = &c.cancel {
            flag.cancel();
        }
        drop(Box::from_raw(c.source));
        tracing::debug!("{}://: stream closed", c.scheme);
    }));
}

// ---------------------------------------------------------------------------
// 3. the Engine bridge
// ---------------------------------------------------------------------------

/// Serves `rillio://` out of a live streaming-server [`Engine`].
///
/// `runtime` is captured at registration time and is the ONLY way these
/// callbacks reach async code: they run on mpv threads, so `block_on` parks the
/// mpv thread (correct) instead of a tokio worker (deadlock).
pub struct EngineSources {
    engine: Engine,
    runtime: tokio::runtime::Handle,
}

impl EngineSources {
    pub fn new(engine: Engine, runtime: tokio::runtime::Handle) -> Self {
        Self { engine, runtime }
    }
}

/// Refuse to `block_on` from inside a tokio worker: that is the documented
/// deadlock, and a silent hang is the worst possible failure mode here.
fn assert_off_runtime(what: &str) -> Result<(), String> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(format!(
            "rillio:// {what} was called from a tokio runtime thread; \
             stream_cb callbacks must run on mpv threads only"
        ));
    }
    Ok(())
}

impl SourceFactory for EngineSources {
    fn open(&self, info_hash: &str, file_idx: u32) -> Result<Box<dyn ByteSource>, String> {
        assert_off_runtime("open")?;
        let engine = self.engine.clone();
        let idx = file_idx as usize;
        let ih = info_hash.to_owned();
        let runtime = self.runtime.clone();
        // A torrent already in the session makes the get_or_create below a
        // cheap lookup, so the open may take as long as the bytes need (reads
        // parking on unverified pieces is by design, and cancel_fn covers them
        // once open has returned). An ABSENT torrent fails the load right here:
        // see the absence note above the parse grammar.
        if self.engine.get(info_hash).is_none() {
            return Err(format!(
                "{info_hash}: not in the session; failing the load immediately \
                 so mpv's stream thread is never held (no cancel exists during \
                 open) - the web side retries over the HTTP stream url"
            ));
        }

        self.runtime.block_on(async move {
            let open = async {
                // Idempotent get-or-create; never re-adds a live torrent (that
                // would reset it to `initializing` and fail concurrent reads).
                let handle = engine
                    .get_or_create(&ih)
                    .await
                    .map_err(|e| format!("get_or_create({ih}) failed: {e:#}"))?;
                // Mark active so the cache sweeper never evicts what is playing.
                engine.touch(&ih);

                let files = Engine::files(&handle);
                if files.is_empty() {
                    return Err(format!("{ih}: metadata not resolved (no files)"));
                }
                let file = files
                    .get(idx)
                    .ok_or_else(|| format!("{ih}: file index {idx} out of range ({})", files.len()))?;
                let len = file.length;

                // Tail-prefetch parity with the HTTP route: warm the Cues once per
                // file so mpv's opening tail seek does not race the front read.
                spawn_tail_prefetch(&runtime, &engine, &handle, &ih, idx, len);

                let stream = Arc::clone(&handle)
                    .stream(idx)
                    .map_err(|e| format!("{ih}: opening file {idx} failed: {e:#}"))?;

                Ok(Box::new(TorrentSource {
                    runtime: runtime.clone(),
                    stream,
                    len,
                    pos: 0,
                    cancel: Arc::new(CancelFlag::new()),
                    label: format!("{ih}/{idx}"),
                }) as Box<dyn ByteSource>)
            };
            open.await
        })
    }
}

/// A librqbit `FileStream` driven synchronously from an mpv thread.
///
/// Generic over the stream type so this module never has to name librqbit
/// (the desktop crate depends on the streaming server, not on librqbit).
struct TorrentSource<S> {
    runtime: tokio::runtime::Handle,
    stream: S,
    len: u64,
    pos: u64,
    cancel: Arc<CancelFlag>,
    label: String,
}

impl<S> ByteSource for TorrentSource<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncSeek + Send + Unpin,
{
    fn size(&mut self) -> Option<u64> {
        Some(self.len)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        assert_off_runtime("read")?;
        if self.cancel.is_cancelled() {
            return Err(format!("{}: read cancelled", self.label));
        }
        let cancel = Arc::clone(&self.cancel);
        let stream = &mut self.stream;
        let label = &self.label;
        let n = self.runtime.block_on(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => Err(format!("{label}: read cancelled")),
                r = stream.read(buf) => r.map_err(|e| format!("{label}: read failed: {e}")),
            }
        })?;
        self.pos += n as u64;
        Ok(n)
    }

    fn seek(&mut self, pos: u64) -> Result<u64, String> {
        assert_off_runtime("seek")?;
        if pos > self.len {
            return Err(format!("{}: seek past EOF ({pos} > {})", self.label, self.len));
        }
        let cancel = Arc::clone(&self.cancel);
        let stream = &mut self.stream;
        let label = &self.label;
        // AsyncSeek on a FileStream only repositions (and re-prioritises the
        // swarm); it does not wait for data, so this returns promptly.
        let at = self.runtime.block_on(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => Err(format!("{label}: seek cancelled")),
                r = stream.seek(std::io::SeekFrom::Start(pos)) =>
                    r.map_err(|e| format!("{label}: seek to {pos} failed: {e}")),
            }
        })?;
        self.pos = at;
        Ok(at)
    }

    fn canceller(&self) -> Option<Arc<CancelFlag>> {
        Some(Arc::clone(&self.cancel))
    }
}

/// Best-effort Cues warm-up, mirroring `stream.rs::spawn_tail_prefetch`.
///
/// The HTTP path's helper is private to the streaming-server crate, so this is
/// the same logic expressed over its public API: `Engine::mark_prefetch` is the
/// shared dedup, so a title first opened over HTTP and then over `rillio://`
/// (or the reverse) still warms its tail exactly once.
fn spawn_tail_prefetch(
    runtime: &tokio::runtime::Handle,
    engine: &Engine,
    handle: &Handle,
    info_hash: &str,
    file_id: usize,
    file_len: u64,
) {
    // Small files: the tail is inside the front read's lookahead anyway.
    if file_len <= PREFETCH_TAIL_BYTES * 2 {
        return;
    }
    if !engine.mark_prefetch(info_hash, file_id) {
        return;
    }
    let handle = Arc::clone(handle);
    let info_hash = info_hash.to_owned();
    let start = file_len - PREFETCH_TAIL_BYTES;
    runtime.spawn(async move {
        let warm = async move {
            let mut fs = handle.stream(file_id).map_err(|e| format!("{e:#}"))?;
            fs.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| e.to_string())?;
            let mut buf = vec![0u8; 256 * 1024];
            let mut read: u64 = 0;
            while read < PREFETCH_TAIL_BYTES {
                let n = fs.read(&mut buf).await.map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                read += n as u64;
            }
            Ok::<u64, String>(read)
        };
        match tokio::time::timeout(PREFETCH_TIMEOUT, warm).await {
            Ok(Ok(read)) => {
                tracing::debug!("tail-prefetch {info_hash}#{file_id}: warmed {read} tail bytes")
            }
            Ok(Err(e)) => tracing::debug!("tail-prefetch {info_hash}#{file_id}: failed: {e}"),
            Err(_) => tracing::warn!(
                "tail-prefetch {info_hash}#{file_id}: timed out after {PREFETCH_TIMEOUT:?}"
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    // -----------------------------------------------------------------------
    // url grammar
    // -----------------------------------------------------------------------

    const IH: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn format_round_trips_through_parse() {
        let url = format_url(IH, 3);
        assert_eq!(url, format!("rillio://{IH}/3"));
        assert_eq!(parse_url(&url), Ok((IH.to_owned(), 3)));
        assert_eq!(parse_url(&format_url(IH, 0)), Ok((IH.to_owned(), 0)));
        assert_eq!(
            parse_url(&format_url(IH, u32::MAX)),
            Ok((IH.to_owned(), u32::MAX))
        );
    }

    #[test]
    fn rejects_everything_malformed() {
        let cases: &[(&str, ParseError)] = &[
            ("http://127.0.0.1:11470/x/0", ParseError::Scheme),
            ("rillio:/0123456789abcdef0123456789abcdef01234567/0", ParseError::Scheme),
            ("RILLIO://0123456789abcdef0123456789abcdef01234567/0", ParseError::Scheme),
            // uppercase hex
            ("rillio://0123456789ABCDEF0123456789abcdef01234567/0", ParseError::InfoHash),
            // 39 and 41 chars
            ("rillio://0123456789abcdef0123456789abcdef0123456/0", ParseError::InfoHash),
            ("rillio://0123456789abcdef0123456789abcdef012345678/0", ParseError::InfoHash),
            // non-hex
            ("rillio://0123456789abcdefg123456789abcdef01234567/0", ParseError::InfoHash),
            // index shapes
            ("rillio://0123456789abcdef0123456789abcdef01234567/-1", ParseError::FileIndex),
            ("rillio://0123456789abcdef0123456789abcdef01234567/+1", ParseError::FileIndex),
            ("rillio://0123456789abcdef0123456789abcdef01234567/01", ParseError::FileIndex),
            ("rillio://0123456789abcdef0123456789abcdef01234567/", ParseError::FileIndex),
            ("rillio://0123456789abcdef0123456789abcdef01234567/1a", ParseError::FileIndex),
            ("rillio://0123456789abcdef0123456789abcdef01234567/4294967296", ParseError::FileIndex),
            // trailing junk
            ("rillio://0123456789abcdef0123456789abcdef01234567", ParseError::Trailing),
            ("rillio://0123456789abcdef0123456789abcdef01234567/0/x.mkv", ParseError::Trailing),
            ("rillio://0123456789abcdef0123456789abcdef01234567/0?tr=x", ParseError::Trailing),
            ("rillio://0123456789abcdef0123456789abcdef01234567/0#f", ParseError::Trailing),
        ];
        for (url, want) in cases {
            assert_eq!(parse_url(url).err().as_ref(), Some(want), "url {url:?}");
        }
    }

    // -----------------------------------------------------------------------
    // THE SPIKE: headless mpv plays and seeks through the callbacks
    // -----------------------------------------------------------------------

    /// Counters the spike asserts on: proof the bytes really came through our
    /// callbacks and not from some mpv-side shortcut.
    #[derive(Default)]
    struct Counters {
        opens: AtomicU64,
        reads: AtomicU64,
        bytes: AtomicU64,
        seeks: AtomicU64,
        sizes: AtomicU64,
        closes: AtomicU64,
    }

    /// A plain local file served through the SAME callback plumbing the torrent
    /// bridge uses. The mpv side is what the spike is proving; the FileStream
    /// bridge is proven by type-compatibility plus the live E2E later.
    struct FileSource {
        file: std::fs::File,
        len: u64,
        counters: Arc<Counters>,
    }

    impl ByteSource for FileSource {
        fn size(&mut self) -> Option<u64> {
            self.counters.sizes.fetch_add(1, Ordering::SeqCst);
            Some(self.len)
        }
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
            let n = self.file.read(buf).map_err(|e| e.to_string())?;
            self.counters.reads.fetch_add(1, Ordering::SeqCst);
            self.counters.bytes.fetch_add(n as u64, Ordering::SeqCst);
            Ok(n)
        }
        fn seek(&mut self, pos: u64) -> Result<u64, String> {
            let at = self.file.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
            self.counters.seeks.fetch_add(1, Ordering::SeqCst);
            Ok(at)
        }
    }

    impl Drop for FileSource {
        fn drop(&mut self) {
            self.counters.closes.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// 16-bit stereo 44.1 kHz PCM wav of `seconds` of a 440 Hz sine. Written in
    /// pure Rust so the spike needs no media asset on the machine, and linear
    /// PCM means a time seek becomes a genuine BYTE seek through `seek_fn`.
    fn wav_bytes(seconds: u32) -> Vec<u8> {
        const RATE: u32 = 44_100;
        const CHANNELS: u16 = 2;
        const BITS: u16 = 16;
        let block_align = CHANNELS * BITS / 8;
        let frames = RATE * seconds;
        let data_len = frames * block_align as u32;

        let mut out = Vec::with_capacity(data_len as usize + 44);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&CHANNELS.to_le_bytes());
        out.extend_from_slice(&RATE.to_le_bytes());
        out.extend_from_slice(&(RATE * block_align as u32).to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&BITS.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let t = i as f64 / RATE as f64;
            let s = ((t * 440.0 * std::f64::consts::TAU).sin() * 12_000.0) as i16;
            out.extend_from_slice(&s.to_le_bytes());
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    fn dev_dll() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("RILLIO_LIBMPV") {
            let p = PathBuf::from(p);
            if p.exists() {
                return Some(p);
            }
        }
        let p = crate::mpv::default_dll_path();
        p.exists().then_some(p)
    }

    fn prop_f64(mpv: &Mpv, name: &str) -> Option<f64> {
        mpv.get_property_string(name)?.parse().ok()
    }

    /// Poll `name` until `ok` accepts it. Returns the accepted value.
    fn wait_for(mpv: &Mpv, name: &str, timeout: Duration, ok: impl Fn(f64) -> bool) -> Option<f64> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(v) = prop_f64(mpv, name) {
                if ok(v) {
                    return Some(v);
                }
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn spike_headless_mpv_plays_and_seeks_through_stream_cb() {
        let Some(dll) = dev_dll() else {
            eprintln!("[skip] no libmpv found (set RILLIO_LIBMPV)");
            return;
        };

        // 120 s of audio: long enough that a 90 s seek lands far outside any
        // read-ahead window, so mpv MUST come back through seek_fn.
        let wav = wav_bytes(120);
        let path = std::env::temp_dir().join("rillio-stream-cb-spike.wav");
        {
            let mut f = std::fs::File::create(&path).expect("create spike wav");
            f.write_all(&wav).expect("write spike wav");
        }
        let total_len = wav.len() as u64;
        eprintln!("[spike] wav {} bytes at {}", total_len, path.display());

        let counters = Arc::new(Counters::default());
        let mpv = Mpv::load(&dll).expect("load libmpv");
        eprintln!(
            "[spike] client_api_version = {:#x}, supports_stream_cb = {}",
            mpv.client_api_version(),
            mpv.supports_stream_cb()
        );
        assert!(
            mpv.supports_stream_cb(),
            "this libmpv exports no mpv_stream_cb_add_ro; the byte plane cannot be proven with it"
        );

        {
            let counters = Arc::clone(&counters);
            let path = path.clone();
            register(&mpv, move |ih: &str, idx: u32| {
                assert_eq!(ih, IH, "the strict parser must hand back the exact infohash");
                assert_eq!(idx, 0);
                counters.opens.fetch_add(1, Ordering::SeqCst);
                let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
                let len = file.metadata().map_err(|e| e.to_string())?.len();
                Ok(Box::new(FileSource { file, len, counters: Arc::clone(&counters) })
                    as Box<dyn ByteSource>)
            })
            .expect("register rillio:// protocol");
        }

        mpv.set_option("vo", "null").unwrap();
        mpv.set_option("ao", "null").unwrap();
        // No stream cache: force every demuxer seek down to seek_fn instead of
        // being answered out of mpv's own buffer.
        mpv.set_option("cache", "no").unwrap();
        mpv.set_option("demuxer-max-bytes", "1MiB").unwrap();
        mpv.initialize().expect("mpv_initialize");

        let url = format_url(IH, 0);
        mpv.command(&["loadfile", &url]).expect("loadfile");

        let duration = wait_for(&mpv, "duration", Duration::from_secs(15), |d| d > 1.0)
            .unwrap_or_else(|| {
                panic!(
                    "mpv never reported a duration for {url}; \
                     opens={} reads={} bytes={} idle-reason={:?} path={:?}",
                    counters.opens.load(Ordering::SeqCst),
                    counters.reads.load(Ordering::SeqCst),
                    counters.bytes.load(Ordering::SeqCst),
                    mpv.get_property_string("idle-active"),
                    mpv.get_property_string("path"),
                )
            });
        eprintln!("[spike] duration = {duration}");
        assert!(
            (duration - 120.0).abs() < 1.0,
            "duration {duration} should be ~120s, i.e. mpv demuxed OUR bytes"
        );

        let before = wait_for(&mpv, "time-pos", Duration::from_secs(15), |t| t > 0.3)
            .unwrap_or_else(|| {
                panic!(
                    "time-pos never advanced; opens={} reads={} bytes={}",
                    counters.opens.load(Ordering::SeqCst),
                    counters.reads.load(Ordering::SeqCst),
                    counters.bytes.load(Ordering::SeqCst),
                )
            });
        eprintln!("[spike] time-pos before seek = {before}");

        let reads_before_seek = counters.reads.load(Ordering::SeqCst);
        let seeks_before = counters.seeks.load(Ordering::SeqCst);
        mpv.command(&["seek", "90", "absolute"]).expect("seek");

        let after = wait_for(&mpv, "time-pos", Duration::from_secs(20), |t| t >= 89.0)
            .unwrap_or_else(|| {
                panic!(
                    "time-pos never reached 89s after an absolute seek to 90; \
                     last={:?} seeks={} reads={}",
                    mpv.get_property_string("time-pos"),
                    counters.seeks.load(Ordering::SeqCst),
                    counters.reads.load(Ordering::SeqCst),
                )
            });
        eprintln!("[spike] time-pos after seek = {after}");

        // And it keeps PLAYING from there (not just repositioned).
        let advanced = wait_for(&mpv, "time-pos", Duration::from_secs(15), |t| t > after + 0.3)
            .expect("playback did not resume after the seek");
        eprintln!("[spike] time-pos advancing after seek = {advanced}");

        let (opens, reads, bytes, seeks, sizes) = (
            counters.opens.load(Ordering::SeqCst),
            counters.reads.load(Ordering::SeqCst),
            counters.bytes.load(Ordering::SeqCst),
            counters.seeks.load(Ordering::SeqCst),
            counters.sizes.load(Ordering::SeqCst),
        );
        eprintln!(
            "[spike] callbacks: opens={opens} reads={reads} bytes={bytes} seeks={seeks} sizes={sizes}"
        );
        assert_eq!(opens, 1, "open_fn must have run exactly once");
        assert!(reads > reads_before_seek, "reads must continue after the seek");
        assert!(bytes > 0, "no bytes flowed through read_fn");
        assert!(
            seeks > seeks_before,
            "the absolute seek must have reached seek_fn (before={seeks_before}, now={seeks})"
        );
        assert!(sizes > 0, "size_fn was never consulted");
        assert!(bytes < total_len, "mpv read the whole file; the seek proved nothing");

        // mpv_terminate_destroy must run close_fn for the open stream.
        drop(mpv);
        assert_eq!(
            counters.closes.load(Ordering::SeqCst),
            1,
            "close_fn must have released the cookie exactly once"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The strict grammar is the trust boundary: a url mpv routes to us but our
    /// parser refuses must fail the load, and must never reach the factory.
    #[test]
    fn malformed_url_is_refused_before_the_factory_runs() {
        let Some(dll) = dev_dll() else {
            eprintln!("[skip] no libmpv found (set RILLIO_LIBMPV)");
            return;
        };
        let opens = Arc::new(AtomicU64::new(0));
        let mpv = Mpv::load(&dll).expect("load libmpv");
        {
            let opens = Arc::clone(&opens);
            register(&mpv, move |_ih: &str, _idx: u32| {
                opens.fetch_add(1, Ordering::SeqCst);
                Err::<Box<dyn ByteSource>, String>("must never be reached".into())
            })
            .expect("register rillio:// protocol");
        }
        mpv.set_option("vo", "null").unwrap();
        mpv.set_option("ao", "null").unwrap();
        mpv.initialize().expect("mpv_initialize");

        // A trailing path segment: legal on the HTTP route, rejected here.
        let bad = format!("rillio://{IH}/0/some-movie.mkv");
        mpv.command(&["loadfile", &bad]).expect("loadfile");

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut end = None;
        while std::time::Instant::now() < deadline && end.is_none() {
            if let crate::mpv::MpvEvent::EndFile { reason, error } = mpv.wait_event(0.5) {
                end = Some((reason, error));
            }
        }
        let (reason, error) = end.expect("mpv never reported END_FILE for the malformed url");
        eprintln!(
            "[reject] END_FILE reason={reason} error={error} ({})",
            mpv.error_string(error)
        );
        assert_ne!(error, 0, "mpv must have failed the load, not played it");
        assert_eq!(
            opens.load(Ordering::SeqCst),
            0,
            "a malformed url must be rejected by the parser, never handed to the factory"
        );
    }

    /// Registering the same scheme twice must FAIL rather than silently install
    /// a second handler (mpv documents MPV_ERROR_INVALID_PARAMETER).
    #[test]
    fn duplicate_registration_is_refused() {
        let Some(dll) = dev_dll() else {
            eprintln!("[skip] no libmpv found (set RILLIO_LIBMPV)");
            return;
        };
        let mpv = Mpv::load(&dll).expect("load libmpv");
        let factory = || {
            |_ih: &str, _idx: u32| Err::<Box<dyn ByteSource>, String>("unused".into())
        };
        register(&mpv, factory()).expect("first registration");
        let second = register(&mpv, factory());
        eprintln!("[dup] second register -> {second:?}");
        assert!(second.is_err(), "a second registration must not succeed silently");
    }

    // -----------------------------------------------------------------------
    // cancellation
    // -----------------------------------------------------------------------

    /// A stream whose reads never complete, standing in for a torrent parked on
    /// an unverified piece.
    struct NeverReady;

    impl tokio::io::AsyncRead for NeverReady {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    impl tokio::io::AsyncSeek for NeverReady {
        fn start_seek(
            self: std::pin::Pin<&mut Self>,
            _pos: SeekFrom,
        ) -> std::io::Result<()> {
            Ok(())
        }
        fn poll_complete(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<u64>> {
            std::task::Poll::Ready(Ok(0))
        }
    }

    /// cancel_fn arrives on ANOTHER thread while a read is parked; the parked
    /// read must return an error promptly instead of hanging mpv's demuxer.
    #[test]
    fn cancel_unblocks_a_parked_read() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");
        let mut source = TorrentSource {
            runtime: rt.handle().clone(),
            stream: NeverReady,
            len: 1024,
            pos: 0,
            cancel: Arc::new(CancelFlag::new()),
            label: "test".into(),
        };
        let flag = source.canceller().expect("torrent sources are cancellable");

        let done = Arc::new(AtomicBool::new(false));
        let watcher = {
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                assert!(!done.load(Ordering::SeqCst), "read returned before the cancel");
                flag.cancel();
            })
        };

        let started = std::time::Instant::now();
        let mut buf = [0u8; 64];
        let err = source.read(&mut buf).expect_err("a cancelled read must fail");
        done.store(true, Ordering::SeqCst);
        let elapsed = started.elapsed();
        watcher.join().unwrap();
        eprintln!("[cancel] read returned after {elapsed:?} with: {err}");
        assert!(err.contains("cancelled"), "unexpected error: {err}");
        assert!(elapsed < Duration::from_secs(5), "cancel took too long: {elapsed:?}");

        // Sticky: a read issued after the cancel must not sneak through.
        let err = source.read(&mut buf).expect_err("cancel must be sticky");
        assert!(err.contains("cancelled"), "unexpected error: {err}");
    }

    // -----------------------------------------------------------------------
    // the Engine bridge against a REAL librqbit session
    // -----------------------------------------------------------------------

    /// Minimal single-piece `.torrent` (bencode). The piece hash is bogus, so
    /// nothing ever verifies: exactly the "parked on an unverified piece" state
    /// this bridge has to survive. Keys are emitted in lexicographic order and
    /// `path` is a LIST of components, as bencode/librqbit require.
    fn make_torrent(name: &str, files: &[(&str, u64)]) -> Vec<u8> {
        let total: u64 = files.iter().map(|(_, len)| len).sum();
        let mut info = Vec::new();
        info.extend_from_slice(b"d5:filesl");
        for (fname, len) in files {
            let components: String = fname
                .split('/')
                .map(|part| format!("{}:{part}", part.len()))
                .collect();
            info.extend_from_slice(format!("d6:lengthi{len}e4:pathl{components}ee").as_bytes());
        }
        info.extend_from_slice(b"e");
        info.extend_from_slice(format!("4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(format!("12:piece lengthi{total}e").as_bytes());
        info.extend_from_slice(b"6:pieces20:");
        info.extend_from_slice(&[0u8; 20]);
        info.extend_from_slice(b"e");
        let mut t = Vec::new();
        t.extend_from_slice(b"d4:info");
        t.extend_from_slice(&info);
        t.extend_from_slice(b"e");
        t
    }

    /// Drives [`EngineSources`] against a live [`Engine`] from a NON-runtime
    /// thread, which is where the mpv callbacks actually run.
    ///
    /// The landmine this covers: librqbit's `ManagedTorrent::stream` builds a
    /// `BlockingSpawner` via `tokio::runtime::Handle::current()`, which PANICS
    /// outside a runtime context. It only works because `Handle::block_on`
    /// enters the runtime for us - and it also sets `allow_block_in_place`, so
    /// FileStream's `block_in_place` read does not panic off-worker either.
    #[test]
    fn engine_bridge_opens_seeks_and_cancels_a_real_file_stream() {
        const FILE_LEN: u64 = 4_000_000;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime");

        let dir = std::env::temp_dir().join("rillio-stream-cb-engine");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create cache dir");

        let (engine, info_hash) = rt.block_on(async {
            let engine = Engine::new(dir.clone()).await.expect("engine");
            let blob = make_torrent("Spike.Movie.2026", &[("Spike.Movie.2026.mkv", FILE_LEN)]);
            let handle = engine.add_blob(blob).await.expect("add_blob");
            let ih = Engine::info_hash_hex(&handle);
            (engine, ih)
        });
        eprintln!("[engine] info_hash = {info_hash}");

        // From the main thread: no runtime context, exactly like an mpv thread.
        let sources = EngineSources::new(engine, rt.handle().clone());
        let mut source = sources
            .open(&info_hash, 0)
            .expect("open the file through the engine bridge");

        assert_eq!(source.size(), Some(FILE_LEN), "size_fn must report the file length");
        assert_eq!(source.seek(1_000_000), Ok(1_000_000), "seek must reposition");
        assert_eq!(source.seek(0), Ok(0));
        eprintln!("[engine] size + seek OK against a live FileStream");

        // Nothing has verified, so the read parks. Cancel must free it.
        let flag = source.canceller().expect("torrent sources are cancellable");
        let watcher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.cancel();
        });
        let started = std::time::Instant::now();
        let mut buf = [0u8; 65536];
        let err = source.read(&mut buf).expect_err("an unverified piece must park, then cancel");
        watcher.join().unwrap();
        eprintln!("[engine] parked read cancelled after {:?}: {err}", started.elapsed());
        assert!(err.contains("cancelled"), "unexpected error: {err}");

        drop(source);
        drop(rt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The no-cancel-during-open rule: mpv installs `cancel_fn` only AFTER
    /// `open_fn` returns, so an open that blocks (the engine's
    /// METADATA_TIMEOUT is 180 s) would hang stop/loadfile/terminate. An
    /// infohash ABSENT from the session must therefore error out IMMEDIATELY
    /// (the web side retries over HTTP, which waits cancellably).
    #[test]
    fn engine_bridge_bounds_open_of_an_absent_torrent() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build runtime");

        let dir = std::env::temp_dir().join("rillio-stream-cb-absent");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create cache dir");
        let engine = rt.block_on(Engine::new(dir.clone())).expect("engine");

        // From the main thread: no runtime context, exactly like an mpv thread.
        let sources = EngineSources::new(engine, rt.handle().clone());
        let started = std::time::Instant::now();
        let err = sources
            .open(IH, 0)
            .err()
            .expect("an absent torrent with unresolvable metadata must not open");
        let elapsed = started.elapsed();
        eprintln!("[absent] open failed after {elapsed:?}: {err}");
        assert!(
            elapsed < Duration::from_secs(2),
            "open of an absent torrent must fail immediately, not wait on \
             metadata; took {elapsed:?}"
        );
        assert!(err.contains("not in the session"), "unexpected error: {err}");

        drop(rt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `block_on` deadlock guard: called from a tokio worker, every entry
    /// point must fail loud instead of parking a runtime thread forever.
    #[test]
    fn refuses_to_run_on_a_tokio_worker() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build runtime");
        let handle = rt.handle().clone();
        let err = rt.block_on(async move {
            let mut source = TorrentSource {
                runtime: handle,
                stream: NeverReady,
                len: 1024,
                pos: 0,
                cancel: Arc::new(CancelFlag::new()),
                label: "test".into(),
            };
            let mut buf = [0u8; 8];
            source.read(&mut buf).expect_err("must refuse to block a worker")
        });
        eprintln!("[guard] {err}");
        assert!(err.contains("tokio runtime thread"), "unexpected error: {err}");
    }
}
