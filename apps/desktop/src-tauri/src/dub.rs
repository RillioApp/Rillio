//! The AI dub track: a replacement audio timeline served to mpv through
//! `rillio-dub://<key>` and produced ahead of the playhead by a worker.
//!
//! The timeline is one WAV on disk (`<app data>/generated-dubs/<key>.wav`,
//! s16 stereo 48 kHz) whose length is known up front from the player's
//! duration, so mpv can seek in it like any file. Windows of [`WINDOW_S`] are
//! produced in any order (ahead of the playhead first, then backfill); a read
//! that reaches an unproduced window BLOCKS until the worker fills it, which
//! is what keeps mpv waiting instead of hitting EOF. The reader publishes the
//! frame it wants, and the worker jumps there after a seek.
//!
//! The worker fills each window from the dub pipeline (`dubpipe.rs`: separate,
//! segment, transcribe, translate, synthesize, fit, mix), which stays resident
//! on the state across streams so the sidecars start once. The pass-through
//! bed (the original audio decoded into the timeline), the worker's first
//! form, remains as the bench knob that measured the playback seam (S1).
//!
//! Threading: [`TimelineSource`] runs on mpv's stream thread (blocking is
//! expected there, see `stream_cb.rs`); the worker is its own OS thread; the
//! commands are Tauri async commands that never block.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::autosync::PcmFormat;
use crate::dubpipe::{Pipeline, ProduceError};
use crate::stream_cb::{ByteSource, CancelFlag};

/// The scheme, without `://`. Registered with mpv under exactly this name.
pub const SCHEME: &str = "rillio-dub";
const FORMAT: PcmFormat = PcmFormat { rate: 48_000, channels: 2 };
const BYTES_PER_FRAME: u64 = 2 * FORMAT.channels as u64;
/// The unit the player waits on: a window plays only once it is complete,
/// so its cost is the hold at the pick and at every seek. 10 s keeps that
/// hold to seconds (the pipeline's own context, separation margins and the
/// turn lookahead, is independent of it: `dubpipe.rs`).
const WINDOW_S: f64 = 10.0;
const HEADER_LEN: u64 = 44;
/// How long a blocked read sleeps between checks of the cancel flag.
const WAIT_SLICE: Duration = Duration::from_millis(100);
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// mpv's decode of a window runs a packet or so past the requested length;
/// that much is trimmed. More than this is the wrong window, not a boundary.
const MAX_OVERSHOOT_S: f64 = 0.25;
const EVENT: &str = "dub";
/// mpv's open of the track probes the file's head (header plus the first
/// reads of window 0) INSIDE the core's playloop, so a blocking read there
/// freezes playback and, through the player lock, the whole shell (observed
/// live: the dub was selected while the worker was on a later window). Reads
/// inside this region never wait: an unproduced window 0 answers silence
/// there, which is never played because the switch waits for the playhead's
/// own window, and window 0 is backfilled like any other.
const PROBE_BYTES: u64 = 1 << 20;
/// How much of a resumed window's slot is read to tell kept audio from a
/// zeroed file (a quarter second: no dubbed window is silent that long from
/// its very start... a bed with music is never all zero bytes).
const RESUME_PROBE_BYTES: usize = 48_000;
/// A held player resumes only once this much dub is ready ahead of it (a
/// couple of windows): resuming on the first window alone had it holding
/// again seconds later, heard as choppy audio. The high watermark of a cache.
const RESUME_AHEAD_S: f64 = 2.0 * WINDOW_S;
/// The label mpv shows for the track.
const TRACK_TITLE: &str = "AI dub (English)";
const TRACK_LANG: &str = "eng";

pub fn format_url(key: &str) -> String {
    format!("{SCHEME}://{key}")
}

/// Parse `rillio-dub://<16 lowercase hex>` (the transcript cache key) and
/// NOTHING else, in the same spirit as `stream_cb::parse_url`.
pub fn parse_url(url: &str) -> Result<String, String> {
    let key = url
        .strip_prefix(SCHEME)
        .and_then(|r| r.strip_prefix("://"))
        .ok_or_else(|| format!("not a {SCHEME}:// url"))?;
    if key.len() != 16 || !key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err("dub key must be exactly 16 lowercase hex characters".into());
    }
    Ok(key.to_owned())
}

/// One stream's timeline: the file, what is produced, and who is waiting.
pub struct Timeline {
    path: PathBuf,
    total_frames: u64,
    state: Mutex<TimelineState>,
    produced_cv: Condvar,
    /// The frame the most recent reader positioned itself at: the worker's
    /// playhead hint after a seek (the player's own `time-pos` lags a seek).
    wanted_frame: AtomicU64,
    /// Bumped by [`interrupt_reads`]: a read parked at the frontier returns
    /// an error instead of holding mpv's demuxer thread through a seek.
    interrupt_epoch: AtomicU64,
    /// A read is parked at the frontier right now: the player is buffering
    /// the dub (mpv's own paused-for-cache never reflects it, S1).
    reader_waiting: std::sync::atomic::AtomicBool,
    /// mpv does NOT hold playback when an already-playing external audio
    /// track stalls (observed live: the video ran on for minutes with no dub
    /// audio while the reader was parked), so the shell pauses the player
    /// itself while a read is parked and resumes it when the audio lands.
    /// `None` in tests (no player).
    app: Option<AppHandle>,
    /// The pause was ours: resume when served (a viewer's own pause stays).
    held_playback: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct TimelineState {
    produced: Vec<bool>,
    retry_after: BTreeMap<u32, Instant>,
    /// Set when the worker gave up for good; readers stop waiting.
    failed: Option<String>,
}

impl Timeline {
    fn window_frames() -> u64 {
        (WINDOW_S * FORMAT.rate as f64) as u64
    }

    /// Open the timeline for a stream: the WAV and its window map from a
    /// previous watch are kept when they describe the same duration (the dub
    /// of a title watched before plays at once and only its gaps are made);
    /// otherwise a fresh sparse file.
    fn create(path: PathBuf, duration_s: f64, app: Option<AppHandle>) -> Result<Arc<Timeline>, String> {
        let total_frames = (duration_s * FORMAT.rate as f64).ceil() as u64;
        let windows = ((total_frames + Self::window_frames() - 1) / Self::window_frames()) as usize;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("dub: creating {}: {e}", dir.display()))?;
        }
        let expected_len = HEADER_LEN + total_frames * BYTES_PER_FRAME;
        let mut produced = vec![false; windows];
        let kept = match (std::fs::metadata(&path), load_window_map(&map_path(&path))) {
            (Ok(meta), Some(map)) if meta.len() == expected_len && map.total_frames == total_frames && map.window_frames == Self::window_frames() => {
                // Trust the map only where the file agrees: a window whose
                // slot reads as digital silence was not kept (observed live:
                // a resumed map over a zeroed file played silence for 20
                // minutes), so it is made again.
                let mut file = File::open(&path).map_err(|e| format!("dub: opening {}: {e}", path.display()))?;
                let mut probe = vec![0u8; RESUME_PROBE_BYTES];
                for w in map.produced {
                    let Some(slot) = produced.get_mut(w as usize) else { continue };
                    let offset = HEADER_LEN + w as u64 * Self::window_frames() * BYTES_PER_FRAME;
                    let has_audio = file
                        .seek(SeekFrom::Start(offset))
                        .and_then(|_| file.read(&mut probe))
                        .map(|n| probe[..n].iter().any(|&b| b != 0))
                        .unwrap_or(false);
                    *slot = has_audio;
                }
                true
            }
            _ => false,
        };
        if kept {
            tracing::info!("dub: resuming {} with {} of {windows} windows made", path.display(), produced.iter().filter(|&&p| p).count());
        } else {
            let mut file = File::create(&path).map_err(|e| format!("dub: creating {}: {e}", path.display()))?;
            file.write_all(&wav_header(total_frames)).map_err(|e| format!("dub: header: {e}"))?;
            // Unproduced regions read as silence (a sparse file, no write cost).
            file.set_len(expected_len).map_err(|e| format!("dub: sizing: {e}"))?;
            let _ = std::fs::remove_file(map_path(&path));
        }
        Ok(Arc::new(Timeline {
            path,
            total_frames,
            state: Mutex::new(TimelineState { produced, ..Default::default() }),
            produced_cv: Condvar::new(),
            wanted_frame: AtomicU64::new(u64::MAX),
            interrupt_epoch: AtomicU64::new(0),
            reader_waiting: std::sync::atomic::AtomicBool::new(false),
            app,
            held_playback: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    /// The reader parked (`true`) or was served (`false`). Only bookkeeping:
    /// mpv's read-ahead parks at the frontier constantly without the playhead
    /// being anywhere near it (S1), so parking says nothing about what the
    /// viewer hears; the hold is decided on the PLAYHEAD, see
    /// [`Timeline::hold_or_resume`].
    fn hold_playback(&self, parked: bool) {
        self.reader_waiting.store(parked, Ordering::Relaxed);
    }

    /// The viewer's hold, on the periodic tick: the dub track is playing and
    /// the playhead sits in audio not made yet -> pause the player (mpv
    /// itself runs on with silence when an external audio track stalls);
    /// held and [`RESUME_AHEAD_S`] of dub ready ahead (or nothing more will
    /// come) -> resume. A viewer's own pause is never touched: only a pause
    /// we set is lifted.
    fn hold_or_resume(&self, time_s: f64, track_selected: bool) {
        let Some(app) = &self.app else { return };
        let frame = (time_s.max(0.0) * FORMAT.rate as f64) as u64;
        let held = self.held_playback.load(Ordering::Relaxed);
        let over = self.state.lock().map(|s| s.failed.is_some()).unwrap_or(true) || self.complete();
        let starving = track_selected && !over && !self.is_produced(self.window_of_frame(frame));
        self.reader_waiting.store(starving, Ordering::Relaxed);
        if !held && starving && crate::shell::player_prop_bool(app, "pause") == Some(false) {
            if crate::shell::player_command(app, &["set", "pause", "yes"]).is_ok() {
                self.held_playback.store(true, Ordering::Relaxed);
                tracing::info!("dub: playback held at {time_s:.1} s, nothing dubbed there yet");
            }
        } else if held && (over || self.ahead_s(time_s) >= RESUME_AHEAD_S) {
            self.held_playback.store(false, Ordering::Relaxed);
            match crate::shell::player_command(app, &["set", "pause", "no"]) {
                Ok(()) => tracing::info!("dub: playback resumed with {:.1} s ready", self.ahead_s(time_s)),
                Err(e) => tracing::warn!("dub: resuming playback: {e}"),
            }
        }
    }

    /// Where the reader last positioned itself, if it has read at all.
    fn reader_frame(&self) -> Option<u64> {
        match self.wanted_frame.load(Ordering::Relaxed) {
            u64::MAX => None,
            frame => Some(frame),
        }
    }

    fn windows(&self) -> u32 {
        self.state.lock().map(|s| s.produced.len() as u32).unwrap_or(0)
    }

    fn produced_count(&self) -> u32 {
        self.state.lock().map(|s| s.produced.iter().filter(|&&p| p).count() as u32).unwrap_or(0)
    }

    fn is_produced(&self, window: u32) -> bool {
        self.state.lock().map(|s| s.produced.get(window as usize).copied().unwrap_or(false)).unwrap_or(false)
    }

    /// Seconds of produced audio from `time_s` forward, through consecutive
    /// produced windows (the rest of the current window counts in full).
    fn ahead_s(&self, time_s: f64) -> f64 {
        let first = self.window_of_frame((time_s.max(0.0) * FORMAT.rate as f64) as u64);
        let Ok(state) = self.state.lock() else { return 0.0 };
        let run = state.produced.iter().skip(first as usize).take_while(|&&p| p).count();
        if run == 0 {
            return 0.0;
        }
        let end_s = ((first as usize + run) as f64 * WINDOW_S).min(self.total_frames as f64 / FORMAT.rate as f64);
        (end_s - time_s).max(0.0)
    }

    fn size_bytes(&self) -> u64 {
        HEADER_LEN + self.total_frames * BYTES_PER_FRAME
    }

    fn window_of_frame(&self, frame: u64) -> u32 {
        (frame / Self::window_frames()) as u32
    }

    /// Write one window's interleaved samples at its slot (a short last window
    /// is padded with silence) and wake every reader parked on it.
    fn fill(&self, window: u32, pcm: &[i16]) -> Result<(), String> {
        let slot_frames = Self::window_frames().min(self.total_frames.saturating_sub(window as u64 * Self::window_frames()));
        let slot_samples = (slot_frames * FORMAT.channels as u64) as usize;
        let max_overshoot = (MAX_OVERSHOOT_S * FORMAT.rate as f64) as usize * FORMAT.channels as usize;
        if pcm.len() > slot_samples + max_overshoot {
            return Err(format!("dub: window {window}: {} samples exceed the slot of {slot_samples}", pcm.len()));
        }
        let pcm = &pcm[..pcm.len().min(slot_samples)];
        let mut bytes = Vec::with_capacity(slot_samples * 2);
        for s in pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        bytes.resize(slot_samples * 2, 0);
        let mut file = OpenOptions::new().write(true).open(&self.path).map_err(|e| format!("dub: opening {}: {e}", self.path.display()))?;
        file.seek(SeekFrom::Start(HEADER_LEN + window as u64 * Self::window_frames() * BYTES_PER_FRAME))
            .and_then(|_| file.write_all(&bytes))
            .map_err(|e| format!("dub: writing window {window}: {e}"))?;
        let made: Vec<u32> = {
            let mut state = self.state.lock().map_err(|_| "dub: poisoned")?;
            if let Some(slot) = state.produced.get_mut(window as usize) {
                *slot = true;
            }
            state.produced.iter().enumerate().filter(|(_, &p)| p).map(|(i, _)| i as u32).collect()
        };
        self.produced_cv.notify_all();
        // The window map next to the WAV, so a later watch resumes here.
        save_window_map(&map_path(&self.path), &WindowMap { total_frames: self.total_frames, window_frames: Self::window_frames(), produced: made })?;
        Ok(())
    }

    fn fail(&self, reason: String) {
        if let Ok(mut state) = self.state.lock() {
            state.failed = Some(reason);
        }
        self.produced_cv.notify_all();
    }

    fn complete(&self) -> bool {
        self.state.lock().map(|s| s.produced.iter().all(|&p| p)).unwrap_or(false)
    }

    /// The next window to produce: forward from `first`, then backward.
    fn next_window(&self, first: u32) -> Option<u32> {
        let state = self.state.lock().ok()?;
        let now = Instant::now();
        let wanted = |w: &u32| {
            !state.produced[*w as usize] && state.retry_after.get(w).map_or(true, |&at| at <= now)
        };
        let end = state.produced.len() as u32;
        (first.min(end)..end).find(wanted).or_else(|| (0..first.min(end)).rev().find(wanted))
    }

    fn retry_later(&self, window: u32) {
        if let Ok(mut state) = self.state.lock() {
            state.retry_after.insert(window, Instant::now() + RETRY_AFTER);
        }
    }

    /// Block until every window covering `[from, to)` bytes of the data
    /// chunk is produced. `Err` when the timeline failed or the read was
    /// cancelled by mpv.
    fn wait_produced(&self, first_frame: u64, last_frame: u64, cancel: &CancelFlag) -> Result<(), String> {
        let (w0, w1) = (self.window_of_frame(first_frame), self.window_of_frame(last_frame));
        self.wanted_frame.store(first_frame, Ordering::Relaxed);
        let epoch = self.interrupt_epoch.load(Ordering::SeqCst);
        let mut state = self.state.lock().map_err(|_| "dub: poisoned")?;
        loop {
            if let Some(reason) = &state.failed {
                return Err(format!("dub: timeline failed: {reason}"));
            }
            let ready = (w0..=w1).all(|w| state.produced.get(w as usize).copied().unwrap_or(true));
            if ready {
                return Ok(());
            }
            if cancel.is_cancelled() {
                return Err("dub: read cancelled while waiting for the worker".into());
            }
            if self.interrupt_epoch.load(Ordering::SeqCst) != epoch {
                // mpv is seeking: it reads again from the new position, so a
                // one-off read error here costs nothing and frees its thread.
                return Err("dub: read interrupted by a seek".into());
            }
            state = self.produced_cv.wait_timeout(state, WAIT_SLICE).map_err(|_| "dub: poisoned")?.0;
        }
    }
}

/// `<key>.json` next to `<key>.wav`: which windows the WAV holds.
#[derive(Serialize, serde::Deserialize)]
struct WindowMap {
    total_frames: u64,
    /// The window size the indices count in; a map from another size is stale.
    window_frames: u64,
    produced: Vec<u32>,
}

fn map_path(wav: &std::path::Path) -> PathBuf {
    wav.with_extension("json")
}

fn load_window_map(path: &std::path::Path) -> Option<WindowMap> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_window_map(path: &std::path::Path, map: &WindowMap) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec(map).map_err(|e| format!("dub: window map: {e}"))?;
    std::fs::write(&tmp, json).map_err(|e| format!("dub: writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("dub: replacing {}: {e}", path.display()))
}

fn wav_header(total_frames: u64) -> [u8; HEADER_LEN as usize] {
    let data = (total_frames * BYTES_PER_FRAME).min(u32::MAX as u64) as u32;
    let mut h = [0u8; HEADER_LEN as usize];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&FORMAT.channels.to_le_bytes());
    h[24..28].copy_from_slice(&FORMAT.rate.to_le_bytes());
    h[28..32].copy_from_slice(&(FORMAT.rate * BYTES_PER_FRAME as u32).to_le_bytes());
    h[32..34].copy_from_slice(&(BYTES_PER_FRAME as u16).to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

/// What mpv reads: the header, then the timeline file, blocking at the
/// production frontier.
struct TimelineSource {
    timeline: Arc<Timeline>,
    file: File,
    pos: u64,
    cancel: Arc<CancelFlag>,
}

impl ByteSource for TimelineSource {
    fn size(&mut self) -> Option<u64> {
        Some(self.timeline.size_bytes())
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        let size = self.timeline.size_bytes();
        if self.pos >= size || buf.is_empty() {
            return Ok(0);
        }
        let want = (buf.len() as u64).min(size - self.pos) as usize;
        if self.pos < HEADER_LEN {
            let header = wav_header(self.timeline.total_frames);
            let n = want.min((HEADER_LEN - self.pos) as usize);
            buf[..n].copy_from_slice(&header[self.pos as usize..self.pos as usize + n]);
            self.pos += n as u64;
            return Ok(n);
        }
        let first_frame = (self.pos - HEADER_LEN) / BYTES_PER_FRAME;
        if self.pos < HEADER_LEN + PROBE_BYTES && !self.timeline.is_produced(self.timeline.window_of_frame(first_frame)) {
            let n = want.min((HEADER_LEN + PROBE_BYTES - self.pos) as usize);
            buf[..n].fill(0);
            self.pos += n as u64;
            return Ok(n);
        }
        let last_frame = (self.pos - HEADER_LEN + want as u64 - 1) / BYTES_PER_FRAME;
        let t0 = Instant::now();
        let parked = !self.timeline.is_produced(self.timeline.window_of_frame(last_frame));
        if parked {
            self.timeline.hold_playback(true);
        }
        let waited_result = self.timeline.wait_produced(first_frame, last_frame, &self.cancel);
        if parked {
            self.timeline.hold_playback(false);
        }
        waited_result?;
        let waited = t0.elapsed();
        if waited > WAIT_SLICE {
            tracing::debug!(
                "dub: read of {want} bytes at {:.1}s waited {} ms for window {}",
                first_frame as f64 / FORMAT.rate as f64,
                waited.as_millis(),
                self.timeline.window_of_frame(last_frame)
            );
        }
        self.file.seek(SeekFrom::Start(self.pos)).map_err(|e| format!("dub: seek: {e}"))?;
        let n = self.file.read(&mut buf[..want]).map_err(|e| format!("dub: read: {e}"))?;
        self.pos += n as u64;
        Ok(n)
    }

    fn seek(&mut self, pos: u64) -> Result<u64, String> {
        self.pos = pos.min(self.timeline.size_bytes());
        if self.pos >= HEADER_LEN {
            let frame = (self.pos - HEADER_LEN) / BYTES_PER_FRAME;
            self.timeline.wanted_frame.store(frame, Ordering::Relaxed);
            tracing::debug!("dub: mpv seeks the track to {:.1}s", frame as f64 / FORMAT.rate as f64);
        }
        Ok(self.pos)
    }

    fn canceller(&self) -> Option<Arc<CancelFlag>> {
        Some(self.cancel.clone())
    }
}

/// The dub for the current stream (one at a time, like the transcript).
#[derive(Default)]
pub struct DubState(Arc<Mutex<Inner>>);

/// The player's position as its own event loop last reported it. The worker
/// reads THIS, never the player state (whose lock is held across mpv
/// commands: a worker waiting on it while a command waits on the worker's
/// timeline is a deadlock, observed live).
static PLAYHEAD_MS: AtomicU64 = AtomicU64::new(0);

/// While a worker runs, the web hears how much is ready ahead of the
/// playhead this often, and a held player is given its chance to resume (a
/// window completes only every 10-40 s, too rarely for the row's "N s
/// ready"; and `time-pos` stops changing while the player is held, so this
/// cannot ride on its events).
const TICK_EVERY: Duration = Duration::from_secs(1);
static TICKER_STARTED: std::sync::Once = std::sync::Once::new();

/// Called by the shell's mpv event loop on every `time-pos` change.
pub(crate) fn note_playhead(seconds: f64) {
    if seconds.is_finite() && seconds >= 0.0 {
        PLAYHEAD_MS.store((seconds * 1000.0) as u64, Ordering::Relaxed);
    }
}

/// One tick: resume a held player when enough is ready, and tell the web.
fn tick(app: &AppHandle) {
    let (url, timeline, phase, added) = {
        let state = app.state::<DubState>();
        let Ok(inner) = state.0.lock() else { return };
        if !inner.worker_alive {
            return;
        }
        match (&inner.url, &inner.timeline, inner.phase) {
            (Some(url), Some(timeline), Some(phase)) => (url.clone(), timeline.clone(), phase, inner.added),
            _ => return,
        }
    };
    timeline.hold_or_resume(playhead_s(), added);
    status(app, &url, &timeline, phase, None);
}

fn start_ticker(app: &AppHandle) {
    let app = app.clone();
    TICKER_STARTED.call_once(move || {
        std::thread::Builder::new()
            .name("dub-tick".into())
            .spawn(move || loop {
                std::thread::sleep(TICK_EVERY);
                tick(&app);
            })
            .map(|_| ())
            .unwrap_or_else(|e| tracing::error!("dub: ticker: {e}"));
    });
}

fn set_phase(arc: &Arc<Mutex<Inner>>, phase: &'static str) {
    if let Ok(mut inner) = arc.lock() {
        inner.phase = Some(phase);
    }
}

fn playhead_s() -> f64 {
    PLAYHEAD_MS.load(Ordering::Relaxed) as f64 / 1000.0
}

#[derive(Default)]
struct Inner {
    url: Option<String>,
    key: Option<String>,
    timeline: Option<Arc<Timeline>>,
    generation: u64,
    worker_alive: bool,
    /// The track has been added to the player for this url.
    added: bool,
    /// Sidecars and models, opened once and kept across streams.
    pipeline: Option<Arc<Mutex<Pipeline>>>,
    /// The worker's phase as last announced ("preparing" while the sidecars
    /// start, "running" after): the periodic status repeats it.
    phase: Option<&'static str>,
    /// The title's name and synopsis (the player's metadata), for the models.
    about: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum Event {
    #[serde(rename_all = "camelCase")]
    Status {
        url: String,
        state: String,
        detail: Option<String>,
        produced: u32,
        windows: u32,
        /// Seconds of dubbed audio ready from the player's position forward.
        ahead_s: f64,
        /// mpv is parked on audio not made yet (the player is buffering the dub).
        waiting: bool,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DubInfo {
    /// What to hand mpv.
    track_url: String,
    produced: u32,
    windows: u32,
}

/// The player is about to seek: release any read parked at the production
/// frontier, or mpv's demuxer thread stays held on a window the worker has
/// moved away from and the seek never completes (observed live).
pub(crate) fn interrupt_reads(app: &AppHandle) {
    let state = app.state::<DubState>();
    let Ok(inner) = state.0.lock() else { return };
    if let Some(timeline) = &inner.timeline {
        timeline.interrupt_epoch.fetch_add(1, Ordering::SeqCst);
        timeline.produced_cv.notify_all();
    }
}

/// Opens `rillio-dub://<key>` for mpv: the timeline must exist for that key.
pub(crate) fn open(app: &AppHandle, url: &str) -> Result<Box<dyn ByteSource>, String> {
    let key = parse_url(url)?;
    let timeline = {
        let state = app.state::<DubState>();
        let inner = state.0.lock().map_err(|_| "dub: poisoned")?;
        match (&inner.key, &inner.timeline) {
            (Some(k), Some(t)) if *k == key => t.clone(),
            _ => return Err(format!("no dub timeline for key {key}")),
        }
    };
    let file = File::open(&timeline.path).map_err(|e| format!("dub: opening {}: {e}", timeline.path.display()))?;
    Ok(Box::new(TimelineSource { timeline, file, pos: 0, cancel: Arc::new(CancelFlag::new()) }))
}

/// Start (or resume) producing the dub for `url`; returns the track url to
/// add to the player. Idempotent for the same url while the worker is alive.
#[tauri::command]
pub async fn dub_start(app: AppHandle, state: State<'_, DubState>, url: String, about: Option<String>) -> Result<DubInfo, String> {
    let url = crate::thumbs::resolve_shadow_url(&app, &url)?;
    crate::thumbs::validate_url(&url)?;
    let duration = crate::shell::player_duration(&app).ok_or("dub: the player reports no duration yet")?;
    let key = crate::transcribe::cache_key(&url);
    let arc = state.0.clone();
    let (spawn, generation, timeline) = {
        let mut inner = arc.lock().map_err(|_| "dub: poisoned")?;
        if inner.url.as_deref() != Some(url.as_str()) {
            let path = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("dub: app data dir: {e}"))?
                .join("generated-dubs")
                .join(format!("{key}.wav"));
            inner.timeline = Some(Timeline::create(path, duration, Some(app.clone()))?);
            inner.url = Some(url.clone());
            inner.key = Some(key.clone());
            inner.about = about.clone();
            inner.added = false;
            inner.generation += 1;
            inner.worker_alive = false;
        }
        let spawn = !inner.worker_alive;
        if spawn {
            inner.generation += 1;
            inner.worker_alive = true;
        }
        (spawn, inner.generation, inner.timeline.clone().ok_or("dub: no timeline")?)
    };
    if spawn && !timeline.complete() {
        start_ticker(&app);
        crate::transcribe::yield_to_dub(&app, &url);
        let (app2, arc2, url2, timeline2) = (app.clone(), arc.clone(), url.clone(), timeline.clone());
        std::thread::Builder::new()
            .name("dub".into())
            .spawn(move || worker(app2, arc2, generation, url2, timeline2))
            .map_err(|e| format!("dub: spawn: {e}"))?;
    } else if spawn {
        if let Ok(mut inner) = arc.lock() {
            inner.worker_alive = false;
        }
    }
    Ok(DubInfo { track_url: format_url(&key), produced: timeline.produced_count(), windows: timeline.windows() })
}

/// Add the dub as an audio track and select it (the web switches back with
/// `aid`, which is allowlisted). The command is issued by the shell, so
/// `audio-add` stays out of the web's mpv allowlist.
/// Add the dub as an audio track and select it, at the pick: from here mpv
/// reads the timeline at the playhead and HOLDS on audio not made yet, like
/// a slow cache, resuming the moment the worker fills it (S1). The head of
/// the file never blocks (mpv's open probes it inside the core), so adding
/// early is safe; the hold is the buffering the web reports.
#[tauri::command]
pub async fn dub_select(app: AppHandle, state: State<'_, DubState>) -> Result<bool, String> {
    let (track_url, already) = {
        let mut inner = state.0.lock().map_err(|_| "dub: poisoned")?;
        let key = inner.key.clone().ok_or("dub: nothing started")?;
        let already = inner.added;
        inner.added = true;
        (format_url(&key), already)
    };
    if already {
        return Ok(true);
    }
    crate::shell::player_command(&app, &["audio-add", &track_url, "select", TRACK_TITLE, TRACK_LANG])?;
    Ok(true)
}

/// Whether a dub worker is producing for `url` right now: while it is, it
/// transcribes the dialogue itself and the subtitle transcriber stays off
/// (two whisper instances on one machine starved each other, observed live).
pub(crate) fn is_active_for(app: &AppHandle, url: &str) -> bool {
    let state = app.state::<DubState>();
    state.0.lock().map(|inner| inner.worker_alive && inner.url.as_deref() == Some(url)).unwrap_or(false)
}

/// Stop producing (the timeline keeps what it has).
#[tauri::command]
pub async fn dub_stop(app: AppHandle, state: State<'_, DubState>) -> Result<(), String> {
    let url = {
        let mut inner = state.0.lock().map_err(|_| "dub: poisoned")?;
        inner.generation += 1;
        inner.worker_alive = false;
        inner.url.clone()
    };
    if let Some(url) = url {
        crate::transcribe::resume_after_dub(&app, &url);
    }
    Ok(())
}

fn status(app: &AppHandle, url: &str, timeline: &Timeline, state: &str, detail: Option<String>) {
    let time_s = playhead_s();
    let event = Event::Status {
        url: url.to_owned(),
        state: state.into(),
        detail,
        produced: timeline.produced_count(),
        windows: timeline.windows(),
        ahead_s: timeline.ahead_s(time_s),
        waiting: timeline.reader_waiting.load(Ordering::Relaxed),
    };
    if let Err(e) = app.emit(EVENT, event) {
        tracing::warn!("dub: emit failed: {e}");
    }
}

fn current_generation(arc: &Arc<Mutex<Inner>>) -> u64 {
    arc.lock().map(|inner| inner.generation).unwrap_or(u64::MAX)
}

/// Bench knob: the least seconds one window may take, so the pass-through
/// worker can stand in for the real pipeline's cost (RTF 0.5 = 15 s per
/// window) while the playback seam is measured. Unset in production.
const SLOW_WINDOW_ENV: &str = "RILLIO_DUB_MIN_WINDOW_S";
/// Bench knob: fill the timeline with the original audio instead of the dub
/// (the S1 playback-seam harness). Unset in production.
const PASSTHROUGH_ENV: &str = "RILLIO_DUB_PASSTHROUGH";

/// The state's pipeline, opened on first use (sidecars started, models
/// loaded: tens of seconds, on the worker's thread).
fn resident_pipeline(app: &AppHandle, arc: &Arc<Mutex<Inner>>) -> Result<Arc<Mutex<Pipeline>>, String> {
    if let Some(pipeline) = arc.lock().map_err(|_| "dub: poisoned")?.pipeline.clone() {
        return Ok(pipeline);
    }
    let pack_dir = crate::packs::pack_dir(app)?;
    let log_dir = app.path().app_log_dir().map_err(|e| format!("dub: log dir: {e}"))?.join("dub");
    let pipeline = Arc::new(Mutex::new(Pipeline::open(&pack_dir, &log_dir)?));
    arc.lock().map_err(|_| "dub: poisoned")?.pipeline = Some(pipeline.clone());
    Ok(pipeline)
}

/// Produce windows until the timeline is complete or a later start/stop
/// supersedes this generation.
fn worker(app: AppHandle, arc: Arc<Mutex<Inner>>, generation: u64, url: String, timeline: Arc<Timeline>) {
    let min_window = std::env::var(SLOW_WINDOW_ENV).ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let passthrough = std::env::var(PASSTHROUGH_ENV).is_ok_and(|v| !v.is_empty());
    let pipeline = if passthrough {
        None
    } else {
        set_phase(&arc, "preparing");
        status(&app, &url, &timeline, "preparing", None);
        match resident_pipeline(&app, &arc) {
            Ok(pipeline) => {
                let about = arc.lock().ok().and_then(|inner| inner.about.clone());
                if let Ok(mut pipeline) = pipeline.lock() {
                    pipeline.set_about(about);
                }
                Some(pipeline)
            }
            Err(e) => {
                tracing::error!("{e}");
                timeline.fail(e.clone());
                status(&app, &url, &timeline, "failed", Some(e));
                worker_ended(&app, &arc, generation, &url);
                return;
            }
        }
    };
    set_phase(&arc, "running");
    status(&app, &url, &timeline, "running", None);
    loop {
        if current_generation(&arc) != generation {
            break;
        }
        if timeline.complete() {
            status(&app, &url, &timeline, "done", None);
            break;
        }
        // The reader's position is what mpv is waiting on (it leads the
        // playhead after a seek and sits at the frontier otherwise); the
        // player's own position only seeds the order before the first read.
        // Before any read the original is still playing: a window the
        // playhead has already half crossed would be mostly gone by the time
        // it is produced (a window costs close to its own length), so the
        // worker starts on the next one and the dub gets ahead sooner.
        // The reader runs ahead of the playhead through everything already
        // made (mpv's cache is unbounded in time, S1), so it points at the
        // first gap ahead, which may be twenty minutes past the viewer. What
        // the viewer needs first is the gap nearest the playhead; the reader
        // wins only right after a seek, when the playhead still lags it.
        let playhead = (playhead_s() * FORMAT.rate as f64) as u64;
        let frame = match timeline.reader_frame() {
            Some(reader) if reader <= playhead + 2 * Timeline::window_frames() => reader,
            Some(_) => playhead,
            None => {
                let into_window = playhead % Timeline::window_frames();
                if into_window > Timeline::window_frames() / 2 { playhead + Timeline::window_frames() } else { playhead }
            }
        };
        let first = timeline.window_of_frame(frame).min(timeline.windows().saturating_sub(1));
        let Some(window) = timeline.next_window(first) else {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        };
        let start_s = window as f64 * WINDOW_S;
        let t0 = Instant::now();
        let produced = match &pipeline {
            Some(pipeline) => {
                let mut lines = Vec::new();
                let result = match pipeline.lock() {
                    Ok(mut pipeline) => pipeline.produce(&url, start_s, WINDOW_S, &mut lines),
                    Err(_) => Err(ProduceError::Failed("dub: the pipeline is poisoned".into())),
                };
                if !lines.is_empty() {
                    tracing::info!("dub: window {window}: {} lines, {}", lines.len(), lines.iter().map(|l| l.fit.split(' ').next().unwrap_or("")).collect::<Vec<_>>().join(" "));
                    // The dub's English lines ARE the AI subtitles while it runs
                    // (Michael: subtitles follow the generated dub, not the
                    // source transcript); transcribe.rs stays idle meanwhile.
                    // A turn that kept its original voice has no line.
                    let segments = lines
                        .iter()
                        .filter(|l| !l.en.is_empty())
                        .map(|l| crate::transcribe::Segment::new(l.start_ms as i64, l.end_ms as i64, l.en.clone()))
                        .collect();
                    crate::transcribe::emit_segments(&app, &url, Some(crate::dubpipe::TARGET_LANGUAGE_CODE.to_owned()), segments);
                }
                result
            }
            None => crate::autosync::decode_pcm_as(&url, start_s, WINDOW_S, FORMAT).map_err(ProduceError::NotReady),
        };
        if min_window > 0.0 {
            let budget = Duration::from_secs_f64(min_window);
            if let Some(rest) = budget.checked_sub(t0.elapsed()) {
                std::thread::sleep(rest);
            }
        }
        match produced {
            Ok(pcm) => match timeline.fill(window, &pcm) {
                Ok(()) => {
                    tracing::info!("dub: window {window} ({:.1}s of audio) in {} ms", pcm.len() as f64 / (FORMAT.rate * FORMAT.channels as u32) as f64, t0.elapsed().as_millis());
                    status(&app, &url, &timeline, "running", None);
                }
                Err(e) => {
                    tracing::error!("{e}");
                    timeline.fail(e.clone());
                    status(&app, &url, &timeline, "failed", Some(e));
                    break;
                }
            },
            Err(ProduceError::NotReady(e)) => {
                // Typically a torrent region that is not downloaded yet; the
                // player itself will stall there too, so waiting is right.
                tracing::debug!("dub: window {window} not ready: {e}");
                timeline.retry_later(window);
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(ProduceError::Failed(e)) => {
                tracing::error!("dub: window {window}: {e}");
                timeline.fail(e.clone());
                status(&app, &url, &timeline, "failed", Some(e));
                break;
            }
        }
    }
    worker_ended(&app, &arc, generation, &url);
}

fn release_worker(arc: &Arc<Mutex<Inner>>, generation: u64) {
    if let Ok(mut inner) = arc.lock() {
        if inner.generation == generation {
            inner.worker_alive = false;
        }
    }
}

/// The worker ended on its own (done or failed): a yielded subtitle
/// transcription gets the CPU back.
fn worker_ended(app: &AppHandle, arc: &Arc<Mutex<Inner>>, generation: u64, url: &str) {
    release_worker(arc, generation);
    // A player we held must not stay paused for a worker that is gone.
    let (timeline, added) = arc.lock().ok().map(|inner| (inner.timeline.clone(), inner.added)).unwrap_or((None, false));
    if let Some(timeline) = timeline {
        timeline.hold_or_resume(playhead_s(), added);
    }
    crate::transcribe::resume_after_dub(app, url);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_grammar_is_a_closed_set() {
        assert_eq!(parse_url("rillio-dub://0123456789abcdef").unwrap(), "0123456789abcdef");
        assert!(parse_url("rillio-dub://0123456789ABCDEF").is_err());
        assert!(parse_url("rillio-dub://0123456789abcde").is_err());
        assert!(parse_url("rillio-dub://0123456789abcdef/x").is_err());
        assert!(parse_url("rillio://0123456789abcdef").is_err());
        assert_eq!(format_url("0123456789abcdef"), "rillio-dub://0123456789abcdef");
    }

    #[test]
    fn header_describes_the_whole_timeline() {
        let h = wav_header(48_000);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes([h[40], h[41], h[42], h[43]]), 48_000 * 4);
        assert_eq!(u16::from_le_bytes([h[22], h[23]]), 2);
        assert_eq!(u32::from_le_bytes([h[24], h[25], h[26], h[27]]), 48_000);
    }

    #[test]
    fn reads_block_until_the_window_is_produced_and_cancel_unblocks() {
        let dir = std::env::temp_dir().join(format!("rillio-dub-test-{}", std::process::id()));
        let timeline = Timeline::create(dir.join("t.wav"), 2.0 * WINDOW_S, None).unwrap();
        assert_eq!(timeline.windows(), 2);
        let file = File::open(&timeline.path).unwrap();
        let cancel = Arc::new(CancelFlag::new());
        let mut source = TimelineSource { timeline: timeline.clone(), file, pos: 0, cancel: cancel.clone() };
        let mut header = [0u8; 44];
        assert_eq!(source.read(&mut header).unwrap(), 44);
        assert_eq!(&header[0..4], b"RIFF");

        // A read into window 0 past the probe region parks until fill(); a
        // second thread fills it.
        let filler = timeline.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let samples = vec![1000i16; Timeline::window_frames() as usize * FORMAT.channels as usize];
            filler.fill(0, &samples).unwrap();
        });
        let _ = source.seek(HEADER_LEN + PROBE_BYTES).unwrap();
        let mut buf = [0u8; 16];
        let n = source.read(&mut buf).unwrap();
        t.join().unwrap();
        assert_eq!(n, 16);
        assert_eq!(i16::from_le_bytes([buf[0], buf[1]]), 1000);

        // A read into window 1 parks; cancel releases it with an error.
        let _ = source.seek(HEADER_LEN + Timeline::window_frames() * BYTES_PER_FRAME).unwrap();
        assert_eq!(timeline.reader_frame(), Some(Timeline::window_frames()));
        // A seek interrupt releases a parked read with an error; the next read waits again.
        let interrupter = timeline.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            interrupter.interrupt_epoch.fetch_add(1, Ordering::SeqCst);
            interrupter.produced_cv.notify_all();
        });
        assert!(source.read(&mut buf).unwrap_err().contains("interrupted"));
        t.join().unwrap();
        let canceller = cancel.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            canceller.cancel();
        });
        assert!(source.read(&mut buf).is_err());
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn next_window_goes_forward_then_backfills() {
        let dir = std::env::temp_dir().join(format!("rillio-dub-test2-{}", std::process::id()));
        let timeline = Timeline::create(dir.join("t.wav"), 4.0 * WINDOW_S, None).unwrap();
        assert_eq!(timeline.next_window(2), Some(2));
        timeline.fill(2, &[]).unwrap();
        timeline.fill(3, &[]).unwrap();
        assert_eq!(timeline.next_window(2), Some(1));
        timeline.fill(1, &[]).unwrap();
        timeline.fill(0, &[]).unwrap();
        assert!(timeline.complete());
        assert_eq!(timeline.next_window(0), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// mpv's open probes the head of the file inside the core: those reads
    /// must never wait. An unproduced window 0 answers silence there and
    /// blocks past the probe region as usual.
    #[test]
    fn probe_region_reads_never_block_on_an_unproduced_window_zero() {
        let dir = std::env::temp_dir().join(format!("rillio-dub-test4-{}", std::process::id()));
        let timeline = Timeline::create(dir.join("t.wav"), 2.0 * WINDOW_S, None).unwrap();
        let file = File::open(&timeline.path).unwrap();
        let mut source = TimelineSource { timeline: timeline.clone(), file, pos: 0, cancel: Arc::new(CancelFlag::new()) };
        let mut buf = vec![1u8; 64 * 1024];
        let t0 = Instant::now();
        assert_eq!(source.read(&mut buf).unwrap(), HEADER_LEN as usize);
        let n = source.read(&mut buf).unwrap();
        assert_eq!(n, buf.len());
        assert!(buf.iter().all(|&b| b == 0), "silence expected in the probe region");
        assert!(t0.elapsed() < Duration::from_secs(1), "the probe read waited");
        // The probe region ends exactly at PROBE_BYTES past the header.
        source.seek(HEADER_LEN + PROBE_BYTES - 16).unwrap();
        assert_eq!(source.read(&mut buf).unwrap(), 16);
        // Beyond it a read waits (here: until cancelled).
        source.seek(HEADER_LEN + PROBE_BYTES).unwrap();
        source.cancel.cancel();
        assert!(source.read(&mut buf).is_err());
        // Once window 0 exists, the same region serves the real bytes.
        let pcm = vec![0x0102i16; Timeline::window_frames() as usize * FORMAT.channels as usize];
        timeline.fill(0, &pcm).unwrap();
        source.cancel = Arc::new(CancelFlag::new());
        source.seek(HEADER_LEN).unwrap();
        assert_eq!(source.read(&mut buf).unwrap(), buf.len());
        assert_eq!(&buf[..4], &[0x02, 0x01, 0x02, 0x01]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A timeline reopened for the same duration keeps its windows (the
    /// dub of a title watched before); a different duration starts over.
    #[test]
    fn reopening_keeps_the_made_windows_for_the_same_duration() {
        let dir = std::env::temp_dir().join(format!("rillio-dub-test5-{}", std::process::id()));
        let path = dir.join("t.wav");
        let first = Timeline::create(path.clone(), 2.0 * WINDOW_S, None).unwrap();
        let pcm = vec![0x0304i16; Timeline::window_frames() as usize * FORMAT.channels as usize];
        first.fill(1, &pcm).unwrap();
        drop(first);
        let again = Timeline::create(path.clone(), 2.0 * WINDOW_S, None).unwrap();
        assert!(again.is_produced(1) && !again.is_produced(0));
        let mut file = File::open(&path).unwrap();
        file.seek(SeekFrom::Start(HEADER_LEN + Timeline::window_frames() * BYTES_PER_FRAME)).unwrap();
        let mut two = [0u8; 2];
        file.read_exact(&mut two).unwrap();
        assert_eq!(two, [0x04, 0x03]);
        drop(again);
        let other = Timeline::create(path.clone(), 3.0 * WINDOW_S, None).unwrap();
        assert!(!other.is_produced(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ahead_counts_consecutive_produced_windows_from_the_playhead() {
        let dir = std::env::temp_dir().join(format!("rillio-dub-test3-{}", std::process::id()));
        let timeline = Timeline::create(dir.join("t.wav"), 3.5 * WINDOW_S, None).unwrap();
        let third = WINDOW_S / 3.0;
        assert_eq!(timeline.ahead_s(third), 0.0);
        timeline.fill(0, &[]).unwrap();
        assert!((timeline.ahead_s(third) - (WINDOW_S - third)).abs() < 1e-9);
        timeline.fill(2, &[]).unwrap();
        // Window 1 is missing: the run stops there.
        assert!((timeline.ahead_s(third) - (WINDOW_S - third)).abs() < 1e-9);
        timeline.fill(1, &[]).unwrap();
        assert!((timeline.ahead_s(third) - (3.0 * WINDOW_S - third)).abs() < 1e-9);
        // The last window is short: ahead never exceeds the stream.
        timeline.fill(3, &[]).unwrap();
        assert!((timeline.ahead_s(3.0 * WINDOW_S + third) - (0.5 * WINDOW_S - third)).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
