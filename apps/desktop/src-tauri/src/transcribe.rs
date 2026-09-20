//! Generated subtitles: a live "Generated" track transcribed from the audio
//! by whisper.cpp, locally, for streams that ship no subtitles (or none you
//! trust).
//!
//! Pipeline, on one worker thread per run:
//!   1. make sure the model file exists (downloaded from Hugging Face on first
//!      use into the app data dir, streamed with progress; never bundled),
//!   2. load it into a whisper context (CPU inference),
//!   3. follow the playhead: pick the first 30 s chunk from the current
//!      position onward that is not done yet (up to a few chunks ahead),
//!      decode it with the auto-sync shadow mpv (`autosync::decode_pcm`, 16 kHz
//!      mono), skip it if the VAD hears no speech (whisper hallucinates on
//!      silence and music: "Thank you for watching"), otherwise transcribe it
//!      and hand the cues to the web layer as they land.
//!
//! 30 s chunks because that is whisper's native window; `no_context` keeps
//! chunks independent, which costs a little continuity at the boundaries and
//! buys immunity to the repetition loops context carry-over is known for. The
//! language is auto-detected on the first chunk with speech and then locked,
//! so a music chunk cannot flip the track to another language mid-film.
//!
//! Seeks are free: the worker re-reads the position every chunk, so it simply
//! continues from wherever the viewer went; already-transcribed chunks are
//! remembered per stream and re-sent when the track is re-selected.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

/// The model: whisper `small`, 5-bit quantised (~190 MB). `base` is half the
/// size and twice as fast but visibly worse on film audio; `small` keeps up
/// with playback on a desktop CPU and handles accents and other languages.
const MODEL_FILE: &str = "ggml-small-q5_1.bin";
const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small-q5_1.bin";
/// A download shorter than this is a captive portal page, not the model.
const MODEL_MIN_BYTES: u64 = 150_000_000;
/// Chunk length. whisper's native window is 30 s, but the first line would
/// then wait for 30 s of audio plus the inference; 10 s chunks with a reduced
/// encoder context (`AUDIO_CTX`, the whisper.cpp streaming trick: the encoder
/// only looks at as much of the padded window as the audio fills) put lines
/// on screen a couple of seconds after the press and still run several times
/// faster than real time. Sentences get cut at chunk edges a little more
/// often; live captions make the same trade.
const CHUNK_S: f64 = 10.0;
/// Encoder context in 20 ms frames: 1500 is the full 30 s window; 768 covers
/// a 10 s chunk with slack (whisper.cpp's stream example uses the same).
const AUDIO_CTX: i32 = 768;
/// Chunks are worked through in playhead order: forward from the current
/// position through everything already downloaded, then backward to fill in
/// what the viewer skipped, until the whole file is done. A chunk that will
/// not decode (torrent region not here yet) is retried later, never waited on.
/// Below this share of voiced 10 ms bins a chunk is not sent to whisper.
const MIN_SPEECH_FRACTION: f64 = 0.02;
/// A chunk whose decode failed (torrent region not downloaded yet) is retried
/// after this long.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// The Tauri event every status/segments update rides on.
const EVENT: &str = "subtitles-generate";
/// Readable timing (see [`readable`]): whisper stamps words to the
/// millisecond, so a two-word line would flash for 300 ms. Every line stays
/// up at least `MIN_CUE_MS` or `MS_PER_CHAR` per character (~18 chars/s, a
/// comfortable reading speed), a fragment shorter than the minimum is merged
/// into its neighbour while the result stays one screen line or two, and no
/// line ever overlaps the next (`CUE_GAP_MS` between them).
const MIN_CUE_MS: i64 = 1_200;
const MS_PER_CHAR: i64 = 55;
const MAX_MERGED_CHARS: usize = 84;
const MERGE_GAP_MS: i64 = 1_500;
const CUE_GAP_MS: i64 = 80;

/// How often the transcript is written to disk while a run is in flight (it
/// is also written when the file finishes).
const SAVE_EVERY: Duration = Duration::from_secs(5);

/// One transcribed line, in video time.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    start_ms: i64,
    end_ms: i64,
    text: String,
}

impl Segment {
    pub(crate) fn new(start_ms: i64, end_ms: i64, text: String) -> Self {
        Self { start_ms, end_ms, text }
    }
}

/// Lines transcribed by another producer (the dub worker) for the generated
/// subtitle track; same event the whisper worker sends.
pub(crate) fn emit_segments(app: &AppHandle, url: &str, language: Option<String>, segments: Vec<Segment>) {
    emit(app, Event::Segments { url: url.to_owned(), language, segments });
}

/// What the web layer receives on `subtitles-generate`.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
enum Event {
    /// `state`: downloading | loading | running | idle | error.
    Status { url: String, state: String, detail: Option<String>, progress: Option<f64> },
    Segments { url: String, language: Option<String>, segments: Vec<Segment> },
}

#[derive(Default)]
pub struct TranscribeState(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    /// The stream being transcribed; a different url resets everything below.
    url: Option<String>,
    /// Bumped by every start and stop: the worker whose generation this no
    /// longer is exits after its current chunk.
    generation: u64,
    /// Transcribed chunks (chunk index -> its lines), per url.
    done: BTreeMap<u32, Vec<Segment>>,
    /// Chunks whose decode failed, and when to try them again.
    retry_after: BTreeMap<u32, Instant>,
    /// Locked after the first chunk with speech.
    language: Option<String>,
    /// The first chunk past the end of the file, from the player's reported
    /// duration (a short decode is NOT evidence: mpv routinely returns a
    /// little under the requested length at packet boundaries, and treating
    /// that as the end stopped whole films a minute in).
    end_chunk: Option<u32>,
    /// A worker is alive for `generation` (a second Start on the same url
    /// must not spawn a second one: two whisper runs, and a download race).
    worker_alive: bool,
    /// When the transcript was last written to disk.
    saved_at: Option<Instant>,
    /// The viewer wants this track but a dub worker is transcribing for it
    /// (its lines arrive on the same event); the CPU worker resumes when
    /// the dub stops.
    yielded: bool,
}

impl Inner {
    /// Everything 0..end transcribed (or skipped): the track is complete.
    fn complete(&self) -> bool {
        self.end_chunk.map_or(false, |end| (0..end).all(|c| self.done.contains_key(&c)))
    }

    /// The next chunk to work on: forward from `first`, then backward.
    fn next_chunk(&self, first: u32) -> Option<u32> {
        let now = Instant::now();
        let wanted = |c: &u32| {
            !self.done.contains_key(c) && self.retry_after.get(c).map_or(true, |&at| at <= now)
        };
        let end = self.end_chunk.unwrap_or(u32::MAX);
        (first..end).find(wanted).or_else(|| (0..first.min(end)).rev().find(wanted))
    }
}

/// Start (or resume) generating subtitles for `url`. Returns immediately; the
/// lines arrive as `subtitles-generate` events. Calling it again for the same
/// url re-sends what is already transcribed and keeps the worker going.
#[tauri::command]
pub async fn subtitles_generate_start(
    app: AppHandle,
    state: State<'_, TranscribeState>,
    url: String,
) -> Result<(), String> {
    let url = crate::thumbs::resolve_shadow_url(&app, &url)?;
    crate::thumbs::validate_url(&url)?;
    let arc = state.0.clone();
    let (spawn, generation, replay, language, complete) = {
        let mut inner = arc.lock().map_err(|_| "transcribe: poisoned")?;
        if inner.url.as_deref() != Some(url.as_str()) {
            inner.url = Some(url.clone());
            inner.done.clear();
            inner.retry_after.clear();
            inner.language = None;
            inner.end_chunk = None;
            inner.saved_at = None;
            // A transcript from a previous watch: the lines are on screen
            // immediately and only the missing chunks are ever recomputed.
            if let Some(cache) = load_cache(&app, &url) {
                tracing::info!("transcribe: loaded {} cached chunks for this stream", cache.chunks.len());
                inner.done = cache.chunks;
                inner.language = cache.language;
                inner.end_chunk = cache.end_chunk;
                inner.saved_at = Some(Instant::now());
            }
        }
        let replay: Vec<Segment> = inner.done.values().flatten().cloned().collect();
        // Same url, worker alive: nothing to start, just re-send the lines.
        let spawn = !inner.worker_alive;
        if spawn {
            inner.generation += 1;
            inner.worker_alive = true;
        }
        (spawn, inner.generation, replay, inner.language.clone(), inner.complete())
    };
    if !replay.is_empty() {
        emit(&app, Event::Segments { url: url.clone(), language, segments: replay });
    }
    if !spawn {
        return Ok(());
    }
    if complete {
        // Nothing left to transcribe for this url: report it, no worker.
        if let Ok(mut inner) = arc.lock() {
            inner.worker_alive = false;
        }
        status(&app, &url, "done", None, None);
        return Ok(());
    }
    // A running dub transcribes the dialogue itself (on the GPU, from the
    // separated stem) and sends the lines on this same event: a second
    // whisper on the CPU would only fight it for cores (observed live: both
    // crawled). The CPU worker resumes when the dub stops.
    if crate::dub::is_active_for(&app, &url) {
        if let Ok(mut inner) = arc.lock() {
            inner.worker_alive = false;
            inner.yielded = true;
        }
        status(&app, &url, "running", Some("from the AI dub".into()), None);
        return Ok(());
    }
    spawn_worker(app, arc, generation, url)
}

/// A dedicated OS thread: decoding (mpv FFI), the model load and inference
/// are all blocking, and one run holds a CPU-heavy context for minutes.
fn spawn_worker(app: AppHandle, arc: Arc<Mutex<Inner>>, generation: u64, url: String) -> Result<(), String> {
    std::thread::Builder::new()
        .name("subtitles-generate".into())
        .spawn(move || worker(app, arc, generation, url))
        .map_err(|e| format!("transcribe: spawn: {e}"))?;
    Ok(())
}

/// A dub worker starts for `url`: a CPU transcription running for it stops
/// (its lines now come from the dub) and is remembered as wanted.
pub(crate) fn yield_to_dub(app: &AppHandle, url: &str) {
    let state = app.state::<TranscribeState>();
    let Ok(mut inner) = state.0.lock() else { return };
    if inner.url.as_deref() != Some(url) || !inner.worker_alive {
        return;
    }
    inner.generation += 1;
    inner.worker_alive = false;
    inner.yielded = true;
    drop(inner);
    status(app, url, "running", Some("from the AI dub".into()), None);
}

/// The dub for `url` stopped: a transcription that had yielded to it
/// resumes on the CPU where the dub left off.
pub(crate) fn resume_after_dub(app: &AppHandle, url: &str) {
    let state = app.state::<TranscribeState>();
    let arc = state.0.clone();
    let generation = {
        let Ok(mut inner) = arc.lock() else { return };
        if inner.url.as_deref() != Some(url) || !inner.yielded || inner.worker_alive {
            return;
        }
        inner.yielded = false;
        inner.generation += 1;
        inner.worker_alive = true;
        inner.generation
    };
    if let Err(e) = spawn_worker(app.clone(), arc.clone(), generation, url.to_owned()) {
        tracing::error!("{e}");
        if let Ok(mut inner) = arc.lock() {
            inner.worker_alive = false;
        }
    }
}

/// Stop generating (the track keeps what it has).
#[tauri::command]
pub async fn subtitles_generate_stop(state: State<'_, TranscribeState>) -> Result<(), String> {
    let mut inner = state.0.lock().map_err(|_| "transcribe: poisoned")?;
    inner.generation += 1;
    // The superseded worker exits after its current chunk; a start in the
    // meantime may spawn its successor.
    inner.worker_alive = false;
    inner.yielded = false;
    Ok(())
}

fn emit(app: &AppHandle, event: Event) {
    if let Err(e) = app.emit(EVENT, event) {
        tracing::warn!("transcribe: emit failed: {e}");
    }
}

fn status(app: &AppHandle, url: &str, state: &str, detail: Option<String>, progress: Option<f64>) {
    emit(app, Event::Status { url: url.to_owned(), state: state.into(), detail, progress });
}

fn current_generation(arc: &Arc<Mutex<Inner>>) -> u64 {
    arc.lock().map(|inner| inner.generation).unwrap_or(u64::MAX)
}

fn worker(app: AppHandle, arc: Arc<Mutex<Inner>>, generation: u64, url: String) {
    let result = run(&app, &arc, generation, &url);
    let still_current = {
        match arc.lock() {
            Ok(mut inner) => {
                let current = inner.generation == generation;
                if current {
                    inner.worker_alive = false;
                }
                current
            }
            Err(_) => false,
        }
    };
    match result {
        Ok(Finish::Complete) if still_current => {
            save_cache(&app, &arc, &url);
            status(&app, &url, "done", None, None);
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!("transcribe: {e}");
            if still_current {
                status(&app, &url, "error", Some(e), None);
            }
        }
    }
}

/// Why a worker returned.
enum Finish {
    /// Every chunk of the file is done.
    Complete,
    /// Superseded by a later start/stop.
    Superseded,
}

fn run(app: &AppHandle, arc: &Arc<Mutex<Inner>>, generation: u64, url: &str) -> Result<Finish, String> {
    let model = ensure_model(app, arc, generation, url)?;
    if current_generation(arc) != generation {
        return Ok(Finish::Superseded);
    }
    status(app, url, "loading", None, None);
    let started = Instant::now();
    let mut params = whisper_rs::WhisperContextParameters::default();
    params.use_gpu(false);
    let ctx = whisper_rs::WhisperContext::new_with_params(&model, params)
        .map_err(|e| format!("transcribe: loading {}: {e}", model.display()))?;
    let mut whisper = ctx.create_state().map_err(|e| format!("transcribe: state: {e}"))?;
    tracing::info!("transcribe: model loaded in {} ms", started.elapsed().as_millis());
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let threads = (threads / 2).clamp(2, 8) as i32;
    status(app, url, "running", None, None);

    loop {
        if current_generation(arc) != generation {
            return Ok(Finish::Superseded);
        }
        let position = crate::shell::player_time_pos(app).unwrap_or(0.0);
        let first = (position / CHUNK_S) as u32;
        let duration_chunks = crate::shell::player_duration(app).map(|d| (d / CHUNK_S).ceil() as u32);
        let (chunk, complete) = {
            let mut inner = arc.lock().map_err(|_| "transcribe: poisoned")?;
            if let Some(end) = duration_chunks {
                inner.end_chunk = Some(end);
            }
            (inner.next_chunk(first), inner.complete())
        };
        if complete {
            return Ok(Finish::Complete);
        }
        let Some(chunk) = chunk else {
            // Everything reachable is done or waiting on a retry.
            std::thread::sleep(Duration::from_millis(1000));
            continue;
        };
        let start_s = chunk as f64 * CHUNK_S;
        let samples = match crate::autosync::decode_pcm(url, start_s, CHUNK_S) {
            Ok(samples) => samples,
            Err(e) => {
                // Typically a torrent region that is not downloaded yet; the
                // player itself will stall there too, so waiting is right.
                tracing::debug!("transcribe: chunk {chunk} decode: {e}");
                if let Ok(mut inner) = arc.lock() {
                    inner.retry_after.insert(chunk, Instant::now() + RETRY_AFTER);
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        let seconds = samples.len() as f64 / crate::autosync::SAMPLE_RATE as f64;
        if seconds < 1.0 {
            // Nothing at all came back: past the end of the file. Only this
            // (empty) case bounds the run when the duration is unknown - a
            // merely SHORT decode is normal at packet boundaries.
            if let Ok(mut inner) = arc.lock() {
                inner.end_chunk = Some(inner.end_chunk.map_or(chunk, |e| e.min(chunk)));
            }
            mark_done(arc, chunk, Vec::new());
            continue;
        }
        let speech = crate::autosync::speech_bins(&samples)?;
        let fraction = speech.iter().filter(|&&s| s).count() as f64 / speech.len().max(1) as f64;
        if fraction < MIN_SPEECH_FRACTION {
            tracing::debug!("transcribe: chunk {chunk}: no speech ({fraction:.3}), skipped");
            mark_done(arc, chunk, Vec::new());
            continue;
        }

        let language = arc.lock().map(|inner| inner.language.clone()).unwrap_or(None);
        let audio: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
        let t0 = Instant::now();
        let (segments, detected) = transcribe(&mut whisper, &audio, threads, language.as_deref())?;
        let segments: Vec<Segment> = readable(
            segments
                .into_iter()
                .filter_map(|(start_cs, end_cs, text)| {
                    let text = clean_text(&text)?;
                    Some(Segment {
                        start_ms: (start_s * 1000.0) as i64 + start_cs * 10,
                        end_ms: (start_s * 1000.0) as i64 + end_cs * 10,
                        text,
                    })
                })
                .collect(),
        );
        tracing::info!(
            "transcribe: chunk {chunk} ({seconds:.1}s, speech {fraction:.2}) -> {} lines in {} ms{}",
            segments.len(),
            t0.elapsed().as_millis(),
            detected.as_deref().map(|l| format!(", language {l}")).unwrap_or_default(),
        );
        if current_generation(arc) != generation {
            return Ok(Finish::Superseded);
        }
        let language = {
            let mut inner = arc.lock().map_err(|_| "transcribe: poisoned")?;
            if inner.language.is_none() {
                inner.language = detected;
            }
            inner.done.insert(chunk, segments.clone());
            inner.language.clone()
        };
        if !segments.is_empty() {
            emit(app, Event::Segments { url: url.to_owned(), language, segments });
        }
        // Persist periodically, so a crash (or just closing the app) costs at
        // most a few seconds of work.
        let due = {
            let mut inner = arc.lock().map_err(|_| "transcribe: poisoned")?;
            let due = inner.saved_at.map_or(true, |at| at.elapsed() >= SAVE_EVERY);
            if due {
                inner.saved_at = Some(Instant::now());
            }
            due
        };
        if due {
            save_cache(app, arc, url);
        }
    }
}

/// Where a stream's transcript lives. The key is derived from the url's
/// identity, not its address: the streaming server binds a different port
/// each run, so `http://127.0.0.1:<port>/<infohash>/<idx>` must map to the
/// same file every time.
fn cache_path(app: &AppHandle, url: &str, extension: &str) -> Option<PathBuf> {
    let dir = app.path().app_data_dir().ok()?.join("generated-subtitles");
    Some(dir.join(format!("{}.{extension}", cache_key(url))))
}

pub(crate) fn cache_key(url: &str) -> String {
    let lower = url.trim().to_ascii_lowercase();
    // Everything after the authority for a local server url; the whole url
    // otherwise (a remote host is part of the identity).
    let identity = lower
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| lower.strip_prefix("http://localhost:"))
        .and_then(|rest| rest.split_once('/').map(|(_, path)| path))
        .unwrap_or(&lower);
    // FNV-1a 64: stable across releases (unlike DefaultHasher), which is the
    // whole point of a cache key written to disk.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in identity.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The on-disk transcript. `chunk_s` and `model` are recorded so a change to
/// either invalidates it rather than mixing incompatible timings.
#[derive(Serialize, Deserialize)]
struct Cache {
    chunk_s: f64,
    model: String,
    language: Option<String>,
    end_chunk: Option<u32>,
    chunks: BTreeMap<u32, Vec<Segment>>,
}

fn load_cache(app: &AppHandle, url: &str) -> Option<Cache> {
    let path = cache_path(app, url, "json")?;
    let bytes = std::fs::read(&path).ok()?;
    let cache: Cache = serde_json::from_slice(&bytes).ok()?;
    if cache.chunk_s != CHUNK_S || cache.model != MODEL_FILE {
        tracing::debug!("transcribe: ignoring {} (different chunking or model)", path.display());
        return None;
    }
    Some(cache)
}

/// Write the transcript: a JSON sidecar (what a later run resumes from) and
/// the VTT next to it (what a person can take elsewhere). Both atomic.
fn save_cache(app: &AppHandle, arc: &Arc<Mutex<Inner>>, url: &str) {
    let (Some(json_path), Some(vtt_path)) = (cache_path(app, url, "json"), cache_path(app, url, "vtt")) else {
        return;
    };
    let cache = {
        let Ok(inner) = arc.lock() else { return };
        Cache {
            chunk_s: CHUNK_S,
            model: MODEL_FILE.to_owned(),
            language: inner.language.clone(),
            end_chunk: inner.end_chunk,
            chunks: inner.done.clone(),
        }
    };
    if let Some(dir) = json_path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!("transcribe: creating {}: {e}", dir.display());
            return;
        }
    }
    let write = |path: &PathBuf, bytes: &[u8]| -> std::io::Result<()> {
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, bytes)?;
        std::fs::rename(&temp, path)
    };
    match serde_json::to_vec(&cache) {
        Ok(bytes) => {
            if let Err(e) = write(&json_path, &bytes) {
                tracing::warn!("transcribe: writing {}: {e}", json_path.display());
            }
        }
        Err(e) => tracing::warn!("transcribe: serializing the transcript: {e}"),
    }
    let vtt = to_vtt(cache.chunks.values().flatten());
    if let Err(e) = write(&vtt_path, vtt.as_bytes()) {
        tracing::warn!("transcribe: writing {}: {e}", vtt_path.display());
    }
}

/// The transcript as one WebVTT document (same shape the web layer builds
/// from the live batches, so the saved file matches what was on screen).
fn to_vtt<'a>(segments: impl Iterator<Item = &'a Segment>) -> String {
    let stamp = |ms: i64| {
        let ms = ms.max(0);
        format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, (ms % 3_600_000) / 60_000, (ms % 60_000) / 1000, ms % 1000)
    };
    let mut sorted: Vec<&Segment> = segments.collect();
    sorted.sort_by_key(|s| s.start_ms);
    let mut out = String::from("WEBVTT\n\n");
    for (i, segment) in sorted.iter().enumerate() {
        let mut end = segment.end_ms.max(segment.start_ms + 300);
        if let Some(next) = sorted.get(i + 1) {
            end = end.min(next.start_ms - CUE_GAP_MS).max(segment.start_ms + 300);
        }
        out.push_str(&format!("{} --> {}\n{}\n\n", stamp(segment.start_ms), stamp(end), segment.text));
    }
    out
}

fn mark_done(arc: &Arc<Mutex<Inner>>, chunk: u32, segments: Vec<Segment>) {
    if let Ok(mut inner) = arc.lock() {
        inner.done.insert(chunk, segments);
    }
}

/// Run whisper over one chunk. Returns `(start_cs, end_cs, text)` per segment
/// (centiseconds into the chunk) and the language it settled on when
/// detecting.
fn transcribe(
    state: &mut whisper_rs::WhisperState,
    audio: &[f32],
    threads: i32,
    language: Option<&str>,
) -> Result<(Vec<(i64, i64, String)>, Option<String>), String> {
    let mut params = whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(threads);
    params.set_audio_ctx(AUDIO_CTX);
    params.set_language(Some(language.unwrap_or("auto")));
    params.set_translate(false);
    params.set_no_context(true);
    params.set_single_segment(false);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    params.set_no_speech_thold(0.6);
    state.full(params, audio).map_err(|e| format!("transcribe: inference: {e}"))?;
    let mut segments = Vec::new();
    for i in 0..state.full_n_segments() {
        let Some(segment) = state.get_segment(i) else { continue };
        let text = segment.to_str_lossy().map_err(|e| format!("transcribe: text: {e}"))?.into_owned();
        segments.push((segment.start_timestamp(), segment.end_timestamp(), text));
    }
    let detected = match language {
        Some(_) => None,
        None => whisper_rs::get_lang_str(state.full_lang_id_from_state()).map(str::to_owned),
    };
    Ok((segments, detected))
}

/// Timing a person can read (constants above): merge sub-minimum fragments
/// forward, hold each line for its reading time, never overlap the next.
fn readable(segments: Vec<Segment>) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for segment in segments {
        if let Some(last) = out.last_mut() {
            // Either side being a fragment merges them: "Yes." before a
            // sentence, or a trailing "OK." after one.
            let short = last.end_ms - last.start_ms < MIN_CUE_MS || segment.end_ms - segment.start_ms < MIN_CUE_MS;
            let fits = last.text.chars().count() + 1 + segment.text.chars().count() <= MAX_MERGED_CHARS;
            let adjacent = segment.start_ms - last.end_ms < MERGE_GAP_MS;
            if short && fits && adjacent {
                last.text.push(' ');
                last.text.push_str(&segment.text);
                last.end_ms = last.end_ms.max(segment.end_ms);
                continue;
            }
        }
        out.push(segment);
    }
    for i in 0..out.len() {
        let chars = out[i].text.chars().count() as i64;
        let held = out[i].start_ms + MIN_CUE_MS.max(chars * MS_PER_CHAR);
        let mut end = out[i].end_ms.max(held);
        if let Some(next_start) = out.get(i + 1).map(|n| n.start_ms) {
            end = end.min(next_start - CUE_GAP_MS);
        }
        out[i].end_ms = end.max(out[i].start_ms + 300);
    }
    out
}

/// Whisper's non-speech markers ("[Music]", "(applause)", "♪") and empties
/// are not lines; everything else is trimmed.
fn clean_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bracketed = (trimmed.starts_with('[') && trimmed.ends_with(']')) ||
        (trimmed.starts_with('(') && trimmed.ends_with(')'));
    if bracketed || trimmed.chars().all(|c| c == '♪' || c == '♫' || c.is_whitespace() || c == '.') {
        return None;
    }
    Some(trimmed.to_owned())
}

/// The model file, downloading it on first use. Progress rides on the status
/// event; an aborted run (generation bumped) removes the partial file.
fn ensure_model(app: &AppHandle, arc: &Arc<Mutex<Inner>>, generation: u64, url: &str) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("transcribe: app data dir: {e}"))?
        .join("models");
    let path = dir.join(MODEL_FILE);
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() >= MODEL_MIN_BYTES {
            return Ok(path);
        }
        // A stub or a truncated file: fetch again.
        let _ = std::fs::remove_file(&path);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("transcribe: creating {}: {e}", dir.display()))?;
    // Per-generation name: a superseded download cleaning up must never
    // unlink the file its successor is writing.
    let part = dir.join(format!("{MODEL_FILE}.{generation}.part"));
    status(app, url, "downloading", None, Some(0.0));
    let result = tauri::async_runtime::block_on(download(app, arc, generation, url, &part));
    match result {
        Ok(()) => {
            std::fs::rename(&part, &path).map_err(|e| format!("transcribe: placing the model: {e}"))?;
            Ok(path)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

async fn download(app: &AppHandle, arc: &Arc<Mutex<Inner>>, generation: u64, url: &str, part: &PathBuf) -> Result<(), String> {
    use futures_util::StreamExt as _;
    use std::io::Write as _;

    let client = reqwest::Client::builder()
        .user_agent(format!("Rillio/{}", app.package_info().version))
        .build()
        .map_err(|e| format!("transcribe: http client: {e}"))?;
    let response = client
        .get(MODEL_URL)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("transcribe: model download: {e}"))?;
    let total = response.content_length();
    let mut file = std::fs::File::create(part).map_err(|e| format!("transcribe: creating {}: {e}", part.display()))?;
    let mut stream = response.bytes_stream();
    let mut received: u64 = 0;
    let mut last_report = Instant::now();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("transcribe: model download: {e}"))?;
        file.write_all(&chunk).map_err(|e| format!("transcribe: writing the model: {e}"))?;
        received += chunk.len() as u64;
        if current_generation(arc) != generation {
            return Err("transcribe: download cancelled".into());
        }
        if last_report.elapsed() >= Duration::from_millis(500) {
            last_report = Instant::now();
            let progress = total.map(|t| received as f64 / t as f64);
            status(app, url, "downloading", None, progress);
        }
    }
    file.flush().map_err(|e| format!("transcribe: writing the model: {e}"))?;
    drop(file);
    if let Some(total) = total {
        if received != total {
            return Err(format!("transcribe: model download stopped at {received} of {total} bytes"));
        }
    }
    if received < MODEL_MIN_BYTES {
        return Err(format!("transcribe: model download too small ({received} bytes)"));
    }
    tracing::info!("transcribe: model downloaded ({received} bytes)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start_ms: i64, end_ms: i64, text: &str) -> Segment {
        Segment { start_ms, end_ms, text: text.into() }
    }

    /// A 300 ms "Yes." must not flash: it merges into the line that follows,
    /// a lone short line is held for the minimum, a long line for its
    /// reading time, and nothing runs into the next line.
    #[test]
    fn timing_is_readable() {
        let out = readable(vec![
            seg(1000, 1300, "Yes."),
            seg(1500, 2600, "We should leave after lunch."),
            seg(2700, 3000, "OK."),
            seg(9000, 9800, "The last train back leaves at a quarter past nine, so do not dawdle."),
            seg(20000, 20400, "Fine."),
        ]);
        let reading = |s: &Segment| s.start_ms + MIN_CUE_MS.max(s.text.chars().count() as i64 * MS_PER_CHAR);
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[0].text, "Yes. We should leave after lunch. OK.");
        assert_eq!(out[0].start_ms, 1000);
        // Both fragments merged into the sentence, held for the whole line's
        // reading time (longer than the 3000 ms whisper stamped).
        assert_eq!(out[0].end_ms, reading(&out[0]));
        assert!(out[0].end_ms > 3000);
        // The long line is held for its reading time, well before the next.
        assert_eq!(out[1].end_ms, reading(&out[1]));
        assert!(out[1].end_ms < out[2].start_ms);
        // Last, short: held for the minimum.
        assert_eq!(out[2].end_ms, 20000 + MIN_CUE_MS);
        // Overlap guard: a held line stops CUE_GAP_MS before the next.
        let out = readable(vec![seg(0, 200, "One."), seg(700, 2000, "Two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen.")]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].end_ms, 700 - CUE_GAP_MS);
    }

    /// The streaming server binds a different port every run, so the same
    /// file must hash the same across runs; different files must not collide.
    #[test]
    fn cache_key_follows_the_file_not_the_port() {
        let ih = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            cache_key(&format!("http://127.0.0.1:11470/{ih}/0")),
            cache_key(&format!("http://127.0.0.1:49312/{ih}/0")),
        );
        assert_ne!(
            cache_key(&format!("http://127.0.0.1:11470/{ih}/0")),
            cache_key(&format!("http://127.0.0.1:11470/{ih}/1")),
        );
        // A remote url keeps its host in the identity.
        assert_ne!(cache_key("https://a.example/x.mkv"), cache_key("https://b.example/x.mkv"));
        assert_eq!(cache_key("https://a.example/x.mkv"), cache_key("  HTTPS://A.EXAMPLE/x.mkv  "));
        assert_eq!(cache_key("https://a.example/x.mkv").len(), 16);
    }

    /// The saved file is the same document the viewer saw: sorted, no
    /// overlaps, hh:mm:ss.mmm stamps.
    #[test]
    fn saved_vtt_is_well_formed() {
        let segments = vec![
            seg(3_600_000, 3_602_000, "An hour in."),
            seg(1_000, 9_000, "Held far too long."),
            seg(5_000, 6_000, "Next."),
        ];
        let vtt = to_vtt(segments.iter());
        assert!(vtt.starts_with("WEBVTT\n\n"), "{vtt}");
        let lines: Vec<&str> = vtt.lines().collect();
        assert_eq!(lines[2], "00:00:01.000 --> 00:00:04.920");
        assert_eq!(lines[3], "Held far too long.");
        assert_eq!(lines[5], "00:00:05.000 --> 00:00:06.000");
        assert_eq!(lines[8], "01:00:00.000 --> 01:00:02.000");
    }

    #[test]
    fn non_speech_markers_are_not_lines() {
        assert_eq!(clean_text("  Hello there.  "), Some("Hello there.".into()));
        assert_eq!(clean_text("[Music]"), None);
        assert_eq!(clean_text("(applause)"), None);
        assert_eq!(clean_text("♪ ♪"), None);
        assert_eq!(clean_text("..."), None);
        assert_eq!(clean_text(""), None);
        // A bracket INSIDE a line is content.
        assert_eq!(clean_text("He said [sic] that"), Some("He said [sic] that".into()));
    }
}
