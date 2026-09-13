//! Subtitle auto-sync: find the delay that lines an external subtitle track up
//! with the speech actually heard around the current position.
//!
//! The idea is ffsubsync's / alass's, without the alignment machinery: speech
//! and subtitles are both "on/off over time" signals, so the offset between
//! them is where their cross-correlation peaks. Nothing here understands
//! language, and nothing needs the picture.
//!
//! Pipeline (all local, all on one blocking worker thread):
//!   1. decode a window of audio around the position with a SHADOW mpv - the
//!      same second-instance trick as thumbs.rs, but `vid=no` + `ao=pcm`: mpv
//!      writes 16 kHz mono s16 to a WAV as fast as it decodes (`ao_pcm` is
//!      untimed). libmpv carries ffmpeg's decoders, so whatever plays, syncs
//!      (AC3, DTS, TrueHD, opus, ...), and no ffmpeg binary is shipped.
//!   2. mark speech per 20 ms frame with the WebRTC VAD (a GMM over spectral
//!      features, no model file; the detector ffsubsync defaults to), spread
//!      onto a 10 ms grid.
//!   3. build the same grid from the cue intervals and take the Pearson
//!      correlation at every candidate delay in ±30 s. The peak is the delay;
//!      how much it stands out over the runner-up says whether to trust it.
//!
//! The window is anchored at the current position on purpose: drift is not
//! always uniform (a scene cut out of one release, an ad break in another), so
//! the user presses Sync where it is wrong, and the answer is for HERE. More
//! window behind the cursor than ahead, because behind is what the torrent has
//! already downloaded.
//!
//! Scope: external (addon / local file) tracks only. Their cues are parsed by
//! the web renderer and handed over; embedded tracks are timed by mpv, which
//! exposes no cue list - and they are muxed against the video anyway.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use webrtc_vad::{SampleRate, Vad, VadMode};

use crate::mpv::{self, Mpv, MpvEvent};

/// Decode target. 16 kHz is what the VAD wants; mono s16 keeps the WAV tiny.
const SAMPLE_RATE: u32 = 16_000;
/// Correlation grid. 10 ms is finer than anyone can perceive and keeps the
/// search at a few thousand bins.
const BIN_MS: i64 = 10;
/// WebRTC VAD frame (10/20/30 ms are legal; 20 is its sweet spot).
const VAD_FRAME_MS: i64 = 20;
const VAD_FRAME_SAMPLES: usize = (SAMPLE_RATE as i64 * VAD_FRAME_MS / 1000) as usize;
/// Audio window around the position: mostly behind (already downloaded).
const WINDOW_BEFORE_S: f64 = 40.0;
const WINDOW_AFTER_S: f64 = 20.0;
/// Largest delay searched either way. Beyond this the subtitles are for a
/// different cut and no single offset will fix them.
const MAX_SHIFT_MS: i64 = 30_000;
/// The decode must finish inside this (a window in an undownloaded torrent
/// region stalls exactly here; 60 s of audio decodes in ~1-2 s otherwise).
const DECODE_TIMEOUT: Duration = Duration::from_secs(25);
/// Below this share of speech bins there is nothing to align against.
const MIN_SPEECH_FRACTION: f64 = 0.04;
/// Below this share of cue bins (over the shifted range) the track has no
/// lines near here.
const MIN_CUE_FRACTION: f64 = 0.02;
/// A peak correlation under this is noise, whatever its shape.
const MIN_PEAK: f64 = 0.15;
/// The peak must beat the best OTHER candidate (outside `PEAK_EXCLUSION_MS` of
/// it) by this much; equal peaks mean a rhythmic scene, not an answer.
const MIN_MARGIN: f64 = 0.04;
const PEAK_EXCLUSION_MS: i64 = 1_500;
/// Onset refinement (see [`refine_by_onsets`]): how far from the coarse peak
/// to look, how close a cue start must land to a speech onset to count, and
/// the shortest voiced run that counts as an onset (shorter is VAD chatter).
const REFINE_RANGE_MS: i64 = 800;
const ONSET_TOLERANCE_MS: i64 = 150;
const MIN_ONSET_RUN_BINS: usize = 15;
/// Unvoiced gaps up to this long inside a run are closed before onsets are
/// read (a VAD dropout mid-word is not a new word).
const ONSET_GAP_FILL_BINS: usize = 6;
/// Fewer matched cue starts than this and the refinement is not evidence.
const MIN_ONSET_MATCHES: usize = 3;

/// One press of Sync, as the web layer sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum Outcome {
    /// A confident match: set the track's delay to `delay_ms`.
    Synced { delay_ms: i64, confidence: f64, margin: f64, speech_fraction: f64 },
    /// The audio window holds (almost) no speech - try another spot.
    NoDialogue { speech_fraction: f64 },
    /// The track has no lines anywhere near the window.
    NoCues,
    /// Speech and lines exist but no offset stands out (music, a rhythmic
    /// scene, subtitles for a different cut).
    Ambiguous { delay_ms: i64, confidence: f64, margin: f64 },
}

/// One sync at a time: a second press while the shadow decodes would spawn a
/// second decoder against the same stream.
static BUSY: AtomicBool = AtomicBool::new(false);

struct BusyGuard;

impl BusyGuard {
    fn acquire() -> Result<Self, String> {
        if BUSY.swap(true, Ordering::AcqRel) {
            return Err("autosync: already running".into());
        }
        Ok(Self)
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

/// Sync the external subtitle track (`cues` = its `[start_ms, end_ms]`
/// intervals, in subtitle time) to the speech around `position_ms` of `url`.
#[tauri::command]
pub async fn subtitles_autosync(
    app: tauri::AppHandle,
    url: String,
    position_ms: f64,
    cues: Vec<(i64, i64)>,
) -> Result<Outcome, String> {
    let url = crate::thumbs::resolve_shadow_url(&app, &url)?;
    crate::thumbs::validate_url(&url)?;
    if !position_ms.is_finite() || position_ms < 0.0 {
        return Err(format!("autosync: bad position {position_ms}"));
    }
    if cues.is_empty() {
        return Err("autosync: the track has no cues".into());
    }
    let guard = BusyGuard::acquire()?;
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let _guard = guard;
        run(&url, position_ms, &cues)
    })
    .await
    .map_err(|e| format!("autosync: worker died: {e}"))?;
    if let Err(e) = &outcome {
        // The web layer toasts it; the shell log is where it gets diagnosed.
        tracing::warn!("{e}");
    }
    outcome
}

/// The blocking pipeline: decode, detect, correlate.
fn run(url: &str, position_ms: f64, cues: &[(i64, i64)]) -> Result<Outcome, String> {
    let start_s = (position_ms / 1000.0 - WINDOW_BEFORE_S).max(0.0);
    let length_s = position_ms / 1000.0 - start_s + WINDOW_AFTER_S;
    let window_start_ms = (start_s * 1000.0).round() as i64;

    let t0 = Instant::now();
    let samples = decode_pcm(url, start_s, length_s)?;
    let decoded = t0.elapsed();
    if samples.len() < VAD_FRAME_SAMPLES * 50 {
        // Under a second of audio: the window fell off the end of the file or
        // the stream produced nothing usable.
        return Err(format!(
            "autosync: only {} ms of audio decoded at {start_s:.1}s",
            samples.len() as u64 * 1000 / SAMPLE_RATE as u64
        ));
    }

    let t1 = Instant::now();
    let speech = speech_bins(&samples)?;
    let outcome = correlate(&speech, window_start_ms, cues);
    tracing::info!(
        "autosync: {:.1}s of audio at {start_s:.1}s decoded in {} ms, analysed in {} ms -> {outcome:?}",
        samples.len() as f64 / SAMPLE_RATE as f64,
        decoded.as_millis(),
        t1.elapsed().as_millis(),
    );
    Ok(outcome)
}

/// Decode `[start_s, start_s + length_s)` of `url` to 16 kHz mono s16 through
/// a shadow mpv writing a WAV. Synchronous; returns once mpv reports END_FILE
/// (the pcm ao is untimed, so this runs at decode speed, not playback speed).
fn decode_pcm(url: &str, start_s: f64, length_s: f64) -> Result<Vec<i16>, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let wav = std::env::temp_dir().join(format!("rillio-autosync-{nanos}.wav"));
    let wav_path = wav.to_string_lossy().to_string();

    let result = (|| {
        let mpv = Mpv::load(&mpv::default_dll_path())?;
        for (name, value) in [
            ("vo", "null"),
            ("vid", "no"),
            ("sid", "no"),
            ("ao", "pcm"),
            ("ao-pcm-file", wav_path.as_str()),
            ("ao-pcm-waveheader", "yes"),
            ("audio-samplerate", "16000"),
            ("audio-channels", "mono"),
            ("audio-format", "s16"),
            ("start", &format!("{start_s:.3}")),
            ("length", &format!("{length_s:.3}")),
            // An exact start matters here (a keyframe-imprecise one would
            // shift the whole window and land in the answer).
            ("hr-seek", "yes"),
            ("pause", "no"),
            ("keep-open", "no"),
            ("idle", "yes"),
            ("gapless-audio", "no"),
            ("audio-pitch-correction", "no"),
            ("config", "no"),
            ("load-scripts", "no"),
            ("ytdl", "no"),
            ("osc", "no"),
            ("cache", "yes"),
            ("demuxer-max-bytes", "64MiB"),
        ] {
            // Every one of these is load-bearing (a wrong sample rate is a
            // wrong answer), so an option this libmpv refuses is a failure,
            // not a warning.
            mpv.set_option(name, value)
                .map_err(|e| format!("autosync: mpv option {name}={value}: {e}"))?;
        }
        mpv.initialize().map_err(|e| format!("autosync: mpv init: {e}"))?;
        mpv.command(&["loadfile", url]).map_err(|e| format!("autosync: loadfile: {e}"))?;

        let deadline = Instant::now() + DECODE_TIMEOUT;
        loop {
            match mpv.wait_event(0.5) {
                MpvEvent::EndFile { reason, error } => {
                    // mpv_end_file_reason: 0 EOF, 2 STOP, 3 QUIT, 4 ERROR, 5 REDIRECT.
                    if reason == 4 {
                        return Err(format!("autosync: decode failed: {}", mpv.error_string(error)));
                    }
                    break;
                }
                MpvEvent::Shutdown => return Err("autosync: mpv shut down mid-decode".into()),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err("autosync: decode timed out (is this part of the file downloaded?)".into());
            }
        }
        // Destroying the instance closes the ao, which is what finalises the
        // WAV header; read only after that.
        drop(mpv);
        let bytes = std::fs::read(&wav).map_err(|e| format!("autosync: reading {wav_path}: {e}"))?;
        parse_wav_s16_mono(&bytes)
    })();
    let _ = std::fs::remove_file(&wav);
    result
}

/// Minimal RIFF/WAVE reader for exactly what the shadow writes: PCM, mono,
/// 16-bit, [`SAMPLE_RATE`]. Anything else is a bug upstream and fails loud.
fn parse_wav_s16_mono(bytes: &[u8]) -> Result<Vec<i16>, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!("autosync: not a WAV ({} bytes)", bytes.len()));
    }
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let mut pos = 12;
    let mut format_ok = false;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let declared = u32_at(pos + 4) as usize;
        let body = pos + 8;
        match id {
            b"fmt " => {
                if body + 16 > bytes.len() {
                    return Err("autosync: truncated fmt chunk".into());
                }
                let (tag, channels, rate, bits) =
                    (u16_at(body), u16_at(body + 2), u32_at(body + 4), u16_at(body + 14));
                // mpv's ao_pcm writes WAVE_FORMAT_EXTENSIBLE (0xFFFE), whose
                // real format is the first two bytes of the sub-format GUID
                // at offset 24 of the chunk; plain PCM (1) is accepted too.
                let pcm = match tag {
                    1 => true,
                    0xFFFE => declared >= 26 && body + 26 <= bytes.len() && u16_at(body + 24) == 1,
                    _ => false,
                };
                if !pcm || channels != 1 || rate != SAMPLE_RATE || bits != 16 {
                    return Err(format!(
                        "autosync: unexpected WAV format tag={tag:#x} ch={channels} rate={rate} bits={bits}"
                    ));
                }
                format_ok = true;
            }
            b"data" => {
                if !format_ok {
                    return Err("autosync: data chunk before fmt".into());
                }
                // A writer that could not seek back leaves 0 (or 0xFFFFFFFF)
                // in the size field; the samples still run to the end of file.
                let available = bytes.len() - body;
                let len = if declared == 0 || declared > available { available } else { declared };
                return Ok(bytes[body..body + len / 2 * 2]
                    .chunks_exact(2)
                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                    .collect());
            }
            _ => {}
        }
        // Chunks are word-aligned; a bogus size past the end stops the walk.
        pos = body.saturating_add(declared.saturating_add(declared & 1));
    }
    Err("autosync: WAV has no data chunk".into())
}

/// Speech flags on the 10 ms grid, from the VAD's 20 ms verdicts.
fn speech_bins(samples: &[i16]) -> Result<Vec<bool>, String> {
    let mut vad = Vad::new_with_rate_and_mode(SampleRate::Rate16kHz, VadMode::Aggressive);
    let per_frame = (VAD_FRAME_MS / BIN_MS) as usize;
    let mut bins = Vec::with_capacity(samples.len() / VAD_FRAME_SAMPLES * per_frame);
    for frame in samples.chunks_exact(VAD_FRAME_SAMPLES) {
        let voiced = vad
            .is_voice_segment(frame)
            .map_err(|_| "autosync: VAD rejected a frame".to_string())?;
        bins.extend(std::iter::repeat(voiced).take(per_frame));
    }
    Ok(bins)
}

/// The pure half: `speech[i]` covers video time `window_start_ms + i*BIN_MS`;
/// `cues` are subtitle-time intervals. A cue is shown at video time `v` when it
/// covers `v - delay` (the renderer's rule), so candidate delay `d` scores the
/// correlation of `speech[i]` against `cue(window_start + i*BIN - d)`.
fn correlate(speech: &[bool], window_start_ms: i64, cues: &[(i64, i64)]) -> Outcome {
    let n = speech.len();
    let speech_count = speech.iter().filter(|&&s| s).count();
    let speech_fraction = speech_count as f64 / n.max(1) as f64;
    if n == 0 || speech_fraction < MIN_SPEECH_FRACTION {
        return Outcome::NoDialogue { speech_fraction };
    }

    // Cue grid over subtitle time [window_start - MAX_SHIFT, window_end + MAX_SHIFT).
    let shift_bins = (MAX_SHIFT_MS / BIN_MS) as usize;
    let m = n + 2 * shift_bins;
    let grid_start = window_start_ms - MAX_SHIFT_MS;
    let mut cue_grid = vec![false; m];
    for &(start, end) in cues {
        if end <= start {
            continue;
        }
        let lo = ((start - grid_start) / BIN_MS).max(0);
        let hi = ((end - grid_start + BIN_MS - 1) / BIN_MS).min(m as i64);
        if lo < hi {
            cue_grid[lo as usize..hi as usize].iter_mut().for_each(|b| *b = true);
        }
    }
    let cue_fraction = cue_grid.iter().filter(|&&c| c).count() as f64 / m as f64;
    if cue_fraction < MIN_CUE_FRACTION {
        return Outcome::NoCues;
    }

    // Pearson r per shift. Speech is mean-centred once (so its dot with the
    // 0/1 cue slice is already the covariance numerator); the cue slice's
    // variance comes from a prefix count.
    let mean_a = speech_fraction;
    let a: Vec<f64> = speech.iter().map(|&s| if s { 1.0 - mean_a } else { -mean_a }).collect();
    let var_a: f64 = a.iter().map(|x| x * x).sum();
    let mut prefix = vec![0usize; m + 1];
    for (i, &c) in cue_grid.iter().enumerate() {
        prefix[i + 1] = prefix[i] + c as usize;
    }

    // Shift s (grid offset) corresponds to delay d = MAX_SHIFT - s*BIN:
    // cue index for speech bin i is i + s, and grid index 0 is subtitle time
    // window_start - MAX_SHIFT, i.e. delay +MAX_SHIFT at s = 0.
    let scores: Vec<f64> = (0..=2 * shift_bins)
        .map(|s| {
            let ones = (prefix[s + n] - prefix[s]) as f64;
            let var_c = ones - ones * ones / n as f64;
            if var_c <= 0.0 || var_a <= 0.0 {
                return 0.0;
            }
            let dot: f64 = (0..n).filter(|&i| cue_grid[i + s]).map(|i| a[i]).sum();
            dot / (var_a * var_c).sqrt()
        })
        .collect();

    let (best_shift, &peak) = scores
        .iter()
        .enumerate()
        .max_by(|x, y| x.1.total_cmp(y.1))
        .expect("scores is non-empty");
    let delay_ms = MAX_SHIFT_MS - best_shift as i64 * BIN_MS;
    let exclusion = (PEAK_EXCLUSION_MS / BIN_MS) as usize;
    let runner_up = scores
        .iter()
        .enumerate()
        .filter(|(s, _)| s.abs_diff(best_shift) > exclusion)
        .map(|(_, &v)| v)
        .fold(f64::NEG_INFINITY, f64::max);
    let margin = if runner_up.is_finite() { peak - runner_up } else { peak };

    if peak < MIN_PEAK || margin < MIN_MARGIN {
        Outcome::Ambiguous { delay_ms, confidence: peak, margin }
    } else {
        let delay_ms = refine_by_onsets(speech, window_start_ms, cues, delay_ms);
        Outcome::Synced { delay_ms, confidence: peak, margin, speech_fraction }
    }
}

/// Cue starts against speech onsets, around the coarse answer.
///
/// Interval overlap finds the offset only to within the cue's slack: subtitles
/// stay up past the last word (and often come up a touch early), so the
/// overlap score is a plateau as wide as that slack and noise picks an edge
/// of it. Subtitle STARTS, though, sit on the first word. Within
/// `REFINE_RANGE_MS` of the coarse peak, the delay that lands the most cue
/// starts on a speech onset is the one the viewer wants; ties go to the
/// candidate nearest the coarse answer.
fn refine_by_onsets(speech: &[bool], window_start_ms: i64, cues: &[(i64, i64)], coarse_ms: i64) -> i64 {
    // Close short dropouts so a word reads as one run, then take the first
    // bin of every run long enough to be a word, not a click.
    let mut filled = speech.to_vec();
    let mut i = 0;
    while i < filled.len() {
        if !filled[i] {
            let gap_start = i;
            while i < filled.len() && !filled[i] {
                i += 1;
            }
            let inside = gap_start > 0 && i < filled.len();
            if inside && i - gap_start <= ONSET_GAP_FILL_BINS {
                filled[gap_start..i].iter_mut().for_each(|b| *b = true);
            }
        } else {
            i += 1;
        }
    }
    let mut onsets: Vec<i64> = Vec::new();
    let mut i = 0;
    while i < filled.len() {
        if filled[i] {
            let run_start = i;
            while i < filled.len() && filled[i] {
                i += 1;
            }
            if i - run_start >= MIN_ONSET_RUN_BINS {
                onsets.push(window_start_ms + run_start as i64 * BIN_MS);
            }
        } else {
            i += 1;
        }
    }
    if onsets.is_empty() {
        return coarse_ms;
    }
    let window_end_ms = window_start_ms + speech.len() as i64 * BIN_MS;
    // Cue starts that can land inside the window for some candidate delay.
    let starts: Vec<i64> = cues
        .iter()
        .filter(|(s, e)| e > s)
        .map(|&(s, _)| s)
        .filter(|&s| {
            s + coarse_ms + REFINE_RANGE_MS >= window_start_ms && s + coarse_ms - REFINE_RANGE_MS < window_end_ms
        })
        .collect();
    if starts.is_empty() {
        return coarse_ms;
    }
    // Distance from a cue start (in video time) to the nearest onset.
    let onset_gap = |video_ms: i64| -> i64 {
        let idx = onsets.partition_point(|&o| o < video_ms);
        let after = onsets.get(idx).map(|&o| o - video_ms);
        let before = idx.checked_sub(1).map(|j| video_ms - onsets[j]);
        [after, before].into_iter().flatten().min().unwrap_or(i64::MAX)
    };
    // Triangular score: a start exactly on an onset is worth the full
    // tolerance, one at the tolerance edge nothing, so the exact alignment
    // beats every near-miss instead of tying with it.
    let score_at = |d: i64| -> (i64, usize) {
        starts.iter().fold((0, 0), |(score, matched), &s| {
            let gap = onset_gap(s + d);
            if gap <= ONSET_TOLERANCE_MS {
                (score + ONSET_TOLERANCE_MS - gap, matched + 1)
            } else {
                (score, matched)
            }
        })
    };
    let mut best = (coarse_ms, score_at(coarse_ms));
    let steps = REFINE_RANGE_MS / BIN_MS;
    for k in -steps..=steps {
        let d = coarse_ms + k * BIN_MS;
        let (score, matched) = score_at(d);
        let closer = (d - coarse_ms).abs() < (best.0 - coarse_ms).abs();
        if score > best.1 .0 || (score == best.1 .0 && closer) {
            best = (d, (score, matched));
        }
    }
    if best.1 .1 < MIN_ONSET_MATCHES {
        return coarse_ms;
    }
    best.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift so the "random" scene is the same every run.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A 60 s window of conversational speech: utterances of 0.6-3 s with
    /// pauses of 0.3-4 s, as ms intervals in VIDEO time from `start`.
    fn dialogue(rng: &mut Rng, start: i64, len: i64) -> Vec<(i64, i64)> {
        let mut out = Vec::new();
        let mut t = start + rng.below(1500) as i64;
        while t < start + len {
            let dur = 600 + rng.below(2400) as i64;
            out.push((t, (t + dur).min(start + len)));
            t += dur + 300 + rng.below(3700) as i64;
        }
        out
    }

    fn grid(intervals: &[(i64, i64)], start: i64, n: usize) -> Vec<bool> {
        let mut g = vec![false; n];
        for &(a, b) in intervals {
            for i in 0..n {
                let t = start + i as i64 * BIN_MS;
                if t >= a && t < b {
                    g[i] = true;
                }
            }
        }
        g
    }

    const WINDOW_START: i64 = 600_000;
    const WINDOW_LEN: i64 = 60_000;
    const N: usize = (WINDOW_LEN / BIN_MS) as usize;

    /// Subtitles that run EARLY by 1.73 s (cue time = speech time - 1730),
    /// with cue edges jittered like real timing and the VAD flipping 10% of
    /// bins, must come back as delay +1730 (the renderer shows cue at
    /// v - delay). Cues exist across the whole "film" so the shifted range is
    /// populated, like a real track.
    #[test]
    fn finds_a_known_offset_through_noise() {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        let speech_iv = dialogue(&mut rng, WINDOW_START - 60_000, WINDOW_LEN + 120_000);
        let cues: Vec<(i64, i64)> = speech_iv
            .iter()
            .map(|&(a, b)| {
                let ja = rng.below(300) as i64 - 150;
                let jb = rng.below(300) as i64 - 150;
                (a - 1730 + ja, b - 1730 + jb)
            })
            .collect();
        let mut speech = grid(&speech_iv, WINDOW_START, N);
        for s in speech.iter_mut() {
            if rng.below(10) == 0 {
                *s = !*s;
            }
        }
        match correlate(&speech, WINDOW_START, &cues) {
            Outcome::Synced { delay_ms, confidence, margin, .. } => {
                // ±150 ms of edge jitter bounds how well ANY method can place
                // the peak; the fine control exists for the remainder.
                assert!((delay_ms - 1730).abs() <= 100, "delay {delay_ms} (r={confidence:.3}, margin={margin:.3})");
                assert!(confidence > 0.5, "confidence {confidence}");
            }
            other => panic!("expected Synced, got {other:?}"),
        }
    }

    /// Subtitles that run LATE come back as a negative delay; sign is the
    /// whole point, a flipped one is worse than no answer.
    #[test]
    fn late_subtitles_give_a_negative_delay() {
        let mut rng = Rng(0xD1B54A32D192ED03);
        let speech_iv = dialogue(&mut rng, WINDOW_START - 60_000, WINDOW_LEN + 120_000);
        let cues: Vec<(i64, i64)> = speech_iv.iter().map(|&(a, b)| (a + 4200, b + 4200)).collect();
        let speech = grid(&speech_iv, WINDOW_START, N);
        match correlate(&speech, WINDOW_START, &cues) {
            Outcome::Synced { delay_ms, .. } => assert!((delay_ms + 4200).abs() <= 20, "delay {delay_ms}"),
            other => panic!("expected Synced, got {other:?}"),
        }
    }

    /// Real subtitles stay up well past the last word. That slack makes the
    /// overlap match a plateau; the start-edge refinement must still return
    /// the delay that puts each line on its first word (here: cues run 2.0 s
    /// early and linger 0.7 s after the speech ends).
    #[test]
    fn cue_tails_do_not_shift_the_answer() {
        let mut rng = Rng(0xA5A5A5A5DEADBEEF);
        let speech_iv = dialogue(&mut rng, WINDOW_START - 60_000, WINDOW_LEN + 120_000);
        let cues: Vec<(i64, i64)> = speech_iv.iter().map(|&(a, b)| (a - 2000, b - 2000 + 700)).collect();
        let speech = grid(&speech_iv, WINDOW_START, N);
        match correlate(&speech, WINDOW_START, &cues) {
            Outcome::Synced { delay_ms, .. } => assert!((delay_ms - 2000).abs() <= 20, "delay {delay_ms}"),
            other => panic!("expected Synced, got {other:?}"),
        }
    }

    /// Music / an action scene: (almost) no voiced bins - say so instead of
    /// fitting the cues to silence.
    #[test]
    fn no_speech_is_reported_not_guessed() {
        let mut rng = Rng(7);
        let cues = dialogue(&mut rng, WINDOW_START - 60_000, WINDOW_LEN + 120_000);
        let mut speech = vec![false; N];
        speech[100..200].iter_mut().for_each(|s| *s = true);
        assert!(matches!(correlate(&speech, WINDOW_START, &cues), Outcome::NoDialogue { .. }));
    }

    /// A track whose lines all sit elsewhere in the film (nothing within the
    /// shifted range) cannot be aligned here.
    #[test]
    fn cues_elsewhere_in_the_film_are_no_cues() {
        let mut rng = Rng(11);
        let speech = grid(&dialogue(&mut rng, WINDOW_START, WINDOW_LEN), WINDOW_START, N);
        let cues = dialogue(&mut rng, 0, 120_000);
        assert_eq!(correlate(&speech, WINDOW_START, &cues), Outcome::NoCues);
    }

    /// Strictly periodic speech against periodic cues: every period is as good
    /// a match as the next, and the answer must admit it.
    #[test]
    fn a_rhythmic_scene_is_ambiguous() {
        let period = 4_000;
        let speech_iv: Vec<(i64, i64)> =
            (0..40).map(|k| (WINDOW_START - 40_000 + k * period, WINDOW_START - 40_000 + k * period + 1_500)).collect();
        let cues: Vec<(i64, i64)> = speech_iv.iter().map(|&(a, b)| (a - 900, b - 900)).collect();
        let speech = grid(&speech_iv, WINDOW_START, N);
        assert!(matches!(correlate(&speech, WINDOW_START, &cues), Outcome::Ambiguous { .. }));
    }

    /// The reader takes exactly the shadow's output shape and refuses others.
    /// `tag` 1 is plain PCM; 0xFFFE is the EXTENSIBLE header mpv actually
    /// writes (22 extra bytes: cbSize, valid bits, channel mask, sub-format
    /// GUID whose first two bytes carry the real tag).
    #[test]
    fn wav_reader_reads_the_shadow_shape_only() {
        let build = |tag: u16, channels: u16, rate: u32, samples: &[i16], declared: u32| {
            let extensible = tag == 0xFFFE;
            let mut b = Vec::new();
            b.extend_from_slice(b"RIFF");
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(b"WAVE");
            b.extend_from_slice(b"fmt ");
            b.extend_from_slice(&(if extensible { 40u32 } else { 16u32 }).to_le_bytes());
            b.extend_from_slice(&tag.to_le_bytes());
            b.extend_from_slice(&channels.to_le_bytes());
            b.extend_from_slice(&rate.to_le_bytes());
            b.extend_from_slice(&(rate * 2).to_le_bytes());
            b.extend_from_slice(&2u16.to_le_bytes());
            b.extend_from_slice(&16u16.to_le_bytes());
            if extensible {
                b.extend_from_slice(&22u16.to_le_bytes()); // cbSize
                b.extend_from_slice(&16u16.to_le_bytes()); // valid bits
                b.extend_from_slice(&4u32.to_le_bytes()); // channel mask
                b.extend_from_slice(&1u16.to_le_bytes()); // sub-format: PCM
                b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71]);
            }
            // A LIST chunk in between, odd-sized (tests the word alignment).
            b.extend_from_slice(b"LIST");
            b.extend_from_slice(&3u32.to_le_bytes());
            b.extend_from_slice(&[1, 2, 3, 0]);
            b.extend_from_slice(b"data");
            b.extend_from_slice(&declared.to_le_bytes());
            for s in samples {
                b.extend_from_slice(&s.to_le_bytes());
            }
            b
        };
        let samples = [0i16, 1000, -1000, i16::MAX, i16::MIN];
        assert_eq!(parse_wav_s16_mono(&build(1, 1, 16_000, &samples, 10)).unwrap(), samples);
        assert_eq!(parse_wav_s16_mono(&build(0xFFFE, 1, 16_000, &samples, 10)).unwrap(), samples);
        // Unfinalised size field (0): samples run to end of file.
        assert_eq!(parse_wav_s16_mono(&build(1, 1, 16_000, &samples, 0)).unwrap(), samples);
        // Oversized size field: clamp to what is there.
        assert_eq!(parse_wav_s16_mono(&build(1, 1, 16_000, &samples, 999)).unwrap(), samples);
        assert!(parse_wav_s16_mono(&build(1, 2, 16_000, &samples, 10)).is_err());
        assert!(parse_wav_s16_mono(&build(1, 1, 44_100, &samples, 10)).is_err());
        assert!(parse_wav_s16_mono(&build(3, 1, 16_000, &samples, 10)).is_err()); // IEEE float
        assert!(parse_wav_s16_mono(b"not a wav at all").is_err());
    }

    /// The VAD path itself: a 200 Hz tone-with-harmonics burst reads as voiced
    /// where it plays and silence does not (sanity that the C library is
    /// linked and framed right, not a quality claim).
    #[test]
    fn vad_marks_a_voiced_burst_and_not_silence() {
        let secs = 2;
        let mut samples = vec![0i16; SAMPLE_RATE as usize * secs];
        for (i, s) in samples.iter_mut().enumerate().take(SAMPLE_RATE as usize) {
            let t = i as f64 / SAMPLE_RATE as f64;
            let v = (1..6).map(|h| (2.0 * std::f64::consts::PI * 200.0 * h as f64 * t).sin() / h as f64).sum::<f64>();
            *s = (v * 6000.0) as i16;
        }
        let bins = speech_bins(&samples).unwrap();
        assert_eq!(bins.len(), (secs as i64 * 1000 / BIN_MS) as usize);
        let first_half = bins[..bins.len() / 2].iter().filter(|&&b| b).count();
        let second_half = bins[bins.len() / 2..].iter().filter(|&&b| b).count();
        assert!(first_half > bins.len() / 4, "burst voiced bins: {first_half}");
        // The VAD keeps a short hangover after speech ends (~100 ms); beyond
        // that, silence must read as silence.
        assert!(second_half <= 20, "silence voiced bins: {second_half}");
    }
}
