//! Placing a synthesized line into the timeline (Stage 1 decisions D5 and
//! its refinement, `docs/dubbing/stage1-pipeline/README.md`): the room a line
//! may occupy, the translation length budget derived from it, the fit
//! (unchanged, time-stretched up to a bound, else cut with a fade), the
//! level match to the original dialogue, and the mix over the residual bed.
//! Ports `s5/dub_offline.py`'s `fit`/`rms`/mix 1:1 apart from the D5
//! refinement measured on the full-episode run: room runs to the next turn of
//! a DIFFERENT speaker, tolerates a short overlap into it, and the cut fade
//! is longer.
//!
//! Pure functions on 48 kHz mono f32 (the TTS output) and the stereo
//! interleaved timeline; no I/O, no clock.

pub const OUT_RATE: u32 = 48_000;
/// A line longer than its room is sped up by at most this factor (the bench
/// verified 1.15 by transcription; Michael's ear on the full run heard the
/// stretcher as "synthetic and chopped", so the bound is tighter).
pub const MAX_STRETCH: f64 = 1.10;
/// Isochrony's slow-down side: an English take shorter than the original
/// utterance could be slowed toward it. OFF (1.0): almost every English take
/// is shorter than its Japanese line, so the slow-down ran on nearly every
/// line and made the whole dub sound processed. A short line ends early and
/// the original's pause stands, as a studio would leave it; the translation
/// budget does the length matching upstream.
pub const MAX_SLOWDOWN: f64 = 1.0;
/// A take at least this fraction of the original's length is left alone.
pub const ISOCHRONY_TOLERANCE: f64 = 0.9;
/// Leading and trailing silence of a take is trimmed (the synthesizer adds
/// some) so the voice starts when the mouth does; below this level, on this
/// grid, is silence.
const SILENCE_DBFS: f32 = -45.0;
const SILENCE_FRAME_MS: u32 = 10;
/// Beyond the stretch bound the line is cut at its room with this fade.
pub const CUT_FADE_MS: u32 = 120;
/// A line may run this far into the next speaker's line before it counts as
/// needing a fit (fast exchanges overlap in real speech too).
pub const TOLERATED_OVERLAP_MS: u32 = 250;
/// Room for the last turn of a window when no later turn is known.
pub const TRAILING_ROOM_MS: u32 = 5_000;
/// The voice never sits more than this above the original dialogue's level.
pub const MAX_GAIN: f32 = 4.0;
/// Under a dubbed line the bed is ducked by this much (-8 dB): the separator
/// leaves the original voice ~15 dB down in the bed, audible under quiet
/// scenes ("Japanese bleeds in", Michael), and a mix ducks music and effects
/// under dialogue anyway. Ramped in and out over [`DUCK_RAMP_MS`].
pub const DUCK_GAIN: f32 = 0.4;
pub const DUCK_RAMP_MS: u32 = 60;
/// The translator's length budget is in SYLLABLES: the delivery handle deals
/// the English syllables into the source's spoken time, so a line fits when
/// its syllables at the corpus's tempo fit that time. The tempo is the
/// median `[line]` of the training rows (syllables per minute over the word
/// time; train.jsonl 2026-09-18, 49,907 rows: median 390, p25 360, p75
/// 450), so a budgeted line asks the model for the tempo it has seen most.
pub const SPOKEN_SPM: f64 = 390.0;
/// English runs 3.0 characters per syllable (the same rows, median): the
/// budget as the translator's prompt states it (G4 measured 13.7 characters
/// per second of natural speech, 4.6 syllables per second, the same rate).
pub const CHARS_PER_SYLLABLE: f64 = 3.0;
pub const MIN_BUDGET_SYLLABLES: usize = 4;
/// Silence guard in the RMS ratio (the bench's 1e-9 on float64).
const RMS_FLOOR: f64 = 1e-9;

/// WSOLA frame: 42.7 ms at 48 kHz, half-overlapped Hann.
const STRETCH_FRAME: usize = 2048;
const STRETCH_HOP: usize = STRETCH_FRAME / 2;
/// Search range around the nominal analysis position for the best-matching
/// waveform continuation (10.7 ms: more than a pitch period of any voice).
const STRETCH_TOLERANCE: usize = 512;

pub(crate) use crate::turns::Turn;

#[derive(Clone, Debug, PartialEq)]
pub enum Fit {
    Fits,
    Stretched { ratio: f64 },
    Cut { room_s: f64, needed_s: f64 },
}

/// Milliseconds from `turn`'s start that its line may occupy: up to the next
/// turn of a different speaker in `later` (turns after it, in order) plus the
/// tolerated overlap. Same-speaker turns may run together.
pub(crate) fn room_ms(turn: &Turn, later: &[Turn]) -> u32 {
    let next_other = later.iter().find(|t| t.speaker != turn.speaker).map(|t| t.start_ms);
    let end = next_other.map_or(turn.end_ms + TRAILING_ROOM_MS as i64, |start| start.max(turn.end_ms) + TOLERATED_OVERLAP_MS as i64);
    (end - turn.start_ms).max(0) as u32
}

/// The translator's length budget in syllables: the ORIGINAL line's spoken
/// time (its voiced runs, pauses and sounds excluded: what a dubbing
/// translator writes to) at the corpus tempo, never more than the room allows.
pub fn budget_syllables(spoken_ms: u32, room_ms: u32) -> usize {
    MIN_BUDGET_SYLLABLES.max((spoken_ms.min(room_ms) as f64 / 1000.0 * SPOKEN_SPM / 60.0) as usize)
}

/// A syllable budget in the characters the translator's prompt states.
pub fn budget_chars(syllables: usize) -> usize {
    (syllables as f64 * CHARS_PER_SYLLABLE).round() as usize
}

/// The sample range of `audio` between its leading and trailing silence;
/// `None` when no frame is loud.
pub fn sounding_range(audio: &[f32]) -> Option<(usize, usize)> {
    let frame = SILENCE_FRAME_MS as usize * OUT_RATE as usize / 1000;
    let threshold = 10f32.powf(SILENCE_DBFS / 20.0);
    let loud = |chunk: &[f32]| (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt() > threshold;
    let first = audio.chunks(frame).position(loud)?;
    let last = audio.chunks(frame).rposition(loud)?;
    Some((first * frame, ((last + 1) * frame).min(audio.len())))
}

/// `audio` without its leading and trailing silence.
pub fn trim_silence(audio: Vec<f32>) -> Vec<f32> {
    match sounding_range(&audio) {
        Some((a, b)) => audio[a..b].to_vec(),
        None => audio,
    }
}

/// Make `audio` (48 kHz mono) sit on the original utterance of `turn_ms`
/// within `room_ms`: a short take is slowed toward the original's length,
/// a long one sped up, a far too long one cut.
pub fn fit(audio: Vec<f32>, turn_ms: u32, room_ms: u32) -> (Vec<f32>, Fit) {
    let room = room_ms as usize * OUT_RATE as usize / 1000;
    let target = (turn_ms as usize * OUT_RATE as usize / 1000).min(room);
    if audio.len() <= room {
        if (audio.len() as f64) < target as f64 * ISOCHRONY_TOLERANCE {
            let ratio = (audio.len() as f64 / target as f64).max(1.0 / MAX_SLOWDOWN);
            if ratio < 1.0 {
                return (stretch(&audio, ratio), Fit::Stretched { ratio });
            }
        }
        return (audio, Fit::Fits);
    }
    let ratio = audio.len() as f64 / room as f64;
    if ratio <= MAX_STRETCH {
        return (stretch(&audio, ratio), Fit::Stretched { ratio });
    }
    let fade = room.min(CUT_FADE_MS as usize * OUT_RATE as usize / 1000);
    let mut cut = audio[..room].to_vec();
    for (i, s) in cut[room - fade..].iter_mut().enumerate() {
        *s *= 1.0 - i as f32 / fade as f32;
    }
    (cut, Fit::Cut { room_s: room as f64 / OUT_RATE as f64, needed_s: audio.len() as f64 / OUT_RATE as f64 })
}

/// Time-scale `audio` by `ratio` (> 1 shortens) at constant pitch: WSOLA,
/// each synthesis frame taken from the analysis position near the nominal
/// one whose waveform best continues the previous frame.
pub fn stretch(audio: &[f32], ratio: f64) -> Vec<f32> {
    assert!(ratio > 0.0, "dubfit: stretch ratio {ratio}");
    if audio.len() < STRETCH_FRAME * 2 {
        return audio.to_vec();
    }
    let window: Vec<f32> = (0..STRETCH_FRAME)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / STRETCH_FRAME as f32).cos())
        .collect();
    let out_len = (audio.len() as f64 / ratio) as usize;
    let mut out = vec![0.0f32; out_len + STRETCH_FRAME];
    let mut norm = vec![0.0f32; out_len + STRETCH_FRAME];
    let last_start = audio.len() - STRETCH_FRAME;
    let mut prev_pos = 0usize;
    let mut k = 0usize;
    loop {
        let out_pos = k * STRETCH_HOP;
        if out_pos >= out_len {
            break;
        }
        let nominal = ((out_pos as f64 * ratio) as usize).min(last_start);
        let pos = if k == 0 {
            0
        } else {
            // The natural continuation of the previous frame, one hop on.
            let target = &audio[prev_pos + STRETCH_HOP..(prev_pos + STRETCH_HOP + STRETCH_FRAME).min(audio.len())];
            let lo = nominal.saturating_sub(STRETCH_TOLERANCE);
            let hi = (nominal + STRETCH_TOLERANCE).min(last_start);
            (lo..=hi)
                .max_by(|&a, &b| {
                    let ca = correlation(&audio[a..a + target.len()], target);
                    let cb = correlation(&audio[b..b + target.len()], target);
                    ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(nominal)
        };
        for i in 0..STRETCH_FRAME {
            out[out_pos + i] += audio[pos + i] * window[i];
            norm[out_pos + i] += window[i];
        }
        prev_pos = pos;
        k += 1;
    }
    out.truncate(out_len);
    for (s, n) in out.iter_mut().zip(&norm) {
        if *n > f32::EPSILON {
            *s /= *n;
        }
    }
    out
}

fn correlation(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn rms(x: &[f32]) -> f64 {
    if x.is_empty() {
        return RMS_FLOOR;
    }
    (x.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt() + RMS_FLOOR
}

/// Gain that brings `take` to the original dialogue's level, bounded.
pub fn level_gain(original: &[f32], take: &[f32]) -> f32 {
    if original.is_empty() {
        return 1.0;
    }
    (rms(original) / rms(take)).min(MAX_GAIN as f64) as f32
}

/// Lower the interleaved stereo `bed` to [`DUCK_GAIN`] over `frames` frames
/// from `start_frame`, with linear ramps at both ends, before a line is
/// mixed there.
pub fn duck(bed: &mut [f32], start_frame: usize, frames: usize, gain: f32) {
    let total = bed.len() / 2;
    let end = (start_frame + frames).min(total);
    if start_frame >= end {
        return;
    }
    let ramp = (DUCK_RAMP_MS as usize * OUT_RATE as usize / 1000).min((end - start_frame) / 2).max(1);
    for f in start_frame..end {
        let into = f - start_frame;
        let left = end - f;
        let edge = if into < ramp { into as f32 / ramp as f32 } else if left < ramp { left as f32 / ramp as f32 } else { 1.0 };
        let g = 1.0 - (1.0 - gain) * edge;
        bed[f * 2] *= g;
        bed[f * 2 + 1] *= g;
    }
}

/// Add `take` (mono) into both channels of the interleaved stereo `bed` from
/// frame `start`, clamped to full scale; the part past the bed's end is
/// dropped (the next window carries it, see the worker).
pub fn mix_into(bed: &mut [f32], take: &[f32], start_frame: usize, gain: f32) {
    let frames = bed.len() / 2;
    let n = take.len().min(frames.saturating_sub(start_frame));
    for (i, &s) in take[..n].iter().enumerate() {
        let f = (start_frame + i) * 2;
        bed[f] = (bed[f] + s * gain).clamp(-1.0, 1.0);
        bed[f + 1] = (bed[f + 1] + s * gain).clamp(-1.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(start_ms: i64, end_ms: i64, speaker: u32) -> Turn {
        Turn { start_ms, end_ms, speaker: format!("s0-{speaker}") }
    }

    fn sine(hz: f32, seconds: f32) -> Vec<f32> {
        (0..(seconds * OUT_RATE as f32) as usize)
            .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / OUT_RATE as f32).sin() * 0.5)
            .collect()
    }

    fn zero_crossings(x: &[f32]) -> usize {
        x.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count()
    }

    #[test]
    fn room_runs_to_the_next_other_speaker_plus_the_tolerated_overlap() {
        let t = turn(1000, 2000, 1);
        let later = [turn(2100, 2600, 1), turn(2800, 3500, 2), turn(4000, 4500, 3)];
        assert_eq!(room_ms(&t, &later), 1800 + TOLERATED_OVERLAP_MS);
        // Only same-speaker turns follow: the trailing room applies.
        assert_eq!(room_ms(&t, &later[..1]), 1000 + TRAILING_ROOM_MS);
        // The next other speaker starts inside this turn: room never shrinks below the turn itself.
        let overlapping = [turn(1500, 2500, 2)];
        assert_eq!(room_ms(&t, &overlapping), 1000 + TOLERATED_OVERLAP_MS);
    }

    #[test]
    fn budget_follows_the_spoken_time_capped_by_the_room_with_a_floor() {
        assert_eq!(budget_syllables(2000, 5000), 13);
        assert_eq!(budget_syllables(5000, 2000), 13);
        assert_eq!(budget_syllables(300, 5000), MIN_BUDGET_SYLLABLES);
        assert_eq!(budget_chars(13), 39);
    }

    #[test]
    fn fit_leaves_a_line_of_the_original_length_alone() {
        let audio = sine(220.0, 1.0);
        let (out, how) = fit(audio.clone(), 1050, 1500);
        assert_eq!(how, Fit::Fits);
        assert_eq!(out, audio);
    }

    #[test]
    fn fit_never_slows_a_short_take_while_the_slowdown_is_off() {
        let audio = sine(220.0, 1.0);
        assert_eq!(fit(audio.clone(), 1050, 3000).1, Fit::Fits);
        let (out, how) = fit(audio.clone(), 2000, 3000);
        assert_eq!(how, Fit::Fits);
        assert_eq!(out, audio);
    }

    #[test]
    fn trim_silence_keeps_the_voice_and_drops_the_edges() {
        let mut audio = vec![0.0f32; 4800];
        audio.extend(sine(220.0, 0.5));
        audio.extend(vec![0.0001f32; 9600]);
        let out = trim_silence(audio);
        assert!((out.len() as i64 - 24_000).abs() <= 480, "{}", out.len());
        assert_eq!(trim_silence(vec![0.0; 100]).len(), 100);
    }

    #[test]
    fn fit_stretches_within_the_bound_and_keeps_the_pitch() {
        let audio = sine(220.0, 2.0);
        let (out, how) = fit(audio.clone(), 2000, 1900);
        assert!(matches!(how, Fit::Stretched { ratio } if (ratio - 2.0 / 1.9).abs() < 1e-9), "{how:?}");
        let expected = 1900 * OUT_RATE as usize / 1000;
        assert!((out.len() as i64 - expected as i64).abs() <= 1, "{}", out.len());
        // Constant pitch: crossings per second unchanged (2 per cycle at 220 Hz).
        let per_s = zero_crossings(&out) as f32 / (out.len() as f32 / OUT_RATE as f32);
        assert!((per_s - 440.0).abs() < 4.0, "{per_s}");
        // Level preserved (the overlap-add is normalized).
        assert!((rms(&out) / rms(&audio) - 1.0).abs() < 0.05, "{}", rms(&out) / rms(&audio));
    }

    #[test]
    fn fit_cuts_beyond_the_bound_with_a_fade_to_silence() {
        let audio = vec![0.5f32; 2 * OUT_RATE as usize];
        let (out, how) = fit(audio, 2000, 1000);
        assert!(matches!(how, Fit::Cut { room_s, needed_s } if room_s == 1.0 && needed_s == 2.0), "{how:?}");
        assert_eq!(out.len(), OUT_RATE as usize);
        let fade = CUT_FADE_MS as usize * OUT_RATE as usize / 1000;
        assert_eq!(out[out.len() - fade - 1], 0.5);
        assert!(out[out.len() - 1].abs() < 1e-3);
        assert!((out[out.len() - fade / 2] - 0.25).abs() < 0.01);
    }

    #[test]
    fn level_gain_matches_rms_and_is_bounded() {
        let orig = vec![0.4f32; 100];
        let take = vec![0.2f32; 100];
        assert!((level_gain(&orig, &take) - 2.0).abs() < 1e-3);
        let quiet = vec![0.01f32; 100];
        assert_eq!(level_gain(&orig, &quiet), MAX_GAIN);
        assert_eq!(level_gain(&[], &take), 1.0);
    }

    #[test]
    fn duck_lowers_the_bed_under_the_line_with_ramps() {
        let mut bed = vec![1.0f32; 2 * OUT_RATE as usize];
        let ramp = DUCK_RAMP_MS as usize * OUT_RATE as usize / 1000;
        duck(&mut bed, 10_000, 20_000, DUCK_GAIN);
        assert_eq!(bed[0], 1.0);
        assert!((bed[(10_000 + ramp + 100) * 2] - DUCK_GAIN).abs() < 1e-6);
        assert!((bed[(10_000 + ramp / 2) * 2] - (1.0 - (1.0 - DUCK_GAIN) * 0.5)).abs() < 0.01);
        assert_eq!(bed[(30_000 + 10) * 2 + 1], 1.0);
        // Past the bed's end: no panic, clamped.
        duck(&mut bed, OUT_RATE as usize - 100, 1000, DUCK_GAIN);
    }

    #[test]
    fn mix_adds_to_both_channels_clamped_and_drops_the_overhang() {
        let mut bed = vec![0.1f32; 8];
        mix_into(&mut bed, &[0.5, 2.0, 0.5], 2, 1.0);
        assert_eq!(bed, vec![0.1, 0.1, 0.1, 0.1, 0.6, 0.6, 1.0, 1.0]);
        mix_into(&mut bed, &[0.5], 10, 1.0);
        assert_eq!(bed.len(), 8);
    }

    /// The stretch keeps a real take intelligible: a line from the resident
    /// TTS, sped up by the full bound, transcribed back by the resident ASR.
    /// `RILLIO_DUB_TTS_URL`, `RILLIO_DUB_ASR_URL`, `RILLIO_DUB_REFERENCE_WAV`.
    #[test]
    #[ignore]
    fn live_stretched_take_stays_faithful() {
        use crate::autosync::{parse_wav_s16, PcmFormat};
        use crate::dubclients::{wer, Asr, Tts};
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} not set"));
        let wav = std::fs::read(env("RILLIO_DUB_REFERENCE_WAV")).unwrap();
        let reference = parse_wav_s16(&wav, PcmFormat { rate: 16_000, channels: 1 }).unwrap();
        let text = "There is no way I am letting you take the last key without a fight.";
        let take = Tts::new(&env("RILLIO_DUB_TTS_URL")).unwrap().synthesize(text, &reference, None, None, 42, 12.0).unwrap();
        let asr = Asr::new(&env("RILLIO_DUB_ASR_URL")).unwrap();
        let to_16k = |x: &[f32]| -> Vec<i16> { x.iter().step_by(3).map(|&s| (s * 32767.0) as i16).collect() };
        let plain = wer(text, &asr.transcribe(&to_16k(&take), "en", None).unwrap().text);
        let shorter = stretch(&take, MAX_STRETCH);
        let heard = asr.transcribe(&to_16k(&shorter), "en", None).unwrap().text;
        let stretched = wer(text, &heard);
        eprintln!("take {:.2} s -> {:.2} s; wer plain {plain:.3}, stretched {stretched:.3}: {heard:?}", take.len() as f32 / 48000.0, shorter.len() as f32 / 48000.0);
        if let Ok(dir) = std::env::var("RILLIO_DUB_OUT_DIR") {
            // The pair for the ear: does the sped-up take still sound natural?
            let s16 = |x: &[f32]| -> Vec<i16> { x.iter().map(|&s| (s * 32767.0) as i16).collect() };
            std::fs::write(format!("{dir}/take-plain.wav"), crate::dubclients::wav_bytes(&s16(&take), OUT_RATE, 1)).unwrap();
            std::fs::write(format!("{dir}/take-stretched-1.15.wav"), crate::dubclients::wav_bytes(&s16(&shorter), OUT_RATE, 1)).unwrap();
        }
        assert!(stretched <= 0.2, "stretched take drifted: {heard:?}");
    }
}
