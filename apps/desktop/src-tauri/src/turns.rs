//! Speaker turns from a dialogue stem: activity by WebRTC VAD, identity by
//! GE2E speaker embeddings clustered per scene. A port of
//! `docs/dubbing/stage1-pipeline/g5/turns.py` (the reference; the constants
//! keep its names) plus `run.py`'s `speaker_reference`.
//!
//! The encoder is Resemblyzer's VoiceEncoder exported to ONNX by
//! `export_speaker_encoder.py` next to the reference; that file's docstring is
//! the mel and partial-slicing contract [`MelSpectrogram`] and
//! [`SpeakerEncoder`] implement (raw power mel, 40 bands, 400/160 frames,
//! 160-frame partials on a 77-frame step, mean of partials, L2 norm).
//!
//! ONNX Runtime is not linked: `ort` (`load-dynamic`) loads onnxruntime.dll
//! at runtime. Call [`init_runtime`] with the DLL's path before the first
//! [`SpeakerEncoder::load`]; without it `ort` reads `ORT_DYLIB_PATH` and then
//! tries a bare `onnxruntime.dll` on the DLL search path. The graph runs on
//! the CPU execution provider (a 5 MB LSTM; a GPU would only add copies).

#![cfg_attr(not(test), allow(dead_code))]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use ort::session::Session;
use ort::value::Tensor;
use realfft::{RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};
use webrtc_vad::{SampleRate, Vad, VadMode};

pub(crate) const RATE: usize = 16_000;
const FRAME_MS: i64 = 20;
const FRAME_SAMPLES: usize = RATE * FRAME_MS as usize / 1000;
/// turns.py's `--vad 3` (webrtcvad mode 3); the crate names mode 2 `Aggressive`.
const VAD_MODE: VadMode = VadMode::VeryAggressive;
/// Unvoiced gaps up to this long stay inside a run (pauses between words).
const GAP_FILL_MS: i64 = 300;
/// Runs shorter than this are not turns (VAD chatter, a breath).
const MIN_TURN_MS: i64 = 400;
/// A run longer than this is split at its quietest inner pause.
pub(crate) const MAX_TURN_MS: i64 = 12_000;
/// The pause search window inside a long run (`hop = RATE // 10` in the reference).
const QUIET_WINDOW_SAMPLES: usize = RATE / 10;
/// Silence longer than this separates scenes: speaker ids do not carry across.
const SCENE_GAP_MS: i64 = 20_000;
/// Two runs closer than this in embedding space are the same speaker.
const SAME_SPEAKER_COSINE: f32 = 0.75;
/// Within-run speaker change: sub-window embeddings on this hop; a run is cut
/// where the two sides differ by more than the same-speaker line.
const CHANGE_WINDOW_MS: i64 = 1000;
const CHANGE_HOP_MS: i64 = 250;
const MIN_PIECE_MS: i64 = 600;
const NORM_EPS: f32 = 1e-9;

/// Mel contract (see the module docs): 25 ms / 10 ms frames, 40 Slaney bands
/// over 0..8000 Hz, periodic Hann, zero centre padding, power spectrum.
const MEL_N_FFT: usize = 400;
const MEL_HOP: usize = 160;
const MEL_N_CHANNELS: usize = 40;
const MEL_FMIN_HZ: f64 = 0.0;
const MEL_FMAX_HZ: f64 = 8000.0;
/// What one mel front end differs in between its two users: GE2E here
/// ([`GE2E_MEL`], Resemblyzer's `wav_to_mel_spectrogram`) and the instrument
/// encoder (`instrument::INSTRUMENT_MEL`, librosa's `melspectrogram`). What
/// they share is one implementation ([`MelSpectrogram`]): 16 kHz input, a
/// periodic Hann of `n_fft`, centred frames with `n_fft / 2` of ZERO padding
/// (librosa 1.0's `pad_mode` default is `constant`; measured 2026-09-19 on
/// the venv's librosa, reflect padding moves the first two frames by 1.4 in
/// log-mel), the power spectrum, Slaney bands with Slaney normalisation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MelParams {
    pub n_fft: usize,
    pub hop: usize,
    pub n_channels: usize,
    pub fmin_hz: f64,
    pub fmax_hz: f64,
}

impl MelParams {
    /// Bins of the one-sided power spectrum.
    const fn bins(&self) -> usize {
        self.n_fft / 2 + 1
    }
}

pub(crate) const GE2E_MEL: MelParams = MelParams { n_fft: MEL_N_FFT, hop: MEL_HOP, n_channels: MEL_N_CHANNELS, fmin_hz: MEL_FMIN_HZ, fmax_hz: MEL_FMAX_HZ };
/// Slaney mel scale: linear below 1 kHz, logarithmic above.
const SLANEY_F_SP: f64 = 200.0 / 3.0;
const SLANEY_MIN_LOG_HZ: f64 = 1000.0;
/// Partials contract: 160 frames (1.6 s) each, 1.3 per second, the last one
/// kept only if 75% of it is real audio (unless it is the only one).
const PARTIALS_N_FRAMES: usize = 160;
const PARTIAL_RATE: f64 = 1.3;
const MIN_COVERAGE: f64 = 0.75;
const PARTIAL_SAMPLES: usize = PARTIALS_N_FRAMES * MEL_HOP;
pub(crate) const EMBEDDING_SIZE: usize = 256;
const INPUT_NAME: &str = "mels";
const OUTPUT_NAME: &str = "embedding";

/// One speaker turn, as `turns.jsonl` spells it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Turn {
    pub start_ms: i64,
    pub end_ms: i64,
    pub speaker: String,
}

/// Point `ort` at the onnxruntime.dll to load. Must run before the first
/// session; a second call is a no-op (the library is process-global).
pub(crate) fn init_runtime(dll: &Path) -> Result<(), String> {
    let builder = ort::init_from(dll).map_err(|e| format!("turns: {e}"))?;
    builder.commit();
    Ok(())
}

fn ms_to_sample(ms: i64) -> usize {
    (ms * RATE as i64 / 1000) as usize
}

/// The voiced flag of every 20 ms frame of `samples`: the ONE VAD pass over
/// a stem, which the turns and the lines' phrases both read. The VAD adapts
/// as it goes (a fresh instance on a short slice flags nothing for a while:
/// the live test of 2026-09-18 found no phrases in lines under 2.5 s), so
/// the flags are computed once over the whole stem and sliced.
pub(crate) fn voiced_flags(samples: &[i16]) -> Result<Vec<bool>, String> {
    let mut vad = Vad::new_with_rate_and_mode(SampleRate::Rate16kHz, VAD_MODE);
    samples
        .chunks_exact(FRAME_SAMPLES)
        .map(|frame| vad.is_voice_segment(frame).map_err(|_| "turns: VAD rejected a frame".to_string()))
        .collect()
}

/// Speech runs on the 20 ms VAD grid, gaps up to `GAP_FILL_MS` closed, runs
/// under `MIN_TURN_MS` dropped. `(start_ms, end_ms)` pairs.
pub(crate) fn vad_runs(samples: &[i16]) -> Result<Vec<(i64, i64)>, String> {
    Ok(runs_from_flags(&voiced_flags(samples)?))
}

/// The spoken PHRASES of the line `[start_ms, end_ms)` of the stem whose
/// flags are `voiced` ([`voiced_flags`]), for the delivery handle
/// (`dubhandle`): the same VAD grid, but a gap of [`dubhandle::PAUSE_MIN_S`]
/// or more breaks a run (the corpus's pause floor, 0.15 s = 7 frames and a
/// half; a gap of 8 frames breaks), and no run is too short to count.
/// `(start_ms, end_ms)` relative to the line's start.
pub(crate) fn phrases(voiced: &[bool], start_ms: i64, end_ms: i64) -> Vec<(i64, i64)> {
    let first = (start_ms.max(0) / FRAME_MS) as usize;
    let last = ((end_ms.max(0) / FRAME_MS) as usize).min(voiced.len());
    let fill = ((crate::dubhandle::PAUSE_MIN_S * 1000.0) as i64 - 1) / FRAME_MS;
    runs_with(&voiced[first.min(last)..last], fill, 0, true)
}

/// The pure half of [`vad_runs`]: flags on the `FRAME_MS` grid to runs.
fn runs_from_flags(voiced: &[bool]) -> Vec<(i64, i64)> {
    runs_with(voiced, GAP_FILL_MS / FRAME_MS, MIN_TURN_MS, false)
}

/// Flags to runs: unvoiced gaps up to `fill` frames stay inside a run, runs
/// shorter than `min_ms` are dropped. A run still open at the end of the
/// flags is dropped as the reference does (`close_open` false: a stem's
/// trailing run is unfinished audio at the buffer's edge, segmented again
/// when more arrives) or ends at its last voiced frame (`close_open` true:
/// a line's last phrase ends where the line does; without it every phrase
/// within `fill` frames of the line's end was lost, 2026-09-18).
fn runs_with(voiced: &[bool], fill: i64, min_ms: i64, close_open: bool) -> Vec<(i64, i64)> {
    let mut runs = Vec::new();
    let (mut start, mut gap) = (None, 0i64);
    for (i, v) in voiced.iter().copied().enumerate() {
        let i = i as i64;
        if v {
            if start.is_none() {
                start = Some(i);
            }
            gap = 0;
        } else if let Some(s) = start {
            gap += 1;
            if gap > fill {
                runs.push((s * FRAME_MS, (i - gap + 1) * FRAME_MS));
                start = None;
                gap = 0;
            }
        }
    }
    if let (Some(s), true) = (start, close_open) {
        runs.push((s * FRAME_MS, (voiced.len() as i64 - gap) * FRAME_MS));
    }
    runs.retain(|&(s, e)| e - s >= min_ms);
    runs
}

/// Runs longer than `MAX_TURN_MS` are cut at the quietest 100 ms inside their
/// middle half, repeatedly, until every piece fits.
fn split_long(runs: &[(i64, i64)], samples: &[i16]) -> Result<Vec<(i64, i64)>, String> {
    let mut out = Vec::with_capacity(runs.len());
    for &(mut s, e) in runs {
        while e - s > MAX_TURN_MS {
            let seg = &samples[ms_to_sample(s)..ms_to_sample(e)];
            let (lo, hi) = (seg.len() / 4, 3 * seg.len() / 4);
            let mut quietest: Option<(f32, usize)> = None;
            for i in (lo..hi.saturating_sub(QUIET_WINDOW_SAMPLES)).step_by(QUIET_WINDOW_SAMPLES) {
                let energy = seg[i..i + QUIET_WINDOW_SAMPLES].iter().map(|&x| (x as f32).abs()).sum::<f32>()
                    / QUIET_WINDOW_SAMPLES as f32;
                if quietest.is_none_or(|(best, _)| energy < best) {
                    quietest = Some((energy, i));
                }
            }
            let (_, at) = quietest.ok_or_else(|| format!("turns: no pause window in a {} ms run", e - s))?;
            let cut = s + (at * 1000 / RATE) as i64;
            out.push((s, cut));
            s = cut;
        }
        out.push((s, e));
    }
    Ok(out)
}

/// Fast exchanges leave no VAD gap; cut a run where the voice changes: the
/// hop position whose left/right mean sub-window embeddings agree least, if
/// they agree less than one speaker would, with both pieces >= `MIN_PIECE_MS`.
fn split_by_speaker_change(
    runs: &[(i64, i64)],
    audio: &[f32],
    encoder: &mut SpeakerEncoder,
) -> Result<Vec<(i64, i64)>, String> {
    let mut out = Vec::with_capacity(runs.len());
    let mut todo: VecDeque<(i64, i64)> = runs.iter().copied().collect();
    while let Some((s, e)) = todo.pop_front() {
        if e - s < 2 * MIN_PIECE_MS + CHANGE_WINDOW_MS {
            out.push((s, e));
            continue;
        }
        let starts: Vec<i64> = (s..=e - CHANGE_WINDOW_MS).step_by(CHANGE_HOP_MS as usize).collect();
        let mut embeds = Vec::with_capacity(starts.len());
        for &t in &starts {
            let mut embed = encoder.embed_utterance(&audio[ms_to_sample(t)..ms_to_sample(t + CHANGE_WINDOW_MS)])?;
            normalize(&mut embed);
            embeds.push(embed);
        }
        let (mut best_cut, mut best_sim) = (None, SAME_SPEAKER_COSINE);
        for k in 1..starts.len() {
            let cut = starts[k];
            if cut - s < MIN_PIECE_MS || e - cut < MIN_PIECE_MS {
                continue;
            }
            let sim = cosine(&mean(&embeds[..k]), &mean(&embeds[k..]));
            if sim < best_sim {
                best_cut = Some(cut);
                best_sim = sim;
            }
        }
        match best_cut {
            None => out.push((s, e)),
            Some(cut) => {
                todo.push_front((cut, e));
                todo.push_front((s, cut));
            }
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// Greedy agglomerative clustering by cosine to cluster centroids (the
/// centroid is the unnormalised sum of its members).
fn cluster(embeds: &[Vec<f32>]) -> Vec<usize> {
    let mut labels = Vec::with_capacity(embeds.len());
    let mut centroids: Vec<Vec<f32>> = Vec::new();
    for e in embeds {
        let mut e = e.clone();
        normalize(&mut e);
        let (mut best, mut best_sim) = (None, SAME_SPEAKER_COSINE);
        for (k, c) in centroids.iter().enumerate() {
            let sim = dot(&e, c) / (norm(c) + NORM_EPS);
            if sim > best_sim {
                best = Some(k);
                best_sim = sim;
            }
        }
        match best {
            None => {
                centroids.push(e);
                labels.push(centroids.len() - 1);
            }
            Some(k) => {
                for (c, x) in centroids[k].iter_mut().zip(&e) {
                    *c += x;
                }
                labels.push(k);
            }
        }
    }
    labels
}

/// The whole pipeline: `samples` is the 16 kHz mono dialogue stem, `voiced`
/// its flags ([`voiced_flags`]). Speaker ids are `s<scene>-<cluster>`,
/// stable within a scene only.
pub(crate) fn segment(samples: &[i16], voiced: &[bool], encoder: &mut SpeakerEncoder) -> Result<Vec<Turn>, String> {
    let audio: Vec<f32> = samples.iter().map(|&x| x as f32 / 32768.0).collect();
    let runs = split_by_speaker_change(&split_long(&runs_from_flags(voiced), samples)?, &audio, encoder)?;
    let mut embeds = Vec::with_capacity(runs.len());
    for &(s, e) in &runs {
        embeds.push(encoder.embed_utterance(&audio[ms_to_sample(s)..ms_to_sample(e)])?);
    }
    let mut turns = Vec::with_capacity(runs.len());
    let (mut scene_start, mut scene) = (0, 0);
    for i in 0..=runs.len() {
        let boundary = i == runs.len() || (i > 0 && runs[i].0 - runs[i - 1].1 > SCENE_GAP_MS);
        if boundary {
            let labels = cluster(&embeds[scene_start..i]);
            for (&(start_ms, end_ms), label) in runs[scene_start..i].iter().zip(labels) {
                turns.push(Turn { start_ms, end_ms, speaker: format!("s{scene}-{label}") });
            }
            scene_start = i;
            scene += 1;
        }
    }
    Ok(turns)
}

/// The cue's speaker's own turns (the speaker whose turn overlaps the cue
/// most), nearest the cue first up to `max_s` seconds, concatenated in
/// chronological order. `None` when no turn overlaps the cue. `stem` must be
/// the audio the turns were segmented from.
pub(crate) fn speaker_reference(turns: &[Turn], stem: &[f32], start_ms: i64, end_ms: i64, max_s: f64) -> Option<Vec<f32>> {
    let overlap = |t: &Turn| t.end_ms.min(end_ms) - t.start_ms.max(start_ms);
    let mut speaker: Option<&Turn> = None;
    for t in turns.iter().filter(|t| t.start_ms < end_ms && t.end_ms > start_ms) {
        if speaker.is_none_or(|best| overlap(t) > overlap(best)) {
            speaker = Some(t);
        }
    }
    let speaker = &speaker?.speaker;
    let mid = (start_ms + end_ms) as f64 / 2.0;
    let mut own: Vec<&Turn> = turns.iter().filter(|t| &t.speaker == speaker).collect();
    own.sort_by(|a, b| {
        let d = |t: &Turn| ((t.start_ms + t.end_ms) as f64 / 2.0 - mid).abs();
        d(a).partial_cmp(&d(b)).expect("turn distances are finite")
    });
    let mut pieces: Vec<&Turn> = Vec::new();
    let mut total_s = 0.0;
    for t in own {
        let len_s = (t.end_ms - t.start_ms) as f64 / 1000.0;
        if total_s + len_s > max_s && !pieces.is_empty() {
            break;
        }
        pieces.push(t);
        total_s += len_s;
    }
    pieces.sort_by_key(|t| t.start_ms);
    let mut out = Vec::with_capacity(pieces.iter().map(|t| ms_to_sample(t.end_ms - t.start_ms)).sum());
    for t in pieces {
        out.extend_from_slice(&stem[ms_to_sample(t.start_ms)..ms_to_sample(t.end_ms)]);
    }
    Some(out)
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn norm(a: &[f32]) -> f32 {
    dot(a, a).sqrt()
}

fn normalize(a: &mut [f32]) {
    let n = norm(a) + NORM_EPS;
    a.iter_mut().for_each(|x| *x /= n);
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    dot(a, b) / (norm(a) * norm(b) + NORM_EPS)
}

fn mean(rows: &[Vec<f32>]) -> Vec<f32> {
    let mut out = vec![0.0f32; rows[0].len()];
    for r in rows {
        for (o, x) in out.iter_mut().zip(r) {
            *o += x;
        }
    }
    out.iter_mut().for_each(|x| *x /= rows.len() as f32);
    out
}

/// Where the partials of an `n_samples` utterance start, in mel frames
/// (`VoiceEncoder.compute_partial_slices`; each spans `PARTIALS_N_FRAMES`).
fn partial_starts(n_samples: usize) -> Vec<usize> {
    let n_frames = (n_samples + 1).div_ceil(MEL_HOP) as i64;
    let frame_step = (RATE as f64 / PARTIAL_RATE / MEL_HOP as f64).round() as usize;
    let steps = (n_frames - PARTIALS_N_FRAMES as i64 + frame_step as i64 + 1).max(1) as usize;
    let mut starts: Vec<usize> = (0..steps).step_by(frame_step).collect();
    let last_start = starts[starts.len() - 1] * MEL_HOP;
    let coverage = (n_samples as f64 - last_start as f64) / PARTIAL_SAMPLES as f64;
    if coverage < MIN_COVERAGE && starts.len() > 1 {
        starts.pop();
    }
    starts
}

/// The raw power mel spectrogram of a [`MelParams`] contract.
pub(crate) struct MelSpectrogram {
    params: MelParams,
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    /// `n_channels` x `bins`, row-major.
    filters: Vec<f32>,
}

impl MelSpectrogram {
    pub(crate) fn new(params: MelParams) -> Self {
        let window = (0..params.n_fft)
            .map(|k| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / params.n_fft as f64).cos()) as f32)
            .collect();
        Self { params, fft: RealFftPlanner::<f32>::new().plan_fft_forward(params.n_fft), window, filters: mel_filters(&params) }
    }

    pub(crate) fn params(&self) -> &MelParams {
        &self.params
    }

    /// Frames x `n_channels`, row-major; `1 + wav.len() / hop` frames.
    pub(crate) fn compute(&self, wav: &[f32]) -> Vec<f32> {
        let MelParams { n_fft, hop, n_channels, .. } = self.params;
        let bins = self.params.bins();
        let n_frames = 1 + wav.len() / hop;
        let half = n_fft / 2;
        let mut frame = self.fft.make_input_vec();
        let mut spectrum = self.fft.make_output_vec();
        let mut scratch = self.fft.make_scratch_vec();
        let mut power = vec![0.0f32; bins];
        let mut out = vec![0.0f32; n_frames * n_channels];
        for t in 0..n_frames {
            let origin = t * hop;
            for (k, x) in frame.iter_mut().enumerate() {
                let s = (origin + k) as i64 - half as i64;
                let sample = if s < 0 || s >= wav.len() as i64 { 0.0 } else { wav[s as usize] };
                *x = sample * self.window[k];
            }
            self.fft.process_with_scratch(&mut frame, &mut spectrum, &mut scratch).expect("planned sizes");
            for (p, c) in power.iter_mut().zip(&spectrum) {
                *p = c.norm_sqr();
            }
            let row = &mut out[t * n_channels..(t + 1) * n_channels];
            for (m, o) in row.iter_mut().enumerate() {
                *o = dot(&self.filters[m * bins..(m + 1) * bins], &power);
            }
        }
        out
    }
}

fn hz_to_mel(hz: f64) -> f64 {
    let min_log_mel = SLANEY_MIN_LOG_HZ / SLANEY_F_SP;
    let logstep = 6.4f64.ln() / 27.0;
    if hz >= SLANEY_MIN_LOG_HZ {
        min_log_mel + (hz / SLANEY_MIN_LOG_HZ).ln() / logstep
    } else {
        hz / SLANEY_F_SP
    }
}

fn mel_to_hz(mel: f64) -> f64 {
    let min_log_mel = SLANEY_MIN_LOG_HZ / SLANEY_F_SP;
    let logstep = 6.4f64.ln() / 27.0;
    if mel >= min_log_mel {
        SLANEY_MIN_LOG_HZ * (logstep * (mel - min_log_mel)).exp()
    } else {
        SLANEY_F_SP * mel
    }
}

/// `librosa.filters.mel(sr=16000, n_fft, n_mels, fmin, fmax, htk=False, norm="slaney")`,
/// including its float32 rounding order (triangles cast to f32, then scaled).
fn mel_filters(params: &MelParams) -> Vec<f32> {
    let MelParams { n_channels, fmin_hz, fmax_hz, .. } = *params;
    let bins = params.bins();
    let n_points = n_channels + 2;
    let (m_lo, m_hi) = (hz_to_mel(fmin_hz), hz_to_mel(fmax_hz));
    let step = (m_hi - m_lo) / (n_points - 1) as f64;
    let mut mel_f: Vec<f64> = (0..n_points).map(|i| m_lo + i as f64 * step).map(mel_to_hz).collect();
    mel_f[n_points - 1] = mel_to_hz(m_hi);
    let fft_f: Vec<f64> = (0..bins).map(|k| k as f64 * (RATE as f64 / 2.0) / (bins - 1) as f64).collect();
    let mut filters = vec![0.0f32; n_channels * bins];
    for m in 0..n_channels {
        let enorm = 2.0 / (mel_f[m + 2] - mel_f[m]);
        for (k, &f) in fft_f.iter().enumerate() {
            let lower = (f - mel_f[m]) / (mel_f[m + 1] - mel_f[m]);
            let upper = (mel_f[m + 2] - f) / (mel_f[m + 2] - mel_f[m + 1]);
            let triangle = lower.min(upper).max(0.0) as f32;
            filters[m * bins + k] = (triangle as f64 * enorm) as f32;
        }
    }
    filters
}

/// The GE2E encoder: an ONNX session plus the mel front end.
pub(crate) struct SpeakerEncoder {
    session: Session,
    mel: MelSpectrogram,
}

impl SpeakerEncoder {
    pub(crate) fn load(model: &Path) -> Result<Self, String> {
        let session = Session::builder()
            .and_then(|mut b| b.commit_from_file(model))
            .map_err(|e| format!("turns: cannot load {}: {e}", model.display()))?;
        Ok(Self { session, mel: MelSpectrogram::new(GE2E_MEL) })
    }

    /// `VoiceEncoder.embed_utterance`: the L2-normalised mean of the partial
    /// embeddings of a 16 kHz float32 utterance.
    pub(crate) fn embed_utterance(&mut self, wav: &[f32]) -> Result<Vec<f32>, String> {
        let starts = partial_starts(wav.len());
        let max_wave_length = (starts[starts.len() - 1] + PARTIALS_N_FRAMES) * MEL_HOP;
        let padded: Vec<f32>;
        let wav = if max_wave_length >= wav.len() {
            padded = wav.iter().copied().chain(std::iter::repeat(0.0)).take(max_wave_length).collect();
            &padded
        } else {
            wav
        };
        let mel = self.mel.compute(wav);
        let n_frames = mel.len() / MEL_N_CHANNELS;
        let mut batch = Vec::with_capacity(starts.len() * PARTIALS_N_FRAMES * MEL_N_CHANNELS);
        for &s in &starts {
            if s + PARTIALS_N_FRAMES > n_frames {
                return Err(format!("turns: partial at frame {s} overruns {n_frames} mel frames"));
            }
            batch.extend_from_slice(&mel[s * MEL_N_CHANNELS..(s + PARTIALS_N_FRAMES) * MEL_N_CHANNELS]);
        }
        let shape = vec![starts.len() as i64, PARTIALS_N_FRAMES as i64, MEL_N_CHANNELS as i64];
        let input = Tensor::from_array((shape, batch)).map_err(|e| format!("turns: {e}"))?;
        let outputs = self.session.run(ort::inputs![INPUT_NAME => input]).map_err(|e| format!("turns: {e}"))?;
        let (out_shape, partials) =
            outputs[OUTPUT_NAME].try_extract_tensor::<f32>().map_err(|e| format!("turns: {e}"))?;
        if partials.len() != starts.len() * EMBEDDING_SIZE {
            return Err(format!("turns: encoder returned shape {out_shape:?} for {} partials", starts.len()));
        }
        let rows: Vec<Vec<f32>> = partials.chunks_exact(EMBEDDING_SIZE).map(<[f32]>::to_vec).collect();
        let mut embed = mean(&rows);
        let n = norm(&embed);
        embed.iter_mut().for_each(|x| *x /= n);
        Ok(embed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Once;
    use std::time::Instant;

    const STEM: &str = r"E:\datasets\g5\stem16k.wav";
    const TURNS: &str = r"E:\datasets\g5\turns.jsonl";
    const PARITY_DIR: &str = r"E:\datasets\g5\parity";
    const MODEL: &str = r"E:\models\speaker\ge2e_resemblyzer.onnx";
    /// The 1.24 DirectML build the separator port loads; `ORT_DYLIB_PATH` overrides.
    const ORT_DLL: &str = r"E:\tools\ort-dml\onnxruntime.dll";
    const MEL_REL_TOL: f32 = 1e-3;
    const MIN_EMBED_COSINE: f32 = 0.999;
    const BOUNDARY_TOL_MS: i64 = 40;
    const MIN_BOUNDARY_AGREEMENT: f64 = 0.95;
    const MAX_SPEAKER_COUNT_DRIFT: f64 = 0.20;
    const VAD_HANGOVER_MS: i64 = 200;

    fn read_f32(path: PathBuf) -> Vec<f32> {
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    }

    fn encoder() -> SpeakerEncoder {
        static RUNTIME: Once = Once::new();
        RUNTIME.call_once(|| {
            let dll = std::env::var("ORT_DYLIB_PATH").unwrap_or_else(|_| ORT_DLL.to_string());
            init_runtime(Path::new(&dll)).expect("onnxruntime.dll loads");
        });
        SpeakerEncoder::load(Path::new(MODEL)).expect("speaker model loads")
    }

    fn stem() -> Vec<i16> {
        let bytes = std::fs::read(STEM).expect("stem wav");
        crate::autosync::parse_wav_s16(&bytes, crate::autosync::PcmFormat { rate: RATE as u32, channels: 1 }).expect("16 kHz mono s16 stem")
    }

    /// A voiced, speech-like signal: a glottal pulse train (harmonics falling
    /// as 1/n) with a slow pitch glide, which the VAD flags as speech.
    fn voice(len_ms: usize) -> Vec<i16> {
        let n = len_ms * RATE / 1000;
        let mut phase = 0.0f64;
        (0..n)
            .map(|i| {
                let f0 = 110.0 + 30.0 * (i as f64 / n as f64);
                phase += 2.0 * std::f64::consts::PI * f0 / RATE as f64;
                let s: f64 = (1..=20).map(|h| (h as f64 * phase).sin() / h as f64).sum();
                (s * 6000.0) as i16
            })
            .collect()
    }

    #[test]
    fn runs_close_short_gaps_and_drop_short_runs() {
        // Frames of 20 ms: 50 silent, 40 voiced, 10 silent (200 ms, filled),
        // 30 voiced, 50 silent, 15 voiced (300 ms, too short), 20 silent,
        // 20 voiced, 16 silent (320 ms, a gap), 25 voiced. The last run ends
        // within GAP_FILL_MS of the end and is dropped: the reference never
        // flushes an open run (turns.py appends one silent frame only).
        let mut flags = Vec::new();
        for (v, n) in [(false, 50), (true, 40), (false, 10), (true, 30), (false, 50), (true, 15), (false, 20), (true, 20), (false, 16), (true, 25)] {
            flags.extend(std::iter::repeat(v).take(n));
        }
        assert_eq!(runs_from_flags(&flags), vec![(1000, 2600), (4300, 4700)]);
        flags.extend(std::iter::repeat(false).take(16));
        assert_eq!(runs_from_flags(&flags), vec![(1000, 2600), (4300, 4700), (5020, 5520)]);
    }

    #[test]
    fn phrases_slice_the_stem_flags_and_break_at_the_pause_floor() {
        // 20 ms frames: 10 voiced, 7 off (filled, 140 ms), 5 voiced, 8 off (a pause, 160 ms), 3 voiced
        let mut voiced = vec![false; 116];
        for i in (50..60).chain(67..72).chain(80..83) {
            voiced[i] = true;
        }
        // the line starts at frame 45 (900 ms): runs come back relative to it
        assert_eq!(phrases(&voiced, 900, 1_700), vec![(100, 540), (700, 760)]);
        // a line past the flags is empty, never a panic
        assert_eq!(phrases(&voiced, 5_000, 6_000), Vec::<(i64, i64)>::new());
    }

    #[test]
    fn vad_runs_on_a_synthetic_voice() {
        let silence = |ms: usize| vec![0i16; ms * RATE / 1000];
        let mut samples = silence(1000);
        samples.extend(voice(800));
        samples.extend(silence(200));
        samples.extend(voice(600));
        samples.extend(silence(1000));
        samples.extend(voice(160));
        samples.extend(silence(1000));
        let runs = vad_runs(&samples).unwrap();
        assert_eq!(runs.len(), 1, "one filled run expected, got {runs:?}");
        // The VAD hangs on for a few frames after a burst ends.
        let (s, e) = runs[0];
        assert!((s - 1000).abs() <= 2 * FRAME_MS && (2600..=2600 + VAD_HANGOVER_MS).contains(&e), "run {runs:?}");
    }

    #[test]
    fn partial_starts_follow_compute_partial_slices() {
        assert_eq!(partial_starts(48_000), vec![0, 77, 154]);
        // 0.5 s: a single padded partial, kept whatever its coverage.
        assert_eq!(partial_starts(8_000), vec![0]);
        // 2.0 s: starts 0 and 77; the second covers (32000 - 12320) / 25600 = 0.77.
        assert_eq!(partial_starts(32_000), vec![0, 77]);
        // 1.9 s: the second would cover 0.71 and is dropped.
        assert_eq!(partial_starts(30_400), vec![0]);
    }

    #[test]
    fn mel_filters_match_librosa_slaney() {
        let f = mel_filters(&GE2E_MEL);
        let at = |m: usize, k: usize| f[m * GE2E_MEL.bins() + k];
        for (m, k, want) in [(0, 1, 0.007390209), (0, 2, 0.012404521), (1, 3, 0.008578158), (39, 190, 0.001215154), (39, 199, 0.000121515)] {
            assert!((at(m, k) - want).abs() < 1e-8, "filter[{m}][{k}] = {} want {want}", at(m, k));
        }
        assert_eq!(at(0, 0), 0.0);
        assert_eq!(at(39, 200), 0.0);
    }

    #[test]
    fn mel_frame_count_and_silence() {
        let mel = MelSpectrogram::new(GE2E_MEL).compute(&vec![0.0f32; 16_000]);
        assert_eq!(mel.len(), 101 * MEL_N_CHANNELS);
        assert!(mel.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn reference_picks_the_overlapping_speaker_nearest_turns_chronologically() {
        let t = |s, e, sp: &str| Turn { start_ms: s, end_ms: e, speaker: sp.into() };
        let turns = vec![t(0, 1000, "a"), t(1000, 2000, "b"), t(2000, 3000, "a"), t(5000, 9000, "a"), t(9000, 9500, "a")];
        let stem: Vec<f32> = (0..ms_to_sample(10_000)).map(|i| i as f32).collect();
        // The cue overlaps b (200 ms) and a (700 ms): a wins; nearest a-turns
        // are [2000,3000) then [0,1000), then [5000,9000) (would exceed 3 s).
        let r = speaker_reference(&turns, &stem, 1800, 2700, 3.0).unwrap();
        assert_eq!(r.len(), ms_to_sample(2000));
        assert_eq!(r[0], 0.0);
        assert_eq!(r[ms_to_sample(1000)], ms_to_sample(2000) as f32);
        // A single turn longer than the budget is still returned whole.
        assert_eq!(speaker_reference(&[t(0, 9000, "a")], &stem, 100, 200, 1.0).unwrap().len(), ms_to_sample(9000));
        assert!(speaker_reference(&turns, &stem, 3500, 4500, 8.0).is_none());
    }

    #[test]
    #[ignore]
    fn mel_parity_with_librosa() {
        let dir = PathBuf::from(PARITY_DIR);
        let wav = read_f32(dir.join("wav.f32"));
        let want = read_f32(dir.join("mel.f32"));
        let got = MelSpectrogram::new(GE2E_MEL).compute(&wav);
        assert_eq!(got.len(), want.len(), "frame count");
        let peak = want.iter().cloned().fold(0.0f32, f32::max);
        let (mut worst_rel, mut worst_rel_at) = (0.0f32, 0usize);
        let mut worst_scaled = 0.0f32;
        for (i, (&g, &w)) in got.iter().zip(&want).enumerate() {
            let rel = (g - w).abs() / w.abs().max(f32::MIN_POSITIVE);
            if w.abs() >= peak * 1e-6 && rel > worst_rel {
                worst_rel = rel;
                worst_rel_at = i;
            }
            worst_scaled = worst_scaled.max((g - w).abs() / (w.abs() + peak * 1e-6));
        }
        println!(
            "mel parity: {} frames, peak {peak:.4e}, worst relative error {worst_rel:.3e} at [{}][{}] (bins >= 1e-6 peak), worst with a 1e-6 peak floor {worst_scaled:.3e}",
            got.len() / MEL_N_CHANNELS,
            worst_rel_at / MEL_N_CHANNELS,
            worst_rel_at % MEL_N_CHANNELS
        );
        assert!(worst_rel <= MEL_REL_TOL, "mel drifts from librosa: {worst_rel:.3e}");
    }

    #[test]
    #[ignore]
    fn embedding_parity_with_resemblyzer() {
        let dir = PathBuf::from(PARITY_DIR);
        let wav = read_f32(dir.join("wav.f32"));
        let want = read_f32(dir.join("embedding.f32"));
        let got = encoder().embed_utterance(&wav).unwrap();
        assert_eq!(got.len(), EMBEDDING_SIZE);
        let c = cosine(&got, &want);
        println!("embedding parity: cosine {c:.6} (norm {:.6})", norm(&got));
        assert!(c >= MIN_EMBED_COSINE, "embedding drifts from Resemblyzer: cosine {c}");
    }

    #[test]
    #[ignore]
    fn segments_like_turns_py() {
        let samples = stem();
        let want: Vec<Turn> = std::fs::read_to_string(TURNS)
            .expect("turns.jsonl")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("turn row"))
            .collect();
        let mut enc = encoder();
        let t0 = Instant::now();
        let got = segment(&samples, &voiced_flags(&samples).unwrap(), &mut enc).unwrap();
        let wall = t0.elapsed();
        let matched = want
            .iter()
            .filter(|w| {
                got.iter().any(|g| (g.start_ms - w.start_ms).abs() <= BOUNDARY_TOL_MS && (g.end_ms - w.end_ms).abs() <= BOUNDARY_TOL_MS)
            })
            .count();
        let agreement = matched as f64 / want.len() as f64;
        let ids = |turns: &[Turn]| turns.iter().map(|t| t.speaker.as_str()).collect::<std::collections::BTreeSet<_>>().len();
        let (want_ids, got_ids) = (ids(&want), ids(&got));
        let scenes = got.iter().filter_map(|t| t.speaker.split('-').next()).collect::<std::collections::BTreeSet<_>>().len();
        println!(
            "segmentation: {} turns ({} in turns.jsonl), {matched} within {BOUNDARY_TOL_MS} ms = {:.1}%, {got_ids} speaker ids ({want_ids} in turns.jsonl) over {scenes} scenes, {:.1} s of audio in {:.2} s wall",
            got.len(),
            want.len(),
            agreement * 100.0,
            samples.len() as f64 / RATE as f64,
            wall.as_secs_f64()
        );
        for w in want.iter().filter(|w| {
            !got.iter().any(|g| (g.start_ms - w.start_ms).abs() <= BOUNDARY_TOL_MS && (g.end_ms - w.end_ms).abs() <= BOUNDARY_TOL_MS)
        }) {
            let near: Vec<_> = got.iter().filter(|g| g.start_ms < w.end_ms + 500 && g.end_ms > w.start_ms - 500).map(|g| (g.start_ms, g.end_ms)).collect();
            println!("  unmatched {}..{} ({}) near {near:?}", w.start_ms, w.end_ms, w.speaker);
        }
        assert!(agreement >= MIN_BOUNDARY_AGREEMENT, "boundary agreement {agreement:.3}");
        let drift = (got_ids as f64 - want_ids as f64).abs() / want_ids as f64;
        assert!(drift <= MAX_SPEAKER_COUNT_DRIFT, "speaker id count drift {drift:.3}");
    }
}
