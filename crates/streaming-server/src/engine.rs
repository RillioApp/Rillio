//! Torrent engine - a thin wrapper over one librqbit [`Session`] shared across
//! the whole server. Quiet by default: DHT + injected trackers for peer
//! discovery and batched disk writes, but NO inbound listen port and NO UPnP -
//! we connect outbound to peers without advertising a reachable port, so we are
//! not a discoverable seeder. `RILLIO_TORRENT_LISTEN=1` opts into the louder,
//! marginally-faster-on-rare-titles inbound behavior. A bring-your-own SOCKS5
//! proxy (RILLIO_SOCKS_PROXY) hides the client IP from peers and keeps the
//! inbound port off (no real-IP leak past the proxy). See [`Engine::new`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use librqbit::{
    torrent_from_bytes, AddTorrent, ByteBuf, ManagedTorrent, Session, SessionOptions,
    SessionPersistenceConfig,
};

use crate::{storage, types};

/// Handle to one managed torrent. librqbit defines this alias internally but
/// does not re-export it, so we mirror it.
pub type Handle = Arc<ManagedTorrent>;

/// How long a stream/create waits for a torrent to become streamable before
/// giving up. This must exceed librqbit's initial full-file checksum pass, which
/// for a large title (tens of GiB) can run ~a minute on a fresh add - the
/// torrent stays `Initializing` (not streamable) that whole time. Too short a
/// wait 500s the stream open mid-validation. (Removing that delay entirely is a
/// follow-up: a lazy response body that returns headers immediately and blocks
/// only the body until the torrent goes live.)
///
/// The same bound caps the step before it, resolving a magnet's metadata
/// from the swarm ([`Engine::resolve_metadata`]), which librqbit would
/// otherwise wait on forever.
const METADATA_TIMEOUT: Duration = Duration::from_secs(180);

/// Default public trackers injected into every torrent, mirroring the blob
/// (server.js:71921 / getDefaults). Without these, DHT is the only peer source
/// and less-popular content gets zero peers; the addon's own trackers are added
/// on top. librqbit supports UDP trackers.
///
/// These MUST reach a magnet through its `&tr=` params, not through
/// `AddTorrentOptions::trackers` - see [`magnet_with_default_trackers`].
const DEFAULT_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.demonoid.ch:6969/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://open.tracker.cl:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.therarbg.to:6969/announce",
    "udp://tracker.qu.ax:6969/announce",
    "udp://tracker.dler.org:6969/announce",
    "udp://tracker.bittor.pw:1337/announce",
    "udp://tracker.0x7c0.com:6969/announce",
    "udp://tracker-udp.gbitt.info:80/announce",
    "udp://run.publictracker.xyz:6969/announce",
    "udp://opentracker.io:6969/announce",
    "udp://open.dstud.io:6969/announce",
    "udp://leet-tracker.moe:1337/announce",
    "udp://explodie.org:6969/announce",
    "udp://bt.rer.lol:6969/announce",
];

/// Filename of the persisted torrent preferences, under the cache root. Written
/// by `POST /torrent-settings` (the desktop "faster downloads" toggle), read
/// once at [`Engine::new`].
const TORRENT_PREFS_FILE: &str = "torrent-settings.json";

/// Read the persisted torrent preferences from the cache root. Absent /
/// unreadable / malformed ⇒ defaults (listen off, streaming mode on).
pub fn read_torrent_settings(cache_dir: &Path) -> types::TorrentSettings {
    std::fs::read(cache_dir.join(TORRENT_PREFS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<types::TorrentSettings>(&bytes).ok())
        .unwrap_or(types::TorrentSettings {
            listen_enabled: false,
            streaming_mode: types::default_streaming_mode(),
        })
}

/// Persist the torrent preferences.
pub fn write_torrent_settings(cache_dir: &Path, settings: &types::TorrentSettings) -> std::io::Result<()> {
    let body = serde_json::to_vec(settings).expect("TorrentSettings serializes");
    std::fs::write(cache_dir.join(TORRENT_PREFS_FILE), body)
}

/// The "inbound listen port + UPnP" preference. Takes effect at [`Engine::new`]
/// (librqbit fixes the listener at session construction).
pub fn read_listen_pref(cache_dir: &Path) -> bool {
    read_torrent_settings(cache_dir).listen_enabled
}

/// Persist the listen preference, keeping the other settings intact.
pub fn write_listen_pref(cache_dir: &Path, listen_enabled: bool) -> std::io::Result<()> {
    let mut settings = read_torrent_settings(cache_dir);
    settings.listen_enabled = listen_enabled;
    write_torrent_settings(cache_dir, &settings)
}

/// Parse a KiB/s rate limit from an env var into librqbit's bytes-per-second
/// `NonZeroU32`. Unset / 0 / invalid ⇒ `None` (uncapped).
fn rate_limit_from_env(var: &str) -> Option<std::num::NonZeroU32> {
    std::env::var(var)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .and_then(|kib| kib.checked_mul(1024))
        .and_then(std::num::NonZeroU32::new)
}

/// Append [`DEFAULT_TRACKERS`] to a magnet link as `&tr=` params.
///
/// librqbit 8.1.1 honours `AddTorrentOptions::trackers` ONLY on the `.torrent`
/// bytes path (session.rs extends the announce-list with them). The MAGNET path
/// builds its tracker list purely from `magnet.trackers` and drops `opts.trackers`
/// on the floor. Every infohash-only add - the stream route AND /cache/download -
/// goes through `get_or_create_for_file` -> a bare `magnet:?xt=urn:btih:<ih>`, so the
/// injection above was silently dead for ALL of them: those torrents ran DHT-only
/// and less-popular titles sat at 0 peers / 0 bytes forever (the exact failure the
/// DEFAULT_TRACKERS doc warns about). Putting them in the URI is the only channel
/// librqbit reads. Values are percent-encoded; `Magnet::parse` reads them back
/// through `url::Url::query_pairs`, which decodes.
fn magnet_with_default_trackers(magnet: &str) -> String {
    // Every magnet we build or accept already carries `?xt=`, so `&tr=` appends
    // cleanly. Duplicates (an addon magnet that already lists one of ours) are
    // harmless: librqbit dedups trackers into a set before announcing.
    let mut out = String::from(magnet);
    for tracker in DEFAULT_TRACKERS {
        out.push_str("&tr=");
        out.extend(form_urlencoded::byte_serialize(tracker.as_bytes()));
    }
    out
}

/// Options for every add that creates a managed torrent. The download
/// selection is a REQUIRED argument, not an optional field left to its
/// default: librqbit's default (`only_files: None`) means "every file", and
/// leaving it there on every add is exactly how streaming one episode of a
/// season pack used to download the whole pack. Every caller states which
/// files it wants (see [`Pick`]).
fn add_torrent_options(only_files: Vec<usize>) -> librqbit::AddTorrentOptions {
    librqbit::AddTorrentOptions {
        only_files: Some(only_files),
        // Honoured on the .torrent-bytes path only. Every managed add is one
        // (see Engine::add_source), but a magnet's metadata resolution before
        // it ignores this, which is why magnets also carry them in the URI.
        trackers: Some(DEFAULT_TRACKERS.iter().map(|s| s.to_string()).collect()),
        // Reuse existing cache files instead of failing on them. With
        // allow_overwrite=false librqbit's fs storage opens files with
        // `create_new` (fs.rs), so re-adding a torrent whose files already exist
        // - the normal "close the app, reopen, replay the same title" flow, and
        // any add after a partial download - fails init ("file is None" / "error
        // creating a new file") and the stream 500s. `overwrite: true` opens
        // existing files with truncate(false): the initial checksum pass
        // validates what's on disk and playback RESUMES. Safe under our sandbox -
        // ConfinedStorage still confines every path before init runs, so this
        // only ever reuses files already under the cache root.
        overwrite: true,
        ..Default::default()
    }
}

/// Which files a NEW torrent downloads from the moment it is added.
///
/// There is deliberately no "everything" variant: no caller wants a whole
/// torrent by default. Fetching more is always an explicit act (the Cache
/// page's `/cache/select`).
pub enum Pick<'a> {
    /// The caller already knows the files (a numeric stream index, a preload's
    /// `fileIdx`). Handed straight to librqbit as `only_files`, which validates
    /// the indices against the real file list and refuses the add if one is out
    /// of range; no decision over the file list is needed.
    Files(Vec<usize>),
    /// Decided from the file list, which a magnet only has once its metadata
    /// arrives. See [`Engine::add_source`] for how that stays add-time. `Ok(vec![])`
    /// is a valid answer: the torrent is added with nothing downloading (a
    /// browsed season pack before any episode is played).
    Decide(&'a (dyn Fn(&[types::File]) -> anyhow::Result<Vec<usize>> + Send + Sync)),
}

/// The one file a playback request names.
pub enum FileRef<'a> {
    /// A plain index (`/{ih}/7`, the rillio:// byte plane, a preload fileIdx).
    Index(usize),
    /// Needs the file list to resolve (`/{ih}/-1`, a filename, a `?f=`
    /// selector). `None` means the request names no file of this torrent.
    Resolve(&'a (dyn Fn(&[types::File]) -> Option<usize> + Send + Sync)),
}

impl FileRef<'_> {
    fn resolve(&self, files: &[types::File]) -> anyhow::Result<usize> {
        match self {
            FileRef::Index(i) if *i < files.len() => Ok(*i),
            FileRef::Index(i) => {
                anyhow::bail!("file index {i} is out of range ({} files)", files.len())
            }
            FileRef::Resolve(resolve) => {
                resolve(files).context("the request does not name a file of this torrent")
            }
        }
    }
}

/// The wire `File` for one torrent entry. Shared by [`Engine::files`] (a
/// managed torrent's metadata) and the pre-add file list of [`Engine::add_source`],
/// so a selection decided before the add addresses exactly the indices the
/// stream route resolves afterwards.
fn wire_file(relative: &Path, length: u64, offset: u64) -> types::File {
    let path = relative.to_string_lossy().replace('\\', "/");
    let name = relative
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.clone());
    types::File { name, path, length, offset }
}

/// BitTorrent tuning knobs the web's torrent-profile selector drives (POST
/// `/settings`) and `GET /settings` reports back, and which the stats `opts`
/// echo reflects.
///
/// IMPORTANT - librqbit 8.1.1 can only honor ONE of these for real: the
/// download-speed HARD limit, applied as the session-wide download rate cap
/// (`Session::ratelimits`, live-tunable via `set_download_bps`). The rest are
/// stored and reported so the profile selector round-trips, but librqbit has NO
/// knob for them, so they are documented as report-only rather than silently
/// pretended-applied (fail loud):
///   - `max_connections`: librqbit hardcodes a 128 live-peer semaphore
///     (`torrent_state/live`: `Semaphore::new(128)`); no public override exists.
///   - soft limit / min_peers / handshake+request timeouts: no librqbit analog.
/// See [`Engine::apply_bt_profile`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BtProfile {
    pub max_connections: u64,
    pub handshake_timeout: u64,
    pub request_timeout: u64,
    pub download_speed_soft_limit: f64,
    pub download_speed_hard_limit: f64,
    pub min_peers_for_stable: u64,
}

impl BtProfile {
    /// Aggressive default so a fresh install downloads fast out of the box.
    /// Byte-for-byte the web `TORRENT_PROFILES["ultra fast"]` entry, so a clean
    /// profile shows "ultra fast" (not "custom") in Settings.
    pub const ULTRA_FAST: BtProfile = BtProfile {
        max_connections: 400,
        handshake_timeout: 25_000,
        request_timeout: 6_000,
        download_speed_soft_limit: 8_388_608.0,
        download_speed_hard_limit: 78_643_200.0,
        min_peers_for_stable: 10,
    };
}

/// Clamp a bytes/sec download cap (an `f64` from the web profile) to librqbit's
/// `NonZeroU32`. Non-finite / sub-1-byte => `None` (uncapped).
fn download_bps_from(bytes_per_sec: f64) -> Option<std::num::NonZeroU32> {
    if !bytes_per_sec.is_finite() || bytes_per_sec < 1.0 {
        return None;
    }
    std::num::NonZeroU32::new(bytes_per_sec.min(u32::MAX as f64) as u32)
}

/// Insert `(info_hash, file_id)` into the prefetch-dedup set, returning `true`
/// only if it was newly inserted (i.e. the caller owns the one prefetch for that
/// pair). A poisoned lock yields `false` (skip - the prefetch is best-effort).
/// Split out from [`Engine::mark_prefetch`] so the dedup logic is unit-testable
/// without standing up a full librqbit session.
fn mark_prefetch_in(
    set: &Mutex<HashSet<(String, usize)>>,
    info_hash: &str,
    file_id: usize,
) -> bool {
    match set.lock() {
        Ok(mut s) => s.insert((info_hash.to_owned(), file_id)),
        Err(_) => false,
    }
}

/// Shared torrent engine handle. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct Engine {
    session: Arc<Session>,
    /// How long an add may spend resolving a torrent's metadata from the
    /// swarm. [`METADATA_TIMEOUT`] always; a field only so the tests can
    /// shorten it instead of waiting minutes.
    resolve_timeout: Duration,
    /// Absolute cache root; torrents whose files would escape it are refused.
    cache_root: Arc<PathBuf>,
    /// Last time each torrent (by lowercase hex infohash) was streamed or queried.
    /// Drives cache-cap eviction: least-recently-used torrents go first, and a
    /// recently-touched (i.e. currently-playing) one is protected. See
    /// [`Engine::touch`] and [`Engine::enforce_cache_cap`].
    last_access: Arc<Mutex<HashMap<String, Instant>>>,
    /// infohash -> unix epoch ms the torrent entered the cache (see
    /// [`Self::added_at_stamped`]); persisted as `added.json`.
    added: Arc<Mutex<HashMap<String, u64>>>,
    /// (infohash, file_id) pairs whose tail (MKV Cues) has already been
    /// prefetched, so we warm each file's Cues at most once per session. See
    /// [`Engine::mark_prefetch`] and the tail-prefetch in stream.rs.
    prefetched: Arc<Mutex<HashSet<(String, usize)>>>,
    /// Current BitTorrent tuning profile (the web torrent-profile selector).
    /// Only its download HARD limit is live-applied to the session; the rest is
    /// stored for `/settings` reporting and the stats `opts` echo. See
    /// [`BtProfile`] and [`Engine::apply_bt_profile`].
    bt: Arc<Mutex<BtProfile>>,
    /// Lowercase hex infohashes the user chose to KEEP ("download to cache"):
    /// the cache-cap sweeper never evicts these. Persisted to
    /// [`PINS_FILE`] in the cache root, loaded at [`Engine::new`].
    pinned: Arc<Mutex<HashSet<String>>>,
    /// Lowercase hex infohash -> unix time (seconds) the player reported the
    /// stream WATCHED (>= ~90% through). Streaming mode's ephemeral sweeper
    /// deletes un-pinned entries a while after this mark; pinned ("kept")
    /// entries are never touched. Persisted to [`WATCHED_FILE`] so a watched
    /// stream still cleans up after a restart.
    watched: Arc<Mutex<HashMap<String, u64>>>,
    /// Lowercase hex infohash -> the addon metadata the media was matched to
    /// (see [`CacheMeta`]). Persisted to [`META_FILE`] so a title identified
    /// once stays identified across restarts.
    meta: Arc<Mutex<HashMap<String, CacheMeta>>>,
    /// Serializes every download-selection change. A selection change is a
    /// read-modify-write of librqbit's `only_files` (read the list, add or drop
    /// a file, write the whole list back), and two unserialized ones (two first
    /// plays of different episodes, a play beside a Cache page toggle) each
    /// write back a list missing the other's file. Held from the read through
    /// librqbit's write; the only way to change a selection is through
    /// [`Engine::update_selection`], which takes it.
    ///
    /// One lock per engine, not per torrent: the critical section is a
    /// synchronous librqbit update plus librqbit's persistence write, and that
    /// write already takes a session-wide lock on `session.json`, so selection
    /// changes on different torrents were serialized anyway. A per-infohash map
    /// would add entries to clean up on every remove for no real concurrency.
    selection: Arc<tokio::sync::Mutex<()>>,
}

/// Filename of the persisted pin set, under the cache root.
const PINS_FILE: &str = "pins.json";

/// Read the persisted pin set. Absent/unreadable/malformed => empty (nothing
/// pinned) - pins are cache-keeping hints, not integrity data.
fn read_pins(cache_dir: &std::path::Path) -> HashSet<String> {
    std::fs::read(cache_dir.join(PINS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

/// Persist the pin set. Loud on failure: an unsaved pin silently un-pins on the
/// next start, which the sweeper could then evict.
fn write_pins(cache_dir: &std::path::Path, pins: &HashSet<String>) {
    let list: Vec<&String> = pins.iter().collect();
    let body = serde_json::to_vec(&list).expect("pin list serializes");
    if let Err(e) = std::fs::write(cache_dir.join(PINS_FILE), body) {
        tracing::error!("pins: persisting {PINS_FILE} failed: {e}");
    }
}

/// Filename of the persisted per-torrent metadata sidecar, under the cache root.
const META_FILE: &str = "cache-meta.json";

/// What the app knows about the media inside a torrent: the addon metadata it
/// was matched to. Written either when playback starts from a real title in the
/// app (we already have all of this) or, for a torrent that arrived without
/// context, after searching the installed addons by its release name.
///
/// Deliberately opaque here: the server does not interpret these fields, it
/// stores and returns them, so the web can evolve the shape (extra art, a
/// release year, an episode label) without a server change. Only the sidecar's
/// map shape is the server's business.
pub type CacheMeta = serde_json::Value;

fn read_meta(cache_dir: &std::path::Path) -> HashMap<String, CacheMeta> {
    std::fs::read(cache_dir.join(META_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<HashMap<String, CacheMeta>>(&bytes).ok())
        .unwrap_or_default()
}

/// Persist the metadata sidecar. Loud on failure: losing it means the Cache
/// page falls back to scene filenames, which is exactly the state this exists
/// to fix.
fn write_meta(cache_dir: &std::path::Path, meta: &HashMap<String, CacheMeta>) {
    let body = serde_json::to_vec(meta).expect("cache metadata serializes");
    if let Err(e) = std::fs::write(cache_dir.join(META_FILE), body) {
        tracing::error!("cache-meta: persisting {META_FILE} failed: {e}");
    }
}

/// Filename of the persisted watched map, under the cache root.
const WATCHED_FILE: &str = "watched.json";

/// Read the persisted watched map. Absent/unreadable/malformed => empty -
/// watched marks are cleanup hints, not integrity data.
fn read_watched(cache_dir: &std::path::Path) -> HashMap<String, u64> {
    std::fs::read(cache_dir.join(WATCHED_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<HashMap<String, u64>>(&bytes).ok())
        .unwrap_or_default()
}

/// Persist the watched map. Loud on failure: a lost mark means a watched stream
/// silently never cleans up (the failure direction is at least data-safe).
fn write_watched(cache_dir: &std::path::Path, watched: &HashMap<String, u64>) {
    let body = serde_json::to_vec(watched).expect("watched map serializes");
    if let Err(e) = std::fs::write(cache_dir.join(WATCHED_FILE), body) {
        tracing::error!("watched: persisting {WATCHED_FILE} failed: {e}");
    }
}

/// Filename of the persisted added-at map (unix epoch MILLISECONDS, matching
/// what `Date` on the web side consumes directly), under the cache root.
const ADDED_FILE: &str = "added.json";

/// Read the persisted added-at map. Absent/unreadable/malformed => empty -
/// added dates are presentation hints (the Cache page's sort), not integrity
/// data; missing entries are backfilled from on-disk file times (see
/// [`Engine::added_at_stamped`]).
fn read_added(cache_dir: &std::path::Path) -> HashMap<String, u64> {
    std::fs::read(cache_dir.join(ADDED_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<HashMap<String, u64>>(&bytes).ok())
        .unwrap_or_default()
}

/// Persist the added-at map. Loud on failure: a lost stamp re-backfills from
/// file times on the next list, so the failure direction is only a possibly
/// drifting sort date.
fn write_added(cache_dir: &std::path::Path, added: &HashMap<String, u64>) {
    let body = serde_json::to_vec(added).expect("added map serializes");
    if let Err(e) = std::fs::write(cache_dir.join(ADDED_FILE), body) {
        tracing::error!("added: persisting {ADDED_FILE} failed: {e}");
    }
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Stale-fastresume guard
// ---------------------------------------------------------------------------
//
// librqbit 8.1.1 persists per-torrent fastresume under `<cache_root>/session/`:
// `session.json` (per-torrent output_folder), `<infohash>.bitv` (the raw
// BitTorrent have-bitfield, Msb0) and `<infohash>.torrent` (metainfo). On
// restore it TRUSTS the .bitv unconditionally: if the bitfield says "have all"
// but the data files at output_folder are gone (moved/deleted externally),
// librqbit preallocates fresh zero-filled files, reports the torrent 100%
// complete, and the stream route serves gigabytes of zeros (mpv dies with
// "unrecognized file format"). Silent corruption; the guard below runs BEFORE
// the Session is constructed and deletes the .bitv of any torrent whose claimed
// progress provably has no data behind it, forcing a clean re-download instead.

/// How many bytes of a file's head (and tail) the zero-prealloc check samples.
/// Never full reads: 64 KiB from each end is enough to tell a preallocated
/// zero-filled file from real media (which always has nonzero structure).
const ZERO_SAMPLE_LEN: u64 = 64 * 1024;

/// One torrent entry of librqbit's `session/session.json`, reduced to the two
/// fields the guard needs. Unknown fields are ignored, and the file itself is
/// never rewritten, so the rest of the entry stays untouched.
#[derive(serde::Deserialize)]
struct PersistedTorrentEntry {
    info_hash: String,
    output_folder: PathBuf,
}

/// The `session.json` shape (`{"torrents": {"<id>": {...}}}`).
#[derive(serde::Deserialize)]
struct PersistedSessionDb {
    torrents: HashMap<String, PersistedTorrentEntry>,
}

fn bitv_path(session_dir: &Path, info_hash: &str) -> PathBuf {
    session_dir.join(format!("{info_hash}.bitv"))
}

/// Whether the raw bitfield claims EVERY piece is downloaded. BitTorrent
/// bitfields are Msb0 (piece 0 = highest bit of byte 0); trailing pad bits of
/// the last byte are ignored.
fn bitv_claims_all_pieces(bitv: &[u8], num_pieces: usize) -> bool {
    if num_pieces == 0 {
        return false;
    }
    let full_bytes = num_pieces / 8;
    let rem_bits = num_pieces % 8;
    if bitv.len() < full_bytes + usize::from(rem_bits > 0) {
        return false;
    }
    if !bitv[..full_bytes].iter().all(|&b| b == 0xFF) {
        return false;
    }
    if rem_bits > 0 {
        let mask = 0xFFu8 << (8 - rem_bits);
        if bitv[full_bytes] & mask != mask {
            return false;
        }
    }
    true
}

/// Sample the first and last [`ZERO_SAMPLE_LEN`] bytes of `path`. `true` means
/// every sampled byte is zero, i.e. the file looks like a fresh preallocation
/// rather than downloaded data. Errors propagate (caller keeps the fastresume
/// when it cannot verify).
fn file_head_tail_all_zeros(path: &Path) -> anyhow::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {path:?}"))?;
    let len = f.metadata().with_context(|| format!("stat {path:?}"))?.len();
    if len == 0 {
        // An existing but empty file carries no data; equivalent to all-zero.
        return Ok(true);
    }
    let head = len.min(ZERO_SAMPLE_LEN) as usize;
    let mut buf = vec![0u8; head];
    f.read_exact(&mut buf).with_context(|| format!("reading head of {path:?}"))?;
    if buf.iter().any(|&b| b != 0) {
        return Ok(false);
    }
    if len > ZERO_SAMPLE_LEN {
        let tail = ZERO_SAMPLE_LEN.min(len - ZERO_SAMPLE_LEN) as usize;
        f.seek(SeekFrom::End(-(tail as i64)))
            .with_context(|| format!("seeking tail of {path:?}"))?;
        let mut buf = vec![0u8; tail];
        f.read_exact(&mut buf).with_context(|| format!("reading tail of {path:?}"))?;
        if buf.iter().any(|&b| b != 0) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Decide whether one torrent's fastresume is provably stale. Returns
/// `Some(reason)` when the .bitv must be invalidated, `None` to keep it, and
/// `Err` when it cannot be verified (caller keeps it and warns).
///
/// Deliberately conservative, two rules only:
/// 1. The .bitv claims progress (any bit set) but NONE of the torrent's
///    payload files exist at the recorded output_folder: a fully absent
///    download. Partially-moved trees (some files still present) are kept.
/// 2. The .bitv claims EVERY piece, all payload files exist, and every one of
///    them samples all-zero at head and tail: the zero-preallocated corpse a
///    previous run left behind after trusting rule-1 state. A false positive
///    here only costs a re-hash/re-download; the data files themselves are
///    never touched.
fn check_fastresume_stale(
    session_dir: &Path,
    entry: &PersistedTorrentEntry,
) -> anyhow::Result<Option<&'static str>> {
    let bitv_bytes = match std::fs::read(bitv_path(session_dir, &entry.info_hash)) {
        Ok(b) => b,
        // No fastresume, nothing to invalidate.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading .bitv"),
    };
    if bitv_bytes.iter().all(|&b| b == 0) {
        // Claims no progress; a full initial check runs anyway.
        return Ok(None);
    }

    let torrent_file = session_dir.join(format!("{}.torrent", entry.info_hash));
    let torrent_raw = std::fs::read(&torrent_file)
        .with_context(|| format!("reading {torrent_file:?} (cannot enumerate expected files)"))?;
    let meta = torrent_from_bytes::<ByteBuf>(&torrent_raw)
        .with_context(|| format!("parsing {torrent_file:?}"))?;

    // Payload files only: bep-47 padding files are never written to disk, and
    // zero-length files prove nothing about downloaded data.
    let files: Vec<PathBuf> = meta
        .info
        .iter_file_details()
        .context("listing torrent files")?
        .filter(|fd| !fd.attrs().padding && fd.len > 0)
        .map(|fd| fd.filename.to_pathbuf())
        .collect::<anyhow::Result<_>>()?;
    if files.is_empty() {
        return Ok(None);
    }

    let existing: Vec<PathBuf> = files
        .iter()
        .map(|rel| entry.output_folder.join(rel))
        .filter(|p| p.is_file())
        .collect();
    if existing.is_empty() {
        return Ok(Some("no data files exist at the recorded output folder"));
    }

    let num_pieces = meta.info.pieces.as_ref().len() / 20;
    if existing.len() == files.len() && bitv_claims_all_pieces(&bitv_bytes, num_pieces) {
        for path in &existing {
            if !file_head_tail_all_zeros(path)? {
                return Ok(None);
            }
        }
        return Ok(Some(
            "bitfield claims complete but every data file samples all-zero (stale preallocation)",
        ));
    }
    Ok(None)
}

/// Startup guard: walk `session.json` and delete the `.bitv` of every torrent
/// whose fastresume is provably stale (see [`check_fastresume_stale`]), so the
/// librqbit [`Session`] constructed right after never trusts it. The
/// `.torrent` file stays so metadata survives and the torrent re-adds without
/// a magnet round-trip; librqbit then runs a fresh initial check and
/// re-downloads instead of serving zeros. Every invalidation and every
/// entry that cannot be verified is logged loudly.
fn invalidate_stale_fastresume(session_dir: &Path) {
    let db_path = session_dir.join("session.json");
    let raw = match std::fs::read(&db_path) {
        Ok(b) => b,
        // Fresh install / first boot: no persisted session, nothing to guard.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!("fastresume-guard: cannot read {db_path:?}: {e}; skipping guard");
            return;
        }
    };
    let db: PersistedSessionDb = match serde_json::from_slice(&raw) {
        Ok(db) => db,
        Err(e) => {
            // Malformed db: librqbit itself fails loud constructing the
            // session; not this guard's call to delete anything.
            tracing::warn!("fastresume-guard: {db_path:?} unparseable ({e}); leaving it to librqbit");
            return;
        }
    };
    for entry in db.torrents.values() {
        // The infohash names files we join onto session_dir; only the exact
        // 40-char lowercase-hex shape librqbit writes is trusted.
        let ih = entry.info_hash.as_str();
        if ih.len() != 40 || !ih.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
            tracing::warn!("fastresume-guard: skipping malformed info_hash {ih:?} in {db_path:?}");
            continue;
        }
        match check_fastresume_stale(session_dir, entry) {
            Ok(Some(reason)) => {
                let bitv = bitv_path(session_dir, ih);
                match std::fs::remove_file(&bitv) {
                    Ok(()) => tracing::warn!(
                        "fastresume-guard: invalidated fastresume for {ih} ({reason}); \
                         output_folder={:?}; deleted {bitv:?} so librqbit re-checks instead of \
                         trusting stale state and serving zeros",
                        entry.output_folder,
                    ),
                    Err(e) => tracing::warn!(
                        "fastresume-guard: {ih} is stale ({reason}) but deleting {bitv:?} \
                         failed: {e}; librqbit may resume corrupt state"
                    ),
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(
                "fastresume-guard: cannot verify {ih}: {e:#}; keeping its fastresume as-is"
            ),
        }
    }
}

impl Engine {
    /// Bootstrap the session rooted at `cache_dir`. librqbit lays out per-torrent
    /// subfolders beneath it.
    ///
    /// All torrent storage goes through [`ConfinedStorageFactory`]: every file is
    /// confined under `cache_dir` (path-traversal guard) and created
    /// non-executable. There is no per-torrent size cap - a streaming server
    /// plays a window of a torrent regardless of its total size.
    pub async fn new(cache_dir: PathBuf) -> anyhow::Result<Self> {
        let cache_root = storage::absolutize(&cache_dir)?;

        // Bring-your-own SOCKS5 proxy (privacy): peers see the proxy's IP, not
        // yours. Off unless RILLIO_SOCKS_PROXY is set. NOTE: we never ship a
        // curated proxy list - a stranger's free proxy sees your IP + all traffic
        // and often can't carry UDP (breaks DHT/uTP). Trust is the user's to bring.
        let socks_proxy = std::env::var("RILLIO_SOCKS_PROXY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // Inbound listen port + UPnP make us reachable by NAT'd/passive seeders -
        // more peers, faster especially on less-seeded titles. But being reachable
        // is also what makes a client a discoverable SEEDER: anti-piracy monitors
        // join a swarm and connect INBOUND to log distributors. Outbound-only
        // leeching still saturates the pipe on well-seeded content (BitTorrent is
        // already multi-source), so the download-speed cost of staying quiet is
        // small while the exposure saved is large. OFF by default.
        //
        // Resolved as: RILLIO_TORRENT_LISTEN env (explicit on/off, a dev escape
        // hatch) ⇒ else the persisted "faster downloads" toggle from the cache
        // root ⇒ else off. A SOCKS5 proxy only tunnels OUTBOUND, so a
        // real-interface listener would leak the real IP past it - the proxy keeps
        // the listener off regardless of the above.
        let listen_requested = match std::env::var("RILLIO_TORRENT_LISTEN").as_deref() {
            Ok("1") | Ok("true") => true,
            Ok("0") | Ok("false") => false,
            _ => read_listen_pref(&cache_dir),
        };
        let listen_enabled = socks_proxy.is_none() && listen_requested;

        let opts = SessionOptions {
            // DHT on: with trackers off it is the only peer source for a magnet.
            disable_dht: false,
            // No DHT routing-table cache file. A streaming server does not need a
            // persisted peer cache, and the shared cache path otherwise serializes
            // multiple Session instances (e.g. concurrent tests) onto one file.
            disable_dht_persistence: true,
            listen_port_range: if listen_enabled { Some(6881..6889) } else { None },
            enable_upnp_port_forwarding: listen_enabled,
            socks_proxy_url: socks_proxy,
            // Batch disk writes (hold up to 32 MiB in memory before flushing) so
            // per-write fsync stalls don't cap throughput once a well-seeded title
            // starts saturating the pipe.
            defer_writes_up_to: Some(32 * 1024 * 1024),
            // Drop dead/slow peers faster so their connection slots recycle to live
            // ones instead of sitting idle on a stalled handshake.
            peer_opts: Some(librqbit::PeerConnectionOptions {
                connect_timeout: Some(Duration::from_secs(10)),
                read_write_timeout: Some(Duration::from_secs(60)),
                keep_alive_interval: None,
            }),
            // Optional rate caps (KiB/s via env, uncapped by default). A modest
            // UPLOAD cap is the useful one: on an asymmetric link, a saturated
            // upstream delays the TCP ACKs for your downloads, so capping upload
            // can raise DOWNLOAD throughput. Download cap is there for parity.
            // The download cap defaults to the ultra-fast profile's HARD limit
            // (~75 MiB/s, effectively uncapped for any home link), so a fresh
            // install downloads aggressively. A user switching torrent profiles
            // re-applies this live (see `apply_bt_profile`). The explicit env
            // knob still wins at startup for a hard operator override.
            ratelimits: librqbit::limits::LimitsConfig {
                upload_bps: rate_limit_from_env("RILLIO_UPLOAD_LIMIT_KBPS"),
                download_bps: rate_limit_from_env("RILLIO_DOWNLOAD_LIMIT_KBPS")
                    .or_else(|| download_bps_from(BtProfile::ULTRA_FAST.download_speed_hard_limit)),
            },
            // Persist torrent state + fast-resume so a restart RESUMES instantly
            // instead of re-hashing the whole file (~a minute for a 31 GiB title).
            // librqbit's persistence store type-checks for its native
            // FilesystemStorageFactory, so we use that (default_storage_factory
            // None) rather than a wrapper. Path confinement is instead asserted at
            // add time ([`Engine::assert_confined`]) and already enforced by
            // librqbit-core (parse-time ".." rejection). See storage.rs.
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(cache_dir.join("session")),
            }),
            fastresume: true,
            default_storage_factory: None,
            ..Default::default()
        };
        // Stale fastresume must be invalidated BEFORE the session is built:
        // librqbit trusts the persisted bitfields at restore time, and a
        // "have all" bitfield over missing files silently becomes zero-filled
        // preallocations served as a 100%-complete stream.
        invalidate_stale_fastresume(&cache_dir.join("session"));
        let session = Session::new_with_opts(cache_dir, opts).await?;
        Ok(Self::from_session(session, cache_root))
    }

    /// The engine around an already-built session, reading the persisted
    /// sidecars (pins, watched marks, metadata, added dates) from `cache_root`.
    fn from_session(session: Arc<Session>, cache_root: PathBuf) -> Self {
        let pinned = read_pins(&cache_root);
        let watched = read_watched(&cache_root);
        let meta = read_meta(&cache_root);
        let added = read_added(&cache_root);
        Self {
            session,
            resolve_timeout: METADATA_TIMEOUT,
            cache_root: Arc::new(cache_root),
            last_access: Arc::new(Mutex::new(HashMap::new())),
            prefetched: Arc::new(Mutex::new(HashSet::new())),
            bt: Arc::new(Mutex::new(BtProfile::ULTRA_FAST)),
            pinned: Arc::new(Mutex::new(pinned)),
            watched: Arc::new(Mutex::new(watched)),
            meta: Arc::new(Mutex::new(meta)),
            added: Arc::new(Mutex::new(added)),
            selection: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// When this torrent entered the cache, as unix epoch milliseconds.
    ///
    /// The stamp is written on add ([`Self::stamp_added_if_new`]); a torrent
    /// that predates the stamping (an existing cache) is backfilled ONCE from
    /// its files' on-disk times - the oldest creation time among them, which on
    /// Windows is the moment the download was first written - and the backfill
    /// is persisted so the date never drifts afterwards. No file times at all
    /// (metadata still resolving) leaves the map alone and reports "now"
    /// transiently, so a just-added magnet sorts newest instead of at 1970.
    pub fn added_at_stamped(&self, info_hash: &str, handle: &Handle) -> u64 {
        if let Ok(map) = self.added.lock() {
            if let Some(&at) = map.get(info_hash) {
                return at;
            }
        }
        let oldest_file_time: Option<u64> = Self::files(handle)
            .iter()
            .filter_map(|file| {
                let meta = std::fs::metadata(self.cache_root.join(&file.path)).ok()?;
                let time = meta.created().or_else(|_| meta.modified()).ok()?;
                let ms = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64;
                Some(ms)
            })
            .min();
        match oldest_file_time {
            Some(at) => {
                if let Ok(mut map) = self.added.lock() {
                    map.insert(info_hash.to_owned(), at);
                    write_added(&self.cache_root, &map);
                }
                at
            }
            None => now_epoch_ms(),
        }
    }

    /// Stamp a torrent's added time if it has none; persists immediately.
    pub fn stamp_added_if_new(&self, info_hash: &str) {
        if let Ok(mut map) = self.added.lock() {
            if !map.contains_key(info_hash) {
                map.insert(info_hash.to_owned(), now_epoch_ms());
                write_added(&self.cache_root, &map);
            }
        }
    }

    /// Drop a torrent's added stamp; persists immediately. Deletion cleanup:
    /// a re-add of the same infohash is a NEW cache entry and must date from
    /// its re-add, not the original download.
    fn clear_added(&self, info_hash: &str) {
        if let Ok(mut map) = self.added.lock() {
            if map.remove(info_hash).is_some() {
                write_added(&self.cache_root, &map);
            }
        }
    }

    /// The stored metadata for a torrent, if it has been identified.
    pub fn meta(&self, info_hash: &str) -> Option<CacheMeta> {
        self.meta.lock().ok().and_then(|m| m.get(info_hash).cloned())
    }

    /// Store (or clear) a torrent's metadata; persists immediately.
    pub fn set_meta(&self, info_hash: &str, meta: Option<CacheMeta>) {
        if let Ok(mut m) = self.meta.lock() {
            match meta {
                Some(value) => {
                    if m.get(info_hash) == Some(&value) {
                        return;
                    }
                    m.insert(info_hash.to_owned(), value);
                }
                None => {
                    if m.remove(info_hash).is_none() {
                        return;
                    }
                }
            }
            write_meta(&self.cache_root, &m);
        }
    }

    /// Whether the user pinned this torrent ("download to cache").
    pub fn is_pinned(&self, info_hash: &str) -> bool {
        self.pinned
            .lock()
            .map(|p| p.contains(info_hash))
            .unwrap_or(false)
    }

    /// Pin or unpin a torrent; persists immediately.
    pub fn set_pinned(&self, info_hash: &str, pinned: bool) {
        if let Ok(mut p) = self.pinned.lock() {
            let changed = if pinned {
                p.insert(info_hash.to_owned())
            } else {
                p.remove(info_hash)
            };
            if changed {
                write_pins(&self.cache_root, &p);
            }
        }
    }

    /// Whether the player marked this torrent watched (streaming mode).
    pub fn is_watched(&self, info_hash: &str) -> bool {
        self.watched
            .lock()
            .map(|w| w.contains_key(info_hash))
            .unwrap_or(false)
    }

    /// Mark or unmark a torrent watched; persists immediately. Marking stamps
    /// the current unix time - the ephemeral sweeper's grace clock starts here.
    pub fn set_watched(&self, info_hash: &str, watched: bool) {
        if let Ok(mut w) = self.watched.lock() {
            let changed = if watched {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                w.insert(info_hash.to_owned(), now).is_none()
            } else {
                w.remove(info_hash).is_some()
            };
            if changed {
                write_watched(&self.cache_root, &w);
            }
        }
    }

    /// Streaming mode's ephemeral sweep: delete torrents the player marked
    /// watched, once the mark is at least `ttl` old - late enough that
    /// "rewatch that scene" and the previous-episode-while-the-next-plays cases
    /// survive. Never touches pinned ("kept") torrents, anything streamed or
    /// queried within `grace` (active playback), and does nothing at all while
    /// streaming mode is off (checked from the settings file each sweep, so
    /// flipping the toggle applies live). Watched marks whose torrent is gone
    /// (deleted manually) are pruned so the map cannot grow stale entries.
    pub async fn sweep_watched(&self, ttl: Duration, grace: Duration) {
        if !read_torrent_settings(&self.cache_root).streaming_mode {
            return;
        }
        let watched = self.watched.lock().map(|w| w.clone()).unwrap_or_default();
        if watched.is_empty() {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let managed: HashSet<String> = self.all().iter().map(Self::info_hash_hex).collect();
        let last = self.last_access.lock().map(|m| m.clone()).unwrap_or_default();
        for (ih, marked_at) in watched {
            if !managed.contains(&ih) {
                self.set_watched(&ih, false);
                continue;
            }
            if self.is_pinned(&ih) {
                continue;
            }
            if now.saturating_sub(marked_at) < ttl.as_secs() {
                continue;
            }
            if last.get(&ih).is_some_and(|t| t.elapsed() < grace) {
                continue;
            }
            if self.remove(&ih).await {
                if let Ok(mut m) = self.last_access.lock() {
                    m.remove(&ih);
                }
                tracing::info!("streaming-mode: cleaned up watched stream {ih}");
            }
        }
    }

    /// Pause a live torrent (the Cached page's per-row pause button).
    ///
    /// Idempotent: already-paused is success. Everything else is REPORTED, not
    /// swallowed - librqbit refuses to pause a torrent that is still hash-checking
    /// ("torrent is initializing, can't pause"), and this used to drop that into a
    /// warn! while `/cache/pause` still answered {"success": true}. A pause during
    /// the check therefore did nothing, said it worked, and the download carried
    /// on: the other half of the "pause sometimes doesn't work" report.
    pub async fn pause(&self, handle: &Handle) -> anyhow::Result<()> {
        if handle.is_paused() {
            return Ok(());
        }
        self.session.pause(handle).await.context("pause failed")
    }

    /// Resume a paused torrent (a disk-full write error pauses it; the user
    /// fixing the disk and retrying expects it to pick back up).
    /// Idempotent: not-paused is success. See [`Engine::pause`] for why failures
    /// propagate instead of becoming a warn!.
    pub async fn unpause(&self, handle: &Handle) -> anyhow::Result<()> {
        if !handle.is_paused() {
            return Ok(());
        }
        self.session.unpause(handle).await.context("unpause failed")
    }

    /// Make sure `file_idx` is downloading: the invariant every playback entry
    /// point (stream route, rillio:// byte plane, /cache/download) needs.
    ///
    /// - An explicit selection (`Some`) only ever GROWS here: the file joins
    ///   it, nothing the user or an earlier play selected is dropped. Already
    ///   selected is a no-op with no librqbit call, which matters because mpv
    ///   opens many connections per title and every one lands here.
    /// - `None` ("every file") is never produced by this code any more: every
    ///   add states its selection ([`Pick`]) and every update is explicit. It
    ///   only survives on torrents added before that fix (restored from
    ///   `session/session.json` with `only_files: null`), and there it was
    ///   never anyone's choice - `/cache/select` on such a torrent always
    ///   writes an explicit list. So the first play NARROWS it: to the played
    ///   file plus every file already COMPLETE on disk. Nothing downloaded is
    ///   discarded or hidden (deselecting never deletes bytes, and complete
    ///   files stay selected so the Cache page still counts them); partial
    ///   files of unplayed episodes stop growing, keeping what they have.
    ///
    /// Fails loud: librqbit refuses a selection change while the torrent is
    /// still hash-checking ("can't update initializing torrent"), and the
    /// caller must not serve a stream that silently keeps pulling the pack.
    pub async fn ensure_selected(&self, handle: &Handle, file_idx: usize) -> anyhow::Result<()> {
        // Lock-free fast path for the common case, every mpv connection to a
        // file already downloading. Safe without the lock: this function only
        // ever GROWS a selection, so "already in it" needs no write; a
        // concurrent deselect racing it is the same as that deselect landing
        // just after. Anything else re-checks under the lock below.
        if handle.only_files().is_some_and(|only| only.contains(&file_idx)) {
            return Ok(());
        }
        let ih = Self::info_hash_hex(handle);
        self.update_selection(handle, |current| {
            Ok(match current {
                Some(only) if only.contains(&file_idx) => None,
                Some(mut only) => {
                    only.push(file_idx);
                    only.sort_unstable();
                    tracing::info!("selection {ih}: adding file {file_idx} -> {only:?}");
                    Some(only)
                }
                None => {
                    let files = Self::files(handle);
                    let progress = handle.stats().file_progress;
                    let mut keep: Vec<usize> = files
                        .iter()
                        .enumerate()
                        .filter(|(i, f)| f.length > 0 && progress.get(*i) == Some(&f.length))
                        .map(|(i, _)| i)
                        .collect();
                    if !keep.contains(&file_idx) {
                        keep.push(file_idx);
                        keep.sort_unstable();
                    }
                    tracing::warn!(
                        "selection {ih}: legacy all-files selection ({} files) narrowed on play \
                         of file {file_idx} to {keep:?} (the played file + files already \
                         complete on disk); no bytes on disk are touched",
                        files.len()
                    );
                    Some(keep)
                }
            })
        })
        .await
        .with_context(|| format!("selecting file {file_idx} of {ih} failed"))
    }

    /// Add one file to the download selection or drop it (the Cache page's
    /// per-file toggles). Unlike [`Engine::ensure_selected`] this can also
    /// REMOVE a file, so it is the path for "stop downloading that extra".
    ///
    /// Dropping the last selected file is allowed: nothing selected is a real
    /// state, the one a browsed season pack is added in (see [`Pick::Decide`]),
    /// so the Cache page must be able to reach it too. The torrent stays
    /// cached, downloading nothing, and its file browser can pick files again.
    /// A legacy all-files selection (`None`) is read as every file, so a toggle
    /// on it writes an explicit list.
    pub async fn set_file_selected(
        &self,
        handle: &Handle,
        file_idx: usize,
        selected: bool,
    ) -> anyhow::Result<()> {
        let count = Self::files(handle).len();
        if file_idx >= count {
            anyhow::bail!("file index {file_idx} is out of range ({count} files)");
        }
        self.update_selection(handle, |current| {
            let mut next: Vec<usize> = match current {
                Some(only) => only.into_iter().filter(|&i| i < count).collect(),
                None => (0..count).collect(),
            };
            if selected {
                if next.contains(&file_idx) {
                    return Ok(None);
                }
                next.push(file_idx);
                next.sort_unstable();
            } else {
                if !next.contains(&file_idx) {
                    return Ok(None);
                }
                next.retain(|&i| i != file_idx);
            }
            Ok(Some(next))
        })
        .await
        .context("updating the file selection failed")
    }

    /// The one place a download selection changes. `change` gets the current
    /// selection (`None` = librqbit's "every file") and returns the new one,
    /// or `None` for "leave it as it is" (no librqbit call, no persistence
    /// write). The read and the write happen under [`Engine::selection`], so a
    /// concurrent change can never be overwritten by a list read before it.
    ///
    /// The lock is held across librqbit's `session.json` write too: its
    /// `Session::update_only_files` applies the selection and then awaits the
    /// persistence update in one call, and the write is not exposed on its own.
    /// That write already serializes session-wide inside librqbit, so holding
    /// ours across it adds no real contention, and the no-change case (the
    /// common one, see the fast path in [`Engine::ensure_selected`]) never
    /// reaches it.
    async fn update_selection(
        &self,
        handle: &Handle,
        change: impl FnOnce(Option<Vec<usize>>) -> anyhow::Result<Option<Vec<usize>>>,
    ) -> anyhow::Result<()> {
        let _serialized = self.selection.lock().await;
        let Some(next) = change(handle.only_files())? else {
            return Ok(());
        };
        self.session.update_only_files(handle, &next.into_iter().collect()).await
    }

    /// The current BitTorrent profile (for `GET /settings` and the stats echo).
    pub fn bt_profile(&self) -> BtProfile {
        self.bt.lock().map(|g| *g).unwrap_or(BtProfile::ULTRA_FAST)
    }

    /// Apply a BitTorrent profile from `POST /settings`. The download-speed HARD
    /// limit takes effect LIVE on the whole session (all torrents, via
    /// librqbit's `Session::ratelimits`). Every other field is stored for
    /// reporting only - librqbit 8.1.1 exposes no knob for them (see
    /// [`BtProfile`]), so we do NOT pretend otherwise.
    pub fn apply_bt_profile(&self, profile: BtProfile) {
        if let Ok(mut g) = self.bt.lock() {
            *g = profile;
        }
        self.session
            .ratelimits
            .set_download_bps(download_bps_from(profile.download_speed_hard_limit));
        tracing::info!(
            "bt-profile applied: download cap ~{} B/s live (max_connections={} reported \
             but librqbit caps live peers at 128; soft/min-peers/timeouts report-only)",
            profile.download_speed_hard_limit,
            profile.max_connections,
        );
    }

    /// Claim the tail prefetch for `(info_hash, file_id)`. Returns `true` only
    /// the FIRST time a pair is seen, so the caller spawns the Cues-warming task
    /// at most once per file. A poisoned lock yields `false` (skip - the
    /// prefetch is best-effort, never load-bearing).
    pub fn mark_prefetch(&self, info_hash: &str, file_id: usize) -> bool {
        mark_prefetch_in(&self.prefetched, info_hash, file_id)
    }

    /// Record that `info_hash` (lowercase hex) was just streamed/queried. Called
    /// from the stream, stats and create routes so the cache sweeper can tell an
    /// actively-used torrent from a stale one.
    pub fn touch(&self, info_hash: &str) {
        if let Ok(mut m) = self.last_access.lock() {
            m.insert(info_hash.to_owned(), Instant::now());
        }
    }

    /// Approximate on-disk cache weight: bytes downloaded (and thus written to the
    /// cache root) across all managed torrents.
    pub fn cache_bytes(&self) -> u64 {
        self.session
            .with_torrents(|it| it.map(|(_, h)| h.stats().progress_bytes).sum())
    }

    /// Enforce a `cap`-byte cache by evicting least-recently-used torrents (which
    /// deletes their cached files) until under the cap. A torrent touched within
    /// `grace` is never evicted, so the currently-playing title is safe. Loud: logs
    /// every eviction and warns if it cannot reach the cap (only active torrents
    /// remain). Adds are never refused by size - the bound is applied here.
    pub async fn enforce_cache_cap(&self, cap: u64, grace: Duration) {
        let mut used = self.cache_bytes();
        if used <= cap {
            return;
        }

        let now = Instant::now();
        let last = self.last_access.lock().map(|m| m.clone()).unwrap_or_default();
        // (infohash, bytes, idle-duration). Unknown touch => most idle (evict first).
        let mut candidates: Vec<(String, u64, Duration)> = self
            .all()
            .iter()
            .map(|h| {
                let ih = Self::info_hash_hex(h);
                let bytes = h.stats().progress_bytes;
                let idle = last.get(&ih).map(|t| now.duration_since(*t)).unwrap_or(Duration::MAX);
                (ih, bytes, idle)
            })
            // Protect anything touched within the grace window (active playback)
            // and anything the user pinned ("download to cache" keeps).
            .filter(|(ih, _, idle)| *idle >= grace && !self.is_pinned(ih))
            .collect();
        // Most idle first.
        candidates.sort_by(|a, b| b.2.cmp(&a.2));

        tracing::warn!("cache-cap: usage ~{used} over cap {cap}; evicting idle torrents");
        for (ih, bytes, _) in candidates {
            if used <= cap {
                break;
            }
            if self.remove(&ih).await {
                if let Ok(mut m) = self.last_access.lock() {
                    m.remove(&ih);
                }
                used = used.saturating_sub(bytes);
                tracing::warn!("cache-cap: evicted {ih} (~{bytes} bytes), usage now ~{used}/{cap}");
            }
        }
        if used > cap {
            tracing::warn!(
                "cache-cap: still ~{used} bytes over {cap} after evicting idle torrents \
                 (active torrents are protected)"
            );
        }
    }

    /// Refuse a torrent whose files would resolve outside the cache root. A
    /// belt-and-suspenders assertion over librqbit-core's own parse-time
    /// rejection. If metadata has not resolved we cannot enumerate the files, so
    /// we fail loud (deny) rather than let the check pass vacuously on an empty
    /// list; callers only run this once metadata is expected to be present.
    fn assert_confined(&self, handle: &Handle) -> anyhow::Result<()> {
        let files: Vec<PathBuf> = handle
            .with_metadata(|m| m.file_infos.iter().map(|fi| fi.relative_filename.clone()).collect())
            .context("assert_confined: torrent metadata not resolved, cannot verify confinement")?;
        storage::assert_confined(&self.cache_root, files.iter().map(PathBuf::as_path))
    }

    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    /// Add a raw `.torrent` blob (`POST /create`), downloading only `pick`.
    /// Metadata is immediate. Re-adding a managed torrent returns it untouched.
    pub async fn add_blob(&self, bytes: Vec<u8>, pick: Pick<'_>) -> anyhow::Result<Handle> {
        self.add_source(AddTorrent::from_bytes(bytes), pick).await
    }

    /// The one place a torrent enters the session. The selection is set AT ADD
    /// TIME, so no byte of an unwanted file is ever requested and no unwanted
    /// file is ever preallocated (librqbit's initial check sizes only the
    /// selected files).
    ///
    /// Every add is two steps. First the metadata is resolved on its own
    /// ([`Engine::resolve_metadata`], librqbit's `list_only`: the metadata
    /// comes back as a complete `.torrent` carrying the magnet's trackers and
    /// nothing is created), bounded by [`Engine::resolve_timeout`]. Then those
    /// `.torrent` bytes are added with the selection set, handing the peers
    /// met during resolution back as `initial_peers` so the real add does not
    /// start its swarm cold. [`Pick::Decide`] needs the file list in between;
    /// [`Pick::Files`] goes the same way so that the one step that waits on the
    /// swarm is always the bounded one that creates nothing. (A single-step
    /// magnet add resolves INSIDE `add_torrent`, and abandoning it at a
    /// deadline could land between librqbit registering the torrent and
    /// starting it, leaving a torrent that exists and never runs.)
    ///
    /// Rejected alternatives: add-then-narrow and add-paused-then-select both
    /// create the torrent with every file selected first, and librqbit's
    /// initial check then preallocates every selected file on disk (a paused
    /// add still initializes), so a 100 GB pack reserves 100 GB before the
    /// selection lands; add-then-narrow additionally races the swarm for the
    /// unwanted pieces.
    async fn add_source(&self, add: AddTorrent<'_>, pick: Pick<'_>) -> anyhow::Result<Handle> {
        let listed = self.resolve_metadata(add).await?;
        let info_hash = listed.info_hash.as_string();
        // A concurrent add of the same torrent may have landed while we
        // resolved: its selection stands, and re-adding would only come back
        // as AlreadyManaged anyway.
        if let Some(handle) = self.get(&info_hash) {
            return Ok(handle);
        }
        let selection = match pick {
            Pick::Files(files) => files,
            Pick::Decide(decide) => {
                let mut offset = 0u64;
                let files: Vec<types::File> = listed
                    .info
                    .iter_file_details()
                    .context("listing the torrent's files")?
                    .map(|fd| {
                        let file = wire_file(&fd.filename.to_pathbuf()?, fd.len, offset);
                        offset += fd.len;
                        Ok(file)
                    })
                    .collect::<anyhow::Result<_>>()?;
                let selection = decide(&files)?;
                tracing::info!("add {info_hash}: selecting {selection:?} of {} files", files.len());
                selection
            }
        };
        let mut opts = add_torrent_options(selection);
        if !listed.seen_peers.is_empty() {
            opts.initial_peers = Some(listed.seen_peers);
        }
        // Metadata in hand, so this add never waits on the swarm: it only
        // registers, persists and starts the torrent.
        let resp = self
            .session
            .add_torrent(AddTorrent::TorrentFileBytes(listed.torrent_bytes), Some(opts))
            .await?;
        let handle = resp.into_handle().context("add_torrent returned list-only")?;
        self.reject_if_unconfined(&handle).await?;
        self.stamp_added_if_new(&Self::info_hash_hex(&handle));
        Ok(handle)
    }

    /// Resolve a source's metadata without creating a torrent (librqbit's
    /// `list_only`), bounded by [`Engine::resolve_timeout`].
    ///
    /// librqbit waits for a magnet's metadata with no deadline (its
    /// `resolve_magnet` polls the DHT/tracker peer stream until a peer
    /// delivers), so a magnet nobody can serve used to hang its request
    /// forever. Giving up here is clean by construction: a `list_only` add
    /// never registers anything in the session, and dropping it drops the
    /// DHT search (its peer stream aborts its task on drop), the tracker
    /// announces and the metadata connections (all owned by the future, none
    /// spawned). A `.torrent` blob carries its metadata and resolves at once.
    async fn resolve_metadata(
        &self,
        add: AddTorrent<'_>,
    ) -> anyhow::Result<librqbit::ListOnlyResponse> {
        let list_only = librqbit::AddTorrentOptions { list_only: true, ..Default::default() };
        let resolved =
            tokio::time::timeout(self.resolve_timeout, self.session.add_torrent(add, Some(list_only)))
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "no peer delivered the torrent's metadata within {:?}; nothing was added",
                        self.resolve_timeout
                    )
                })?
                .context("resolving the torrent's metadata")?;
        match resolved {
            librqbit::AddTorrentResponse::ListOnly(listed) => Ok(listed),
            _ => anyhow::bail!("bug: a list_only add created a torrent"),
        }
    }

    /// Tear the torrent back down (and its files) if it escapes the cache.
    async fn reject_if_unconfined(&self, handle: &Handle) -> anyhow::Result<()> {
        if let Err(e) = self.assert_confined(handle) {
            self.remove(&Self::info_hash_hex(handle)).await;
            return Err(e);
        }
        Ok(())
    }

    /// Get-or-create a torrent from a magnet URL (`POST /:ih/create`, and the
    /// auto-create behind [`Engine::get_or_create_for_file`]), downloading only
    /// `pick` if it is new. An already-managed torrent is returned as it is,
    /// selection untouched and WITHOUT re-adding (see
    /// [`Engine::get_or_create_for_file`] for why a re-add is harmful), which
    /// also skips a pointless metadata round trip to the swarm. Waits, bounded,
    /// for the hash check so files are available for index resolution.
    pub async fn add_magnet(&self, magnet: &str, pick: Pick<'_>) -> anyhow::Result<Handle> {
        let info_hash = librqbit::Magnet::parse(magnet)
            .context("not a valid magnet url")?
            .as_id20()
            .context("magnet has no BTv1 infohash")?
            .as_string();
        let handle = match self.get(&info_hash) {
            Some(handle) => handle,
            None => {
                // The defaults ride in the URI, the only channel librqbit reads
                // for a magnet; an addon magnet's own trackers are preserved and
                // ours are added on top, which is what the DEFAULT_TRACKERS doc
                // always claimed happened.
                let magnet = magnet_with_default_trackers(magnet);
                self.add_source(AddTorrent::from_url(magnet), pick).await?
            }
        };
        // Bounded wait: a magnet with no reachable peers must not hang the request.
        let _ = tokio::time::timeout(METADATA_TIMEOUT, handle.wait_until_initialized()).await;
        Ok(handle)
    }

    /// Get-or-create by infohash for PLAYING one file, and make sure that file
    /// is downloading ([`Engine::ensure_selected`]). Returns the handle and the
    /// resolved file index. Every playback entry point goes through here: the
    /// stream route, the rillio:// byte plane, and `/cache/download`.
    ///
    /// A new torrent is added with ONLY that file selected. A known index goes
    /// to librqbit directly ([`Pick::Files`], the common `/{ih}/7` case); one
    /// that needs the file list is decided from it ([`Pick::Decide`]), and if
    /// it names no file the add is refused outright,
    /// nothing is added and nothing downloads.
    ///
    /// Crucially, if the torrent is already managed it returns the LIVE handle
    /// without re-adding: the media player opens many connections per title
    /// (header read, mkv-index seek, read-ahead), and calling `add_torrent`
    /// again on a live torrent resets it to the `initializing` state
    /// (`overwrite: true` re-runs storage init), which makes the concurrent
    /// stream reads fail with "invalid state: initializing" and playback abort.
    pub async fn get_or_create_for_file(
        &self,
        info_hash: &str,
        file: FileRef<'_>,
    ) -> anyhow::Result<(Handle, usize)> {
        let handle = match self.get(info_hash) {
            Some(handle) => {
                // Already managed: make sure metadata is ready, but never re-add.
                let _ =
                    tokio::time::timeout(METADATA_TIMEOUT, handle.wait_until_initialized()).await;
                handle
            }
            None => {
                let magnet = format!("magnet:?xt=urn:btih:{info_hash}");
                let decide = |files: &[types::File]| file.resolve(files).map(|i| vec![i]);
                let pick = match &file {
                    FileRef::Index(i) => Pick::Files(vec![*i]),
                    FileRef::Resolve(_) => Pick::Decide(&decide),
                };
                self.add_magnet(&magnet, pick).await?
            }
        };
        let files = Self::files(&handle);
        if files.is_empty() {
            anyhow::bail!("{info_hash}: metadata not resolved (no files)");
        }
        let idx = file.resolve(&files).with_context(|| format!("{info_hash}"))?;
        // A no-op for the fresh add above (it already selected exactly idx);
        // real work for a torrent that was already managed.
        self.ensure_selected(&handle, idx).await?;
        Ok((handle, idx))
    }

    /// Lowercase hex infohash of a handle. Uses librqbit-core's stable
    /// `Id20::as_string` (`hex::encode` of the raw 20 bytes), NOT Debug
    /// formatting, since every `Engine::get`/`remove`/stats lookup keys off this.
    pub fn info_hash_hex(handle: &Handle) -> String {
        handle.info_hash().as_string()
    }

    /// Look up an already-managed torrent by infohash WITHOUT creating one.
    /// Stats routes use this: an unknown infohash yields `null`, not an add.
    pub fn get(&self, info_hash: &str) -> Option<Handle> {
        self.session.with_torrents(|it| {
            it.filter(|(_, h)| Self::info_hash_hex(h) == info_hash)
                .map(|(_, h)| h.clone())
                .next()
        })
    }

    /// Handles of all managed torrents (for the aggregate `/stats.json`).
    pub fn all(&self) -> Vec<Handle> {
        self.session.with_torrents(|it| it.map(|(_, h)| h.clone()).collect())
    }

    // delete_files: removing a torrent also deletes its cached files.
    //
    // The spec suggested keep-files (delete_files=false) to mirror the blob's
    // `destroy`, but librqbit 8.1.1 cannot re-add a torrent whose files still
    // exist in its output folder - the second add fails during init with
    // "setting length for file ...: file is None". Since remove→re-add is a real
    // flow, we delete files: a clean teardown that re-adds cleanly, and the
    // natural meaning of "remove" (free the cache) for a streaming server.
    const DELETE_FILES_ON_REMOVE: bool = true;

    /// Stop and forget a torrent by infohash (`GET /:ih/remove`). No-op if not
    /// managed. Returns whether one was found - the route responds `200 {}`
    /// either way (blob parity).
    pub async fn remove(&self, info_hash: &str) -> bool {
        let target = self.session.with_torrents(|it| {
            it.filter(|(_, h)| Self::info_hash_hex(h) == info_hash)
                .map(|(id, _)| id)
                .next()
        });
        if let Some(id) = target {
            let _ = self.session.delete(id.into(), Self::DELETE_FILES_ON_REMOVE).await;
            // A deleted torrent must not leave a stale pin behind (it would
            // shield a future re-add of the same infohash from eviction), nor a
            // stale watched mark (it would schedule a future re-add for
            // cleanup the moment it appears).
            self.set_pinned(info_hash, false);
            self.set_watched(info_hash, false);
            self.set_meta(info_hash, None);
            self.clear_added(info_hash);
            true
        } else {
            false
        }
    }

    /// Stop and forget every torrent (`GET /removeAll`).
    pub async fn remove_all(&self) {
        let ids: Vec<librqbit::api::TorrentIdOrHash> =
            self.session.with_torrents(|it| it.map(|(id, _)| id.into()).collect());
        for id in ids {
            let _ = self.session.delete(id, Self::DELETE_FILES_ON_REMOVE).await;
        }
    }

    /// The torrent's files as the wire `File` shape. Empty if metadata is not
    /// yet resolved (magnet still fetching).
    pub fn files(handle: &Handle) -> Vec<types::File> {
        handle
            .with_metadata(|m| {
                m.file_infos
                    .iter()
                    .map(|fi| wire_file(&fi.relative_filename, fi.len, fi.offset_in_torrent))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Build the `getStatistics` object (server.js:18294-18338). `idx`, when a
    /// valid file index, merges the per-file `stream*` fields. Swarm/peer
    /// counters are stubbed in M1 and made real in M2.
    pub fn statistics(
        &self,
        handle: &Handle,
        cache_path: String,
        peer_search: types::PeerSearch,
        idx: Option<usize>,
    ) -> types::Statistics {
        let files = Self::files(handle);
        let stats = handle.stats();
        // Echo the live torrent profile so the stats menu reflects the selected
        // profile (the download cap is real; the rest is report-only).
        let bt = self.bt_profile();
        // Live metrics: speeds (Speed.mbps is megabits/s; blob reports bytes/s,
        // ×125_000) and peer counts. `peers` = connected; `queued` = queued.
        let (download_speed, upload_speed, peers, queued) = stats
            .live
            .as_ref()
            .map(|l| {
                (
                    l.download_speed.mbps * 125_000.0,
                    l.upload_speed.mbps * 125_000.0,
                    l.snapshot.peer_stats.live as u64,
                    l.snapshot.peer_stats.queued as u64,
                )
            })
            .unwrap_or((0.0, 0.0, 0, 0));

        let (stream_len, stream_name, stream_progress) = match idx {
            Some(i) => files
                .get(i)
                .map(|f| {
                    let done = stats.file_progress.get(i).copied().unwrap_or(0);
                    let frac = if f.length > 0 {
                        done as f64 / f.length as f64
                    } else {
                        0.0
                    };
                    (f.length, f.name.clone(), frac)
                })
                .unwrap_or((0, String::new(), 0.0)),
            None => (0, String::new(), 0.0),
        };

        types::Statistics {
            name: handle.name().unwrap_or_default(),
            info_hash: Self::info_hash_hex(handle),
            files,
            sources: vec![],
            opts: types::Options {
                connections: Some(bt.max_connections),
                dht: false,
                growler: types::Growler {
                    flood: 0,
                    pulse: Some(bt.download_speed_hard_limit as u64),
                },
                handshake_timeout: Some(bt.handshake_timeout),
                path: cache_path,
                peer_search,
                swarm_cap: types::SwarmCap {
                    max_speed: Some(bt.download_speed_soft_limit),
                    min_peers: Some(bt.min_peers_for_stable),
                },
                timeout: Some(bt.request_timeout),
                tracker: false,
                r#virtual: true,
            },
            download_speed,
            upload_speed,
            downloaded: stats.progress_bytes,
            uploaded: stats.uploaded_bytes,
            unchoked: 0,
            peers,
            queued,
            unique: 0,
            connection_tries: 0,
            peer_search_running: false,
            stream_len,
            stream_name,
            stream_progress,
            swarm_connections: 0,
            swarm_paused: handle.is_paused(),
            swarm_size: 0,
            // "initializing" | "live" | "paused" | "error" (librqbit state),
            // plus the failure text (e.g. a disk-full write error) so the
            // player can explain a dead stream to the user.
            engine_state: format!("{:?}", stats.state).to_lowercase(),
            engine_error: stats.error.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use super::{
        magnet_with_default_trackers, mark_prefetch_in, read_listen_pref, write_listen_pref,
        DEFAULT_TRACKERS, TORRENT_PREFS_FILE,
    };

    // The contract that matters is not the string we build, it is what librqbit
    // parses back out of it - that is the step where opts.trackers was silently
    // dropped and left every infohash-only add on DHT alone.
    #[test]
    fn librqbit_parses_our_default_trackers_out_of_a_bare_magnet() {
        let bare = format!("magnet:?xt=urn:btih:{}", "a".repeat(40));
        assert!(
            librqbit::Magnet::parse(&bare)
                .unwrap()
                .trackers
                .is_empty(),
            "precondition: a bare magnet carries no trackers"
        );

        let parsed =
            librqbit::Magnet::parse(&magnet_with_default_trackers(&bare)).unwrap();
        assert_eq!(
            parsed.trackers,
            DEFAULT_TRACKERS.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            "percent-encoded &tr= params must decode back to the exact tracker urls"
        );
    }

    #[test]
    fn an_addon_magnets_own_trackers_survive_and_ours_are_added_on_top() {
        let magnet = format!(
            "magnet:?xt=urn:btih:{}&tr=udp%3A%2F%2Faddon.example%3A1337%2Fannounce",
            "b".repeat(40)
        );
        let parsed =
            librqbit::Magnet::parse(&magnet_with_default_trackers(&magnet)).unwrap();
        assert_eq!(parsed.trackers[0], "udp://addon.example:1337/announce");
        assert_eq!(parsed.trackers.len(), 1 + DEFAULT_TRACKERS.len());
    }

    #[test]
    fn mark_prefetch_dedups_per_infohash_and_file() {
        let set = Mutex::new(HashSet::new());
        // First claim of a pair wins.
        assert!(mark_prefetch_in(&set, "abc", 0));
        // Repeat of the same pair is refused.
        assert!(!mark_prefetch_in(&set, "abc", 0));
        // A different file in the same torrent is a distinct claim.
        assert!(mark_prefetch_in(&set, "abc", 1));
        assert!(!mark_prefetch_in(&set, "abc", 1));
        // A different torrent, same file index, is also distinct.
        assert!(mark_prefetch_in(&set, "def", 0));
        assert!(!mark_prefetch_in(&set, "def", 0));
    }

    /// Each test gets its own dir so parallel runs don't clobber the shared file.
    fn fresh_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rillio-torrent-prefs-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn absent_pref_is_quiet_default() {
        assert!(!read_listen_pref(&fresh_dir("absent")));
    }

    #[test]
    fn pref_roundtrips_both_ways() {
        let dir = fresh_dir("roundtrip");
        write_listen_pref(&dir, true).unwrap();
        assert!(read_listen_pref(&dir));
        write_listen_pref(&dir, false).unwrap();
        assert!(!read_listen_pref(&dir));
    }

    #[test]
    fn malformed_pref_falls_back_to_off() {
        let dir = fresh_dir("malformed");
        std::fs::write(dir.join(TORRENT_PREFS_FILE), b"{ not valid json").unwrap();
        assert!(!read_listen_pref(&dir));
    }

    // -----------------------------------------------------------------------
    // Stale-fastresume guard
    // -----------------------------------------------------------------------

    use super::{bitv_claims_all_pieces, invalidate_stale_fastresume};

    const IH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// Minimal valid single-file torrent metainfo (16 KiB pieces). Bencode
    /// dict keys are already in the required sorted order.
    fn single_file_torrent(name: &str, length: u64, num_pieces: usize) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"d4:infod6:lengthi");
        out.extend_from_slice(length.to_string().as_bytes());
        out.extend_from_slice(b"e4:name");
        out.extend_from_slice(format!("{}:{name}", name.len()).as_bytes());
        out.extend_from_slice(b"12:piece lengthi16384e6:pieces");
        out.extend_from_slice(format!("{}:", num_pieces * 20).as_bytes());
        out.extend(std::iter::repeat(0x11u8).take(num_pieces * 20));
        out.extend_from_slice(b"ee");
        out
    }

    /// Minimal valid two-file torrent metainfo (`a.bin` + `b.bin`, 16 KiB each).
    fn two_file_torrent() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(
            b"d4:infod5:filesl\
              d6:lengthi16384e4:pathl5:a.binee\
              d6:lengthi16384e4:pathl5:b.binee\
              e4:name3:dir12:piece lengthi16384e6:pieces40:",
        );
        out.extend(std::iter::repeat(0x11u8).take(40));
        out.extend_from_slice(b"ee");
        out
    }

    /// Fabricate a librqbit session dir: session.json + .torrent + .bitv.
    /// Returns (session_dir, output_folder, bitv_path).
    fn fake_session(
        tag: &str,
        torrent_bytes: &[u8],
        bitv: &[u8],
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let root = fresh_dir(&format!("fastresume-{tag}"));
        let session_dir = root.join("session");
        let out_dir = root.join("out");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&out_dir).unwrap();
        let db = serde_json::json!({
            "torrents": {
                "0": {
                    "info_hash": IH,
                    "trackers": [],
                    "output_folder": out_dir,
                    "only_files": null,
                    "is_paused": false,
                }
            }
        });
        std::fs::write(session_dir.join("session.json"), serde_json::to_vec(&db).unwrap())
            .unwrap();
        std::fs::write(session_dir.join(format!("{IH}.torrent")), torrent_bytes).unwrap();
        let bitv_path = session_dir.join(format!("{IH}.bitv"));
        std::fs::write(&bitv_path, bitv).unwrap();
        (session_dir, out_dir, bitv_path)
    }

    #[test]
    fn stale_bitv_over_fully_missing_files_is_invalidated() {
        // 3 pieces claimed complete (Msb0: top 3 bits), but no data file at all.
        let (session_dir, _out, bitv) =
            fake_session("missing", &single_file_torrent("file.bin", 40960, 3), &[0xE0]);
        invalidate_stale_fastresume(&session_dir);
        assert!(!bitv.exists(), "stale .bitv must be deleted");
        // Metadata and the session db survive so the torrent re-adds cleanly.
        assert!(session_dir.join(format!("{IH}.torrent")).exists());
        assert!(session_dir.join("session.json").exists());
    }

    #[test]
    fn bitv_over_real_data_is_kept() {
        let (session_dir, out, bitv) =
            fake_session("real", &single_file_torrent("file.bin", 40960, 3), &[0xE0]);
        std::fs::write(out.join("file.bin"), vec![0x42u8; 40960]).unwrap();
        invalidate_stale_fastresume(&session_dir);
        assert!(bitv.exists(), "fastresume over real data must survive");
    }

    #[test]
    fn complete_bitv_over_zero_filled_files_is_invalidated() {
        // The already-corrupted variant: librqbit preallocated zeros on a
        // previous run and the bitfield still claims 100%.
        let (session_dir, out, bitv) =
            fake_session("zeros", &single_file_torrent("file.bin", 40960, 3), &[0xE0]);
        std::fs::write(out.join("file.bin"), vec![0u8; 40960]).unwrap();
        invalidate_stale_fastresume(&session_dir);
        assert!(!bitv.exists(), "complete-but-all-zero fastresume must be deleted");
        // The data files are never touched, only the bitfield.
        assert!(out.join("file.bin").exists());
    }

    #[test]
    fn partial_bitv_over_zero_filled_file_is_kept() {
        // Claims only piece 0 of 3: a legit in-progress download whose data
        // happens to be zeros must NOT be nuked (the zero check requires a
        // complete bitfield).
        let (session_dir, out, bitv) =
            fake_session("partial", &single_file_torrent("file.bin", 40960, 3), &[0x80]);
        std::fs::write(out.join("file.bin"), vec![0u8; 40960]).unwrap();
        invalidate_stale_fastresume(&session_dir);
        assert!(bitv.exists());
    }

    #[test]
    fn empty_bitv_is_left_alone() {
        // All-zero bitfield claims nothing; missing files prove nothing.
        let (session_dir, _out, bitv) =
            fake_session("empty", &single_file_torrent("file.bin", 40960, 3), &[0x00]);
        invalidate_stale_fastresume(&session_dir);
        assert!(bitv.exists());
    }

    #[test]
    fn unverifiable_torrent_keeps_fastresume() {
        // Without the .torrent we cannot enumerate expected files: keep.
        let (session_dir, _out, bitv) =
            fake_session("noverify", &single_file_torrent("file.bin", 40960, 3), &[0xFF]);
        std::fs::remove_file(session_dir.join(format!("{IH}.torrent"))).unwrap();
        invalidate_stale_fastresume(&session_dir);
        assert!(bitv.exists());
    }

    #[test]
    fn partially_present_multi_file_torrent_is_kept() {
        // One of two files still exists: a partially-moved tree, not a fully
        // absent download. Conservative rule: keep.
        let (session_dir, out, bitv) = fake_session("multi", &two_file_torrent(), &[0x80]);
        std::fs::write(out.join("a.bin"), vec![0x42u8; 16384]).unwrap();
        invalidate_stale_fastresume(&session_dir);
        assert!(bitv.exists());
    }

    #[test]
    fn fully_missing_multi_file_torrent_is_invalidated() {
        let (session_dir, _out, bitv) = fake_session("multi-gone", &two_file_torrent(), &[0x80]);
        invalidate_stale_fastresume(&session_dir);
        assert!(!bitv.exists());
    }

    #[test]
    fn missing_session_json_is_a_noop() {
        let dir = fresh_dir("fastresume-fresh");
        // Must not panic or create anything on a fresh boot.
        invalidate_stale_fastresume(&dir.join("session"));
    }

    // -----------------------------------------------------------------------
    // Metadata resolution is bounded
    // -----------------------------------------------------------------------

    use super::{Engine, Pick};

    /// An engine that can never reach anything outside this machine: no DHT,
    /// no listener, no persistence, and a short resolve bound so a test does
    /// not wait out the production one.
    async fn offline_engine(tag: &str, resolve_timeout: std::time::Duration) -> Engine {
        let dir = fresh_dir(&format!("offline-{tag}"));
        let session = librqbit::Session::new_with_opts(
            dir.clone(),
            librqbit::SessionOptions {
                disable_dht: true,
                disable_dht_persistence: true,
                listen_port_range: None,
                enable_upnp_port_forwarding: false,
                persistence: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut engine = Engine::from_session(session, dir);
        engine.resolve_timeout = resolve_timeout;
        engine
    }

    /// A magnet for an infohash nobody has, whose only peer source is a UDP
    /// tracker that receives every announce and never answers: the shape of a
    /// real magnet whose swarm never delivers metadata. Returns the magnet, its
    /// infohash, and the socket (kept alive so the port stays silent, not
    /// refused).
    fn unanswerable_magnet(seed: u8) -> (String, String, std::net::UdpSocket) {
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = silent.local_addr().unwrap().port();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let ih = format!("{seed:02x}{nanos:038x}");
        let magnet = format!("magnet:?xt=urn:btih:{ih}&tr=udp%3A%2F%2F127.0.0.1%3A{port}%2Fannounce");
        (magnet, ih, silent)
    }

    /// librqbit resolves a magnet's metadata with no deadline, so a magnet whose
    /// peers never deliver it used to hang its request forever. Both add shapes
    /// (a known file index, and a selection decided from the file list) must
    /// fail loud within the bound and leave nothing behind in the session.
    #[tokio::test]
    async fn a_magnet_whose_metadata_never_arrives_fails_within_the_bound() {
        let bound = std::time::Duration::from_secs(2);
        let engine = offline_engine("resolve-timeout", bound).await;
        let decide = |_: &[crate::types::File]| Ok(vec![0]);
        for (seed, pick) in [(1u8, Pick::Files(vec![0])), (2u8, Pick::Decide(&decide))] {
            let (magnet, ih, _silent) = unanswerable_magnet(seed);
            let started = std::time::Instant::now();
            let outcome = tokio::time::timeout(
                bound * 10,
                engine.add_source(librqbit::AddTorrent::from_url(magnet), pick),
            )
            .await
            .unwrap_or_else(|_| panic!("the add of {ih} hung past 10x its {bound:?} bound"));
            let err = match outcome {
                Ok(handle) => panic!("an unresolvable magnet was added: {:?}", handle.info_hash()),
                Err(e) => format!("{e:#}"),
            };
            assert!(started.elapsed() >= bound, "gave up before the bound: {:?}", started.elapsed());
            assert!(err.contains("metadata"), "the error must say what timed out: {err}");
            assert!(engine.get(&ih).is_none(), "a timed-out add left {ih} in the session");
            assert!(engine.all().is_empty(), "a timed-out add left a torrent behind");
        }
    }

    #[test]
    fn bitv_claims_all_pieces_handles_partial_last_byte() {
        // 3 pieces in one byte: top 3 bits (Msb0).
        assert!(bitv_claims_all_pieces(&[0xE0], 3));
        // Pad bits beyond the piece count are ignored.
        assert!(bitv_claims_all_pieces(&[0xFF], 3));
        // Missing piece 2.
        assert!(!bitv_claims_all_pieces(&[0xC0], 3));
        // Exact multiple of 8.
        assert!(bitv_claims_all_pieces(&[0xFF], 8));
        assert!(!bitv_claims_all_pieces(&[0xFE], 8));
        // 9 pieces need two bytes.
        assert!(bitv_claims_all_pieces(&[0xFF, 0x80], 9));
        assert!(!bitv_claims_all_pieces(&[0xFF], 9));
        // Degenerate inputs claim nothing.
        assert!(!bitv_claims_all_pieces(&[], 1));
        assert!(!bitv_claims_all_pieces(&[0xFF], 0));
    }
}

