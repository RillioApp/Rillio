//! The dub worker's real form (Stage 1, `docs/dubbing/stage1-pipeline`): one
//! window of the timeline in, its dubbed stereo PCM out. Per window:
//!
//!   1. decode the source audio for the span the window needs, separate it
//!      into dialogue and residual (`separate.rs`, D1c) with a margin on each
//!      side so the chunked separator sees full context at the edges;
//!   2. segment the dialogue into speaker turns (`turns.rs`, D3a) over a
//!      rolling buffer that keeps the previous turns for references and looks
//!      ahead far enough to hold the longest turn and its room;
//!   3. per turn starting in the window: transcribe its dialogue slice, translate
//!      with the room's length budget (D5), synthesize in the speaker's own
//!      voice from their turns (G5 arm pcs, D4) with the verified retry,
//!      fit, level-match and mix over the residual bed (`dubfit.rs`);
//!   4. hand back the window's slice of the bed.
//!
//! The sidecars are the pack's (`sidecar.rs`, D1/D6a). A window that is not
//! the continuation of the previous one (a seek, a backfill) resets the
//! buffer, and turns that straddle the window start are produced then too, so
//! the first line after a seek is dubbed rather than silent.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::autosync::{decode_pcm_as, PcmFormat};
use crate::dubclients::{synthesize_verified, Asr, Translator, Tts, VerifiedTake};
use crate::dubhandle;
use crate::dubplace;
use crate::dubfit::{self, budget_syllables, duck, fit, level_gain, mix_into, room_ms, sounding_range, trim_silence, Fit, DUCK_GAIN};
use crate::instrument::{self, Instrument};
use crate::separate::Separator;
use crate::sidecar::{SidecarKind, SidecarModels, Supervisor};
use crate::turns::{self, segment, speaker_reference, SpeakerEncoder, Turn};

/// The pack files this pipeline opens, by the manifest's names
/// (`packs::MANIFEST` is cross-checked in the tests).
/// The 4B: Stage 0 measured the 2B at 80 % of it on Japanese (the anime
/// pair) and its latency at a quarter second per line on this GPU; Michael
/// heard the 2B's misses. The 2B stays in the pack as the small-GPU arm.
pub const TRANSLATOR_GGUF: &str = "Qwen3.5-4B-Q8_0.gguf";
pub const TTS_BASE_LM_GGUF: &str = "VoxCPM2-BaseLM-F16.gguf";
pub const TTS_ACOUSTIC_GGUF: &str = "VoxCPM2-Acoustic-F16.gguf";
pub const ASR_MODEL: &str = "ggml-small-q5_1.bin";
/// whisper's DTW alignment-heads preset for [`ASR_MODEL`]'s size.
pub const ASR_DTW_PRESET: &str = "small";
pub const SEPARATOR_ONNX: &str = "bs_roformer_ep368_fp16.onnx";
pub const SPEAKER_ENCODER_ONNX: &str = "ge2e_resemblyzer.onnx";
pub const ORT_DLL: &str = "onnxruntime.dll";

const RATE: u32 = dubfit::OUT_RATE;
const FORMAT: PcmFormat = PcmFormat { rate: RATE, channels: 2 };
const ASR_RATE: u32 = turns::RATE as u32;
/// The separator's chunk step: audio within one step of a span's edge is
/// estimated from a single chunk, so spans overlap by this much and the edges
/// are discarded.
const SEPARATE_MARGIN_S: f64 = 4.0;
/// How far past the window's end the buffer must reach: the longest turn a
/// window can start plus the room its line may run into.
const LOOKAHEAD_S: f64 = turns::MAX_TURN_MS as f64 / 1000.0 + dubfit::TRAILING_ROOM_MS as f64 / 1000.0;
/// How far before a fresh window's start the buffer begins: a turn that
/// straddles the start is produced, and the previous turns feed references.
const LOOKBACK_S: f64 = turns::MAX_TURN_MS as f64 / 1000.0;
/// The speaker's own turns nearest the line, up to this long (G5 arm pcs).
const REFERENCE_MAX_S: f64 = 8.0;
/// Previous source lines given to the translator as context.
const CONTEXT_TURNS: usize = 3;
/// A turn whose transcript is shorter than this carries no line.
const MIN_TURN_TEXT_CHARS: usize = 2;
/// The dub's target language (decision D7).
const TARGET_LANGUAGE: &str = "English";
const TARGET_LANGUAGE_WHISPER: &str = "english";
/// The dub's language as the subtitle track labels it.
pub const TARGET_LANGUAGE_CODE: &str = "en";
/// The synopsis handed to the models is cut here (whisper's prompt window
/// is 224 tokens; a paragraph is plenty for names and the story's words).
const ABOUT_MAX_CHARS: usize = 500;
/// Generation may run this far past the line's room before it counts as a
/// runaway (D5's guard: the server stops there).
const RUNAWAY_ROOM_FACTOR: f32 = 3.0;
/// With a handle the take's length is known (the handle's span): a take
/// past twice it plus the lead-in silence takes carry is a runaway whatever
/// the room (reel v2, 2026-09-17: takes overshoot the span by 1.3x at the
/// median; the live test of 2026-09-18 ran a 0.4 s interjection with a
/// 4.4 s room to three 13 s takes, 170 s for one line).
const RUNAWAY_SPAN_FACTOR: f32 = 2.0;
const RUNAWAY_LEAD_IN_S: f32 = 1.0;

/// Seconds of audio past which a take is a runaway: from the handle's span
/// when there is one, never more than the room allows.
fn runaway_s(room_ms: u32, span_s: Option<f64>) -> f32 {
    let by_room = room_ms as f32 / 1000.0 * RUNAWAY_ROOM_FACTOR;
    span_s.map_or(by_room, |span| (span as f32 * RUNAWAY_SPAN_FACTOR + RUNAWAY_LEAD_IN_S).min(by_room))
}
/// The dub model's second prefix segment (M1, engine repo): the source line's
/// own separated voice goes to the TTS as `style_audio`, so the take follows
/// its delivery. On by default; `RILLIO_DUB_STYLE=0` is the plain clone
/// (reference mode), the control arm.
const STYLE_SEGMENT_ENV: &str = "RILLIO_DUB_STYLE";
/// The delivery handle (`dubhandle`): the source line's phrases and pauses
/// dealt to the English words as a bracket score in front of the spoken
/// text. On by default; `RILLIO_DUB_HANDLE=0` speaks the plain line.
const HANDLE_ENV: &str = "RILLIO_DUB_HANDLE";
/// The instrument (M3, `instrument`): the speaker reference's voice as
/// pseudo-patches the sidecar prepends to the reference segment. On when
/// the pack holds `instrument.onnx`; `RILLIO_DUB_INSTRUMENT=0` leaves it
/// out, `=1` demands the file (a pack without it is then an error, never a
/// silent plain clone).
const INSTRUMENT_ENV: &str = "RILLIO_DUB_INSTRUMENT";
/// Phrase-anchored placement (`dubplace`): the take cut between phrases and
/// each phrase put back on its planned time. On by default;
/// `RILLIO_DUB_ANCHOR=0` leaves the take in one piece (the A/B for cuts that
/// are heard as abruptions, Michael 2026-09-21).
const ANCHOR_ENV: &str = "RILLIO_DUB_ANCHOR";

/// Why a window could not be produced: the source is not decodable yet (a
/// torrent region still downloading: the player stalls there too, retry
/// later) or the pipeline itself broke (a sidecar died: stop, loudly).
#[derive(Debug)]
pub enum ProduceError {
    NotReady(String),
    Failed(String),
}

impl From<String> for ProduceError {
    fn from(e: String) -> Self {
        ProduceError::Failed(e)
    }
}

impl From<&str> for ProduceError {
    fn from(e: &str) -> Self {
        ProduceError::Failed(e.to_owned())
    }
}

/// One dubbed line, for the log and the bench (`lines.jsonl`'s fields).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Line {
    pub start_ms: u32,
    pub end_ms: u32,
    pub speaker: String,
    pub src: String,
    pub en: String,
    pub fit: String,
    pub wer: f64,
    pub gain: f32,
    pub ms: u128,
    /// The delivery handle the model was given (empty without one).
    pub handle: String,
}

/// The separated, segmented span the worker is currently inside; sample
/// positions are absolute stream frames at 48 kHz.
struct Buffer {
    url: String,
    start_frame: u64,
    /// What the player gets, stereo interleaved 48 kHz: the ORIGINAL mix
    /// everywhere, swapped for the residual (crossfaded) only inside a
    /// dubbed line, with the line mixed over it. Outside lines the film's
    /// own music and effects stay untouched (the separator's artifacts were
    /// audible everywhere when the residual was the whole bed).
    bed: Vec<f32>,
    /// The voice-removed stem, stereo interleaved 48 kHz.
    residual: Vec<f32>,
    /// Dialogue, mono 48 kHz (the level reference).
    vocals48: Vec<f32>,
    /// Dialogue, mono 16 kHz (turns, ASR, references).
    vocals16: Vec<f32>,
    /// The VAD's flag per 20 ms frame of `vocals16` (one pass over the
    /// buffer: the turns and the lines' phrases both read it).
    voiced: Vec<bool>,
    turns: Vec<Turn>,
    /// Absolute start (ms) of every turn already dubbed into the bed.
    produced: HashSet<u32>,
    /// Source lines in order, the translator's context.
    history: Vec<String>,
}

impl Buffer {
    fn end_frame(&self) -> u64 {
        self.start_frame + (self.bed.len() / 2) as u64
    }

    /// Where the buffer starts, in stream milliseconds. Turns and slices are
    /// RELATIVE to it (the segmenter sees the buffer as its whole stem);
    /// produced keys and line records are absolute.
    fn origin_ms(&self) -> i64 {
        (self.start_frame * 1000 / RATE as u64) as i64
    }

    fn frame_of_relative_ms(&self, ms: i64) -> usize {
        (ms.max(0) as u64 * RATE as u64 / 1000) as usize
    }
}

pub struct Pipeline {
    supervisor: Supervisor,
    translator: Translator,
    tts: Tts,
    asr: Asr,
    separator: Separator,
    encoder: SpeakerEncoder,
    /// Whisper's full lower-case name of the source language ("japanese"),
    /// detected on the first window and pinned.
    source_language: Option<String>,
    /// The title's name and synopsis, for the recognizer's prompt and the
    /// translator (ambiguous words resolve to the story's).
    about: Option<String>,
    buffer: Option<Buffer>,
    /// Send the source line's voice as the TTS's style segment (see
    /// [`STYLE_SEGMENT_ENV`]).
    style_segment: bool,
    /// Write the delivery handle in front of the spoken line (see [`HANDLE_ENV`]).
    handle: bool,
    /// The reference's instrument for the TTS (see [`INSTRUMENT_ENV`]).
    instrument: Option<Instrument>,
    /// Put the take's phrases back on their planned times (see [`ANCHOR_ENV`]).
    anchor: bool,
}

impl Pipeline {
    /// Start the three sidecars from `pack_dir` and load the two ONNX models.
    pub fn open(pack_dir: &Path, log_dir: &Path) -> Result<Self, String> {
        let style_segment = std::env::var(STYLE_SEGMENT_ENV).map_or(true, |v| v != "0");
        let handle = std::env::var(HANDLE_ENV).map_or(true, |v| v != "0");
        let anchor = std::env::var(ANCHOR_ENV).map_or(true, |v| v != "0");
        let file =|name: &str| -> PathBuf { pack_dir.join(name) };
        let instrument_file = file(instrument::FILE);
        let want_instrument = match std::env::var(INSTRUMENT_ENV).as_deref() {
            Ok("0") => false,
            Ok("1") if !instrument_file.exists() => return Err(format!("dubpipe: {INSTRUMENT_ENV}=1 but the pack has no {}", instrument::FILE)),
            Ok(_) => true,
            Err(_) => instrument_file.exists(),
        };
        tracing::info!(
            "dubpipe: pack {}, style segment {}, handle {}, instrument {}, anchor {}",
            pack_dir.display(),
            if style_segment { "on (M1)" } else { "off (plain clone)" },
            if handle { "on" } else { "off" },
            if want_instrument { "on (M3)" } else { "off" },
            if anchor { "on" } else { "off" }
        );
        let mut supervisor = Supervisor::new(log_dir.to_path_buf())?;
        supervisor.start(&file(SidecarKind::Asr.exe_name()), SidecarModels::Asr { model: file(ASR_MODEL), dtw_preset: ASR_DTW_PRESET })?;
        supervisor.start(&file(SidecarKind::Translator.exe_name()), SidecarModels::Translator { gguf: file(TRANSLATOR_GGUF) })?;
        supervisor.start(
            &file(SidecarKind::Tts.exe_name()),
            SidecarModels::Tts { base_lm: file(TTS_BASE_LM_GGUF), acoustic: file(TTS_ACOUSTIC_GGUF) },
        )?;
        let url = |kind: SidecarKind| supervisor.url(kind).ok_or_else(|| format!("dubpipe: {} has no url", kind.name()));
        let translator = Translator::new(&url(SidecarKind::Translator)?)?;
        let tts = Tts::new(&url(SidecarKind::Tts)?)?;
        let asr = Asr::new(&url(SidecarKind::Asr)?)?;
        turns::init_runtime(&file(ORT_DLL))?;
        let separator = Separator::open(&file(SEPARATOR_ONNX), pack_dir).map_err(|e| format!("dubpipe: separator: {e}"))?;
        let encoder = SpeakerEncoder::load(&file(SPEAKER_ENCODER_ONNX))?;
        let instrument = want_instrument.then(|| Instrument::load(&instrument_file)).transpose()?;
        Ok(Self { supervisor, translator, tts, asr, separator, encoder, source_language: None, about: None, buffer: None, style_segment, handle, instrument, anchor })
    }

    /// What the stream is (name, synopsis), from the player's metadata.
    pub fn set_about(&mut self, about: Option<String>) {
        self.about = about.filter(|s| !s.trim().is_empty()).map(|s| s.chars().take(ABOUT_MAX_CHARS).collect());
    }

    /// Whisper's full name of the detected source language ("japanese"),
    /// once the first window has been seen.
    pub fn source_language(&self) -> Option<&str> {
        self.source_language.as_deref()
    }

    /// The window `[start_s, start_s + len_s)` of `url`, dubbed: interleaved
    /// stereo s16 at 48 kHz, exactly `len_s` long (shorter at the stream's
    /// end). `lines` receives what was dubbed.
    pub fn produce(&mut self, url: &str, start_s: f64, len_s: f64, lines: &mut Vec<Line>) -> Result<Vec<i16>, ProduceError> {
        for kind in [SidecarKind::Translator, SidecarKind::Tts, SidecarKind::Asr] {
            self.supervisor.check(kind)?;
        }
        let window_start = (start_s * RATE as f64) as u64;
        let window_end = ((start_s + len_s) * RATE as f64) as u64;
        let fresh = self.extend_buffer(url, window_start, window_end)?;
        let buffer = self.buffer.as_mut().ok_or("dubpipe: no buffer")?;
        if buffer.end_frame() <= window_start {
            return Err(ProduceError::NotReady(format!("dubpipe: nothing decoded at {start_s:.1} s")));
        }

        let language = match &self.source_language {
            Some(l) => l.clone(),
            None => {
                let probe: Vec<i16> = buffer.vocals16.iter().map(|&s| (s * 32767.0) as i16).collect();
                let detected = self.asr.detect_language(&probe)?;
                if detected == TARGET_LANGUAGE_WHISPER {
                    return Err(ProduceError::Failed(format!("dubpipe: the dialogue is already {TARGET_LANGUAGE}")));
                }
                tracing::info!("dubpipe: source language {detected}");
                self.source_language = Some(detected.clone());
                detected
            }
        };
        let source_name = capitalized(&language);

        // Turns to dub now: those starting in the window, plus, on a fresh
        // buffer, those straddling its start. Window bounds in the buffer's
        // relative milliseconds.
        let origin_ms = buffer.origin_ms();
        let from_ms = if fresh { 0 } else { (window_start * 1000 / RATE as u64) as i64 - origin_ms };
        let to_ms = (window_end * 1000 / RATE as u64) as i64 - origin_ms;
        let due: Vec<Turn> = buffer
            .turns
            .iter()
            .filter(|t| (from_ms..to_ms).contains(&t.start_ms) && !buffer.produced.contains(&((t.start_ms + origin_ms) as u32)))
            .cloned()
            .collect();
        for (i, turn) in due.iter().enumerate() {
            let t0 = Instant::now();
            buffer.produced.insert((turn.start_ms + origin_ms) as u32);
            let after = buffer.turns.partition_point(|t| t.start_ms <= turn.start_ms);
            let room = room_ms(turn, &buffer.turns[after..]);
            let at = buffer.frame_of_relative_ms(turn.start_ms);
            // The line's own separated voice: what the recognizer hears and,
            // as the style segment, what the take's delivery follows.
            let style16 = to_i16(slice(&buffer.vocals16, ASR_RATE, turn.start_ms, turn.end_ms));
            let heard = self.asr.transcribe(&style16, &language, self.about.as_deref())?;
            // Where the line's time goes (ms): hear, translate, instrument, takes.
            let mut stage_ms = [t0.elapsed().as_millis(), 0, 0, 0];
            let src = heard.text;
            // Heard and translated side by side in the log: a wrong line is
            // either misheard here or mistranslated below, and only this tells which.
            tracing::info!("dubpipe: heard {:>7.1}s {src:?}", (turn.start_ms + origin_ms) as f64 / 1000.0);
            // The line's voiced runs and which of them are a sound without a
            // word: the handle's rests, and the spoken time the translator
            // writes to.
            let runs: Vec<(f64, f64)> = turns::phrases(&buffer.voiced, turn.start_ms, turn.end_ms).into_iter().map(|(s, e)| (s as f64 / 1000.0, e as f64 / 1000.0)).collect();
            let nonverbal = dubhandle::nonverbal_phrases(&runs, &heard.token_points_s);
            let spoken_ms: u32 = runs.iter().zip(&nonverbal).filter(|(_, &nv)| !nv).map(|((s, e), _)| ((e - s) * 1000.0).round() as u32).sum();
            let mut line = Line {
                start_ms: (turn.start_ms + origin_ms) as u32,
                end_ms: (turn.end_ms + origin_ms) as u32,
                speaker: turn.speaker.clone(),
                src: src.clone(),
                en: String::new(),
                fit: String::new(),
                wer: 0.0,
                gain: 1.0,
                ms: 0,
                handle: String::new(),
            };
            // A turn without a line to speak keeps its original voice: a
            // scream, a laugh, a breath the separator kept, is not dubbed away.
            // The bed IS the original there: nothing to do but record it.
            let keep_original = |_buffer: &mut Buffer, line: &mut Line, why: &str| {
                line.fit = format!("original: {why}");
            };
            // Only a turn with nothing to SAY keeps its original voice; a
            // translator failure is a failure, never Japanese in the dub.
            let outcome: Result<(), &str> = if src.chars().count() < MIN_TURN_TEXT_CHARS || is_non_speech(&src) {
                Err("non-speech")
            } else {
                let context: Vec<String> = buffer.history.iter().rev().take(CONTEXT_TURNS).rev().cloned().collect();
                let translate_t0 = Instant::now();
                let en = self.translator.translate(&src, &context, &source_name, TARGET_LANGUAGE, budget_syllables(spoken_ms, room), self.about.as_deref())?;
                stage_ms[1] = translate_t0.elapsed().as_millis();
                buffer.history.push(src.clone());
                if en.is_empty() || is_non_speech(&en) {
                    tracing::warn!("dubpipe: nothing to say for {src:?} (translated as {en:?})");
                    Err("nothing to say")
                } else {
                    line.en = en;
                    Ok(())
                }
            };
            if let Err(why) = outcome {
                keep_original(buffer, &mut line, why);
            } else {
                let Some(reference) = speaker_reference(&buffer.turns, &buffer.vocals16, turn.start_ms, turn.end_ms, REFERENCE_MAX_S) else {
                    return Err(ProduceError::Failed(format!("dubpipe: no reference for the turn at {} ms", line.start_ms)));
                };
                // What the model is asked for: the delivery handle of the
                // source line's runs, then the line. `line.en` stays the
                // plain line: it is the subtitle and the WER truth.
                let handle = if self.handle { dubhandle::source_handle(&runs, &nonverbal, &line.en, 1.0) } else { None };
                if self.handle && handle.is_none() {
                    tracing::info!("dubpipe: no handle for {:?}: runs {runs:?}, non-verbal {nonverbal:?}, token points {:?}", line.en, heard.token_points_s);
                }
                let (spoken, max_s) = match &handle {
                    Some(handle) => {
                        line.handle = handle.prefix();
                        (handle.spoken(&line.en), runaway_s(room, Some(handle.span_s)))
                    }
                    None => (line.en.clone(), runaway_s(room, None)),
                };
                let style = self.style_segment.then_some(style16.as_slice());
                // the voice as the model's instrument: from the same reference the take clones
                let instrument_t0 = Instant::now();
                let patches = match self.instrument.as_mut() {
                    Some(instrument) => Some(instrument.patches(&reference)?),
                    None => None,
                };
                stage_ms[2] = instrument_t0.elapsed().as_millis();
                let takes_t0 = Instant::now();
                let verified = synthesize_verified(&self.tts, &self.asr, &spoken, &line.en, &to_i16(&reference), style, patches.as_deref(), max_s)?;
                stage_ms[3] = takes_t0.elapsed().as_millis();
                match verified {
                    Some(VerifiedTake { audio: take, wer, word_starts_s }) => {
                        let turn_ms = (turn.end_ms - turn.start_ms).max(0) as u32;
                        // The model holds the asked tempo inside a phrase but
                        // collapses the handle's rests, so between phrases the
                        // handle is the authority (`dubplace`): the word starts
                        // move with the trimmed lead-in, the phrases go back on
                        // their planned times, and only then is the take fitted.
                        let lead_s = sounding_range(&take).map_or(0.0, |(from, _)| from as f64 / RATE as f64);
                        let take = trim_silence(take);
                        let starts: Vec<f64> = word_starts_s.iter().map(|t| (t - lead_s).max(0.0)).collect();
                        let anchored = handle.as_ref().filter(|_| self.anchor).and_then(|handle| dubplace::anchor_phrases(&take, RATE, &starts, &handle.entries));
                        let was_anchored = anchored.is_some();
                        let (take, how) = fit(anchored.unwrap_or(take), turn_ms, room);
                        let original = slice(&buffer.vocals48, RATE, turn.start_ms, turn.end_ms);
                        let gain = level_gain(original, &take);
                        // The voice is removed for the WHOLE original line (an
                        // English take is usually shorter than it: the original
                        // voice must not come back for the rest of the line),
                        // and for as long as the take runs past it.
                        let removed = original.len().max(take.len());
                        swap_to_residual(&mut buffer.bed, &buffer.residual, at, removed);
                        duck(&mut buffer.bed, at, take.len(), DUCK_GAIN);
                        mix_into(&mut buffer.bed, &take, at, gain);
                        line.fit = if was_anchored { format!("{}, anchored", describe(&how)) } else { describe(&how) };
                        line.wer = wer;
                        line.gain = gain;
                    }
                    None => keep_original(buffer, &mut line, "every take ran away"),
                }
            }
            line.ms = t0.elapsed().as_millis();
            tracing::info!(
                "dubpipe: {}/{} {:>7.1}s {:<22} {} {} [{} ms: hear {} translate {} instrument {} takes {}]",
                i + 1, due.len(), line.start_ms as f64 / 1000.0, line.fit, line.handle, line.en, line.ms, stage_ms[0], stage_ms[1], stage_ms[2], stage_ms[3]
            );
            lines.push(line);
        }

        let first = (window_start - buffer.start_frame) as usize * 2;
        let last = ((window_end.min(buffer.end_frame()) - buffer.start_frame) as usize) * 2;
        Ok(buffer.bed[first..last].iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16).collect())
    }

    /// Make the buffer cover `[window_start, window_end + lookahead)`. Returns
    /// whether the buffer was started fresh (the window did not continue it).
    fn extend_buffer(&mut self, url: &str, window_start: u64, window_end: u64) -> Result<bool, ProduceError> {
        let lookahead = (LOOKAHEAD_S * RATE as f64) as u64;
        let margin = (SEPARATE_MARGIN_S * RATE as f64) as u64;
        let need_end = window_end + lookahead;
        let continues = self
            .buffer
            .as_ref()
            .is_some_and(|b| b.url == url && b.start_frame <= window_start && window_start <= b.end_frame());
        if !continues {
            if self.buffer.as_ref().is_some_and(|b| b.url != url) {
                // A new stream: its language is detected afresh.
                self.source_language = None;
            }
            let start = window_start.saturating_sub((LOOKBACK_S * RATE as f64) as u64);
            let (stems, got) = separate_span(&self.separator, url, start, need_end, margin)?;
            let mut buffer = Buffer {
                url: url.to_owned(),
                start_frame: start,
                bed: Vec::new(),
                residual: Vec::new(),
                vocals48: Vec::new(),
                vocals16: Vec::new(),
                voiced: Vec::new(),
                turns: Vec::new(),
                produced: HashSet::new(),
                history: Vec::new(),
            };
            append_stems(&mut buffer, stems);
            debug_assert_eq!(buffer.end_frame(), got);
            resegment(&mut buffer, &mut self.encoder)?;
            self.buffer = Some(buffer);
            return Ok(true);
        }
        let separator = &self.separator;
        let buffer = self.buffer.as_mut().ok_or("dubpipe: no buffer")?;
        if buffer.end_frame() < need_end {
            let (stems, _) = separate_span(separator, url, buffer.end_frame(), need_end, margin)?;
            append_stems(buffer, stems);
            // Drop what no reference or straddling turn can still need.
            let keep_from = window_start.saturating_sub((LOOKBACK_S * RATE as f64) as u64);
            if keep_from > buffer.start_frame {
                let frames = (keep_from - buffer.start_frame) as usize;
                buffer.bed.drain(..frames * 2);
                buffer.residual.drain(..frames * 2);
                buffer.vocals48.drain(..frames);
                buffer.vocals16.drain(..frames * ASR_RATE as usize / RATE as usize);
                buffer.start_frame = keep_from;
                let keep_ms = buffer.origin_ms() as u32;
                buffer.produced.retain(|&ms| ms >= keep_ms);
            }
            resegment(buffer, &mut self.encoder)?;
        }
        Ok(false)
    }
}

/// The buffer's voiced flags and turns, from its whole dialogue stem.
fn resegment(buffer: &mut Buffer, encoder: &mut SpeakerEncoder) -> Result<(), String> {
    let t0 = Instant::now();
    let stem = to_i16(&buffer.vocals16);
    buffer.voiced = turns::voiced_flags(&stem)?;
    buffer.turns = segment(&stem, &buffer.voiced, encoder)?;
    tracing::info!("dubpipe: segmented {:.1} s into {} turns in {} ms", stem.len() as f64 / ASR_RATE as f64, buffer.turns.len(), t0.elapsed().as_millis());
    Ok(())
}

/// Decode and separate `[start, end)` with `margin` frames of context on
/// each side (discarded). Returns the stems and the frame the decoded
/// audio actually reached (the stream may end sooner).
fn separate_span(separator: &Separator, url: &str, start: u64, end: u64, margin: u64) -> Result<(Stems, u64), ProduceError> {
    let from = start.saturating_sub(margin);
    let head = (start - from) as usize;
    let decode_t0 = Instant::now();
    let pcm = decode_pcm_as(url, from as f64 / RATE as f64, (end + margin - from) as f64 / RATE as f64, FORMAT).map_err(ProduceError::NotReady)?;
    let decode_ms = decode_t0.elapsed().as_millis();
    let frames = pcm.len() / 2;
    if frames <= head {
        return Err(ProduceError::NotReady(format!("dubpipe: decoded nothing past frame {start}")));
    }
    let left: Vec<f32> = pcm.iter().step_by(2).map(|&s| s as f32 / 32768.0).collect();
    let right: Vec<f32> = pcm.iter().skip(1).step_by(2).map(|&s| s as f32 / 32768.0).collect();
    let t0 = Instant::now();
    let separated = separator.separate([&left, &right], RATE)?;
    tracing::info!("dubpipe: separated {:.1} s in {} ms (decoded in {decode_ms} ms) for {:.1} s kept", frames as f64 / RATE as f64, t0.elapsed().as_millis(), (end - start) as f64 / RATE as f64);
    let wanted = (frames - head).min((end - start) as usize);
    let range = head..head + wanted;
    let mix: Vec<f32> = range.clone().flat_map(|i| [left[i], right[i]]).collect();
    let residual: Vec<f32> = range.clone().flat_map(|i| [separated.residual[0][i], separated.residual[1][i]]).collect();
    let vocals48: Vec<f32> = range.clone().map(|i| 0.5 * (separated.vocals[0][i] + separated.vocals[1][i])).collect();
    let vocals16 = decimate(&vocals48, RATE, ASR_RATE);
    Ok((Stems { mix, residual, vocals48, vocals16 }, start + wanted as u64))
}

struct Stems {
    mix: Vec<f32>,
    residual: Vec<f32>,
    vocals48: Vec<f32>,
    vocals16: Vec<f32>,
}

fn append_stems(buffer: &mut Buffer, stems: Stems) {
    buffer.bed.extend(stems.mix);
    buffer.residual.extend(stems.residual);
    buffer.vocals48.extend(stems.vocals48);
    buffer.vocals16.extend(stems.vocals16);
}

/// Crossfade for the swap from the original mix to the residual at a line's
/// edges: long enough to hide the seam, short enough to stay inside the
/// line's own onset and tail.
const SWAP_FADE_MS: u32 = 40;

/// Inside `[start_frame, start_frame + frames)` the bed becomes the residual,
/// crossfaded over [`SWAP_FADE_MS`] at both ends (the region just outside
/// the line keeps the original; inside it the voice is removed for the dub).
fn swap_to_residual(bed: &mut [f32], residual: &[f32], start_frame: usize, frames: usize) {
    let total = bed.len() / 2;
    let fade = (SWAP_FADE_MS as usize * RATE as usize / 1000).max(1);
    let from = start_frame.saturating_sub(fade);
    let to = (start_frame + frames + fade).min(total);
    for f in from..to {
        let w = if f < start_frame {
            (f - from) as f32 / fade as f32
        } else if f >= start_frame + frames {
            (to - f) as f32 / fade as f32
        } else {
            1.0
        };
        for c in 0..2 {
            let i = f * 2 + c;
            bed[i] = bed[i] * (1.0 - w) + residual[i] * w;
        }
    }
}

/// `[start_ms, end_ms)` of a mono buffer at `rate` whose first sample is the
/// buffer's origin (ms relative to it), clamped to the buffer.
fn slice(x: &[f32], rate: u32, start_ms: i64, end_ms: i64) -> &[f32] {
    let at = |ms: i64| ((ms.max(0) as u64 * rate as u64 / 1000) as usize).min(x.len());
    &x[at(start_ms)..at(end_ms)]
}

fn to_i16(x: &[f32]) -> Vec<i16> {
    x.iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16).collect()
}

/// 48 kHz -> 16 kHz by an exact 3:1 box average (the dialogue stem feeds
/// VAD, embeddings and ASR, none of which resolve above 8 kHz finely enough
/// for a sharper filter to matter; the bench's `soxr` path was not measured
/// against this, so the parity tests of `turns.rs` run on 16 kHz fixtures).
fn decimate(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    let step = (from / to) as usize;
    x.chunks_exact(step).map(|c| c.iter().sum::<f32>() / step as f32).collect()
}

/// Whisper writes what is not speech as a tag, "(laughs)", "[applause]",
/// "♪"; a line with nothing outside such tags carries no words to dub.
fn is_non_speech(text: &str) -> bool {
    let mut depth = 0usize;
    let mut words = false;
    for c in text.chars() {
        match c {
            '(' | '[' | '（' | '［' => depth += 1,
            ')' | ']' | '）' | '］' => depth = depth.saturating_sub(1),
            c if depth == 0 && c.is_alphanumeric() => words = true,
            _ => {}
        }
    }
    !words
}

fn capitalized(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn describe(how: &Fit) -> String {
    match how {
        Fit::Fits => "fits".into(),
        Fit::Stretched { ratio } => format!("stretched x{ratio:.2}"),
        Fit::Cut { room_s, needed_s } => format!("cut at {room_s:.2}s (needed {needed_s:.2}s)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pack_file_the_pipeline_opens_is_in_the_manifest() {
        for name in [TRANSLATOR_GGUF, TTS_BASE_LM_GGUF, TTS_ACOUSTIC_GGUF, ASR_MODEL, SEPARATOR_ONNX, SPEAKER_ENCODER_ONNX, ORT_DLL] {
            assert!(crate::packs::MANIFEST.iter().any(|f| f.name == name), "{name} missing from packs::MANIFEST");
        }
        // the instrument graph is optional until M3's trained projector exists:
        // the manifest gains it (size and sha) the day the real file is staged
        assert!(crate::packs::OPTIONAL_FILES.contains(&instrument::FILE) || crate::packs::MANIFEST.iter().any(|f| f.name == instrument::FILE));
    }

    #[test]
    fn slices_are_clamped_and_in_ms() {
        let x: Vec<f32> = (0..160).map(|i| i as f32).collect();
        assert_eq!(slice(&x, 16_000, 5, 6), &x[80..96]);
        assert_eq!(slice(&x, 16_000, -3, 2).len(), 32);
        assert_eq!(slice(&x, 16_000, 9, 20).len(), 16);
    }

    #[test]
    fn decimation_is_a_box_average() {
        let x = [3.0, 3.0, 3.0, 0.0, 6.0, 0.0, 1.0];
        assert_eq!(decimate(&x, 48_000, 16_000), vec![3.0, 2.0]);
    }

    #[test]
    fn non_speech_is_a_tag_and_nothing_else() {
        assert!(is_non_speech("(Scream)"));
        assert!(is_non_speech("[applause] ♪"));
        assert!(is_non_speech("（笑）"));
        assert!(is_non_speech(""));
        assert!(!is_non_speech("(sigh) Fine."));
        assert!(!is_non_speech("行かせて"));
    }

    #[test]
    fn language_names_are_capitalized_for_the_prompt() {
        assert_eq!(capitalized("japanese"), "Japanese");
        assert_eq!(capitalized(""), "");
    }

    /// The whole pipeline on the pack, offline: F6's windows 3 and 4 (90-150 s,
    /// inside the S5 sample span) through the pack's own sidecars and models.
    /// Needs the staged pack (`RILLIO_DUB_PACK_DIR`, default `E:\packs\dubbing\1`),
    /// F6 at `E:\datasets\g0\tomb-raider-king-s01e09.mkv` and libmpv next to
    /// the test exe. Writes `E:\datasets\s5\pipeline\dub-90-150.wav` + `lines.jsonl`.
    /// `cargo test --release --lib dubpipe::tests::live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_f6_windows() {
        let pack = std::env::var("RILLIO_DUB_PACK_DIR").unwrap_or_else(|_| r"E:\packs\dubbing\1".into());
        let _ = tracing_subscriber::fmt().with_env_filter("rillio_desktop_lib=info").with_writer(std::io::stderr).try_init();
        let out = std::path::PathBuf::from(r"E:\datasets\s5\pipeline");
        std::fs::create_dir_all(&out).unwrap();
        let t0 = Instant::now();
        let mut pipeline = Pipeline::open(Path::new(&pack), &out.join("logs")).expect("pipeline");
        eprintln!("pipeline open in {:.1} s", t0.elapsed().as_secs_f64());
        let url = r"E:\datasets\g0\tomb-raider-king-s01e09.mkv";
        let mut pcm = Vec::new();
        let mut lines = Vec::new();
        let mut wall = 0.0;
        for window in [3u32, 4] {
            let t = Instant::now();
            let audio = pipeline.produce(url, window as f64 * 30.0, 30.0, &mut lines).unwrap_or_else(|e| panic!("window {window}: {e:?}"));
            wall += t.elapsed().as_secs_f64();
            eprintln!("window {window}: {:.2} s of audio in {:.1} s", audio.len() as f64 / 96_000.0, t.elapsed().as_secs_f64());
            pcm.extend(audio);
        }
        eprintln!("{} lines, RTF {:.3}", lines.len(), wall / 60.0);
        for line in &lines {
            eprintln!("{:>7.1}s {:<24} wer {:.2} gain {:.2} {:>5} ms  {} {}", line.start_ms as f64 / 1000.0, line.fit, line.wer, line.gain, line.ms, line.handle, line.en);
        }
        std::fs::write(out.join("dub-90-150.wav"), crate::dubclients::wav_bytes(&pcm, RATE, 2)).unwrap();
        let jsonl: String = lines.iter().map(|l| serde_json::to_string(l).unwrap() + "\n").collect();
        std::fs::write(out.join("lines.jsonl"), jsonl).unwrap();
        assert!(!lines.is_empty(), "no lines dubbed");
    }

    #[test]
    fn swap_to_residual_crossfades_at_the_line_edges_only() {
        let bed_orig = vec![1.0f32; 2 * 48_000];
        let residual = vec![0.0f32; 2 * 48_000];
        let mut bed = bed_orig.clone();
        let fade = SWAP_FADE_MS as usize * 48;
        swap_to_residual(&mut bed, &residual, 20_000, 10_000);
        assert_eq!(bed[0], 1.0);
        assert_eq!(bed[(20_000 - fade - 1) * 2], 1.0);
        assert!((bed[(20_000 - fade / 2) * 2] - 0.5).abs() < 0.01);
        assert_eq!(bed[25_000 * 2 + 1], 0.0);
        assert!((bed[(30_000 + fade / 2) * 2] - 0.5).abs() < 0.01);
        assert_eq!(bed[(30_000 + fade + 1) * 2], 1.0);
    }

    #[test]
    fn a_runaway_is_bounded_by_the_handle_span_when_there_is_one() {
        assert!((runaway_s(4_400, None) - 13.2).abs() < 1e-4);
        assert!((runaway_s(4_400, Some(0.4)) - 1.8).abs() < 1e-4);
        // a long line in a short room: the room's bound still holds
        assert!((runaway_s(1_000, Some(5.0)) - 3.0).abs() < 1e-4);
    }

    #[test]
    fn lookahead_holds_the_longest_turn_and_its_room() {
        assert!(LOOKAHEAD_S >= 12.0 + 5.0);
        assert!(LOOKBACK_S >= 12.0);
    }
}
