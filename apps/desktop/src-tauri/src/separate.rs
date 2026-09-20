//! Dialogue separation: the BS-RoFormer ep368 vocals model, as the ONNX core
//! exported in `docs/dubbing/stage1-pipeline/s2`, on ONNX Runtime's DirectML
//! execution provider (decision D1c). Everything around the graph is a Rust
//! port of the Python reference (`parity.py` there, and audio-separator's
//! `MDXCSeparator`), in this order:
//!
//!   1. resample the caller's stereo to the model's 44.1 kHz,
//!   2. peak-normalize to 0.9 (`spec_utils.normalize`); the gain is undone on
//!      the way out so the stems come back at the input's scale,
//!   3. chunk into `hop * (dim_t - 1)` = 352,800-sample windows on
//!      `MDXCSeparator._roformer_chunk_starts`'s schedule at overlap 2,
//!   4. per chunk: STFT (the `torch.stft` twin: n_fft 2048, hop 441, periodic
//!      hann, centered, reflect-padded, one-sided, unnormalized), the graph,
//!      iSTFT,
//!   5. hamming overlap-add divided by the summed window,
//!   6. resample the vocals back; residual = input - vocals at the caller's
//!      rate, so `vocals + residual == input` sample for sample.
//!
//! The graph takes `spec` float32 (batch, 2, 1025, frames, 2), the real view
//! of the per-channel STFT, and returns `masked_spec` in the same layout.
//!
//! Parity with the Python arms is held by the ignored `f5_parity_*` tests,
//! which run at the model's native rate so the resampler is not inside the
//! measurement (SI-SDR of vocals vs the true speech and of the residual vs
//! the true background, G2's scorer reimplemented below).
//!
//! The runtime is not linked: `onnxruntime.dll` and `DirectML.dll` (the
//! DirectML build of ONNX Runtime) ship as a pack and are loaded from a
//! directory at [`Separator::open`]. One runtime per process: ort loads the
//! first path it is given and ignores later ones.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ort::ep::{self, ExecutionProvider};
use ort::session::Session;
use ort::value::TensorRef;
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// `audio.sample_rate` in the model yaml: the rate the graph was trained at.
pub const MODEL_SAMPLE_RATE: u32 = 44_100;
/// `model.stft_n_fft` (== `model.stft_win_length`: the model pads no window).
const N_FFT: usize = 2048;
/// `model.stft_hop_length`.
const HOP: usize = 441;
/// One-sided bins, == `model.dim_freqs_in`.
const FREQ_BINS: usize = N_FFT / 2 + 1;
/// `torch.stft(center=True)` pads `n_fft / 2` on both sides.
const PAD: usize = N_FFT / 2;
/// `inference.dim_t`: frames per chunk.
const DIM_T: usize = 801;
/// `MDXCSeparator`: chunk = stft hop * (dim_t - 1), so a chunk is exactly
/// `DIM_T` frames and the iSTFT returns exactly the chunk length.
const CHUNK_SAMPLES: usize = HOP * (DIM_T - 1);
/// `mdxc_overlap`: G2's winning arm ran 2 (the step is chunk / overlap).
const OVERLAP: usize = 2;
const CHUNK_STEP: usize = CHUNK_SAMPLES / OVERLAP;
const CHANNELS: usize = 2;
const COMPLEX: usize = 2;
const SPEC_SHAPE: [usize; 5] = [1, CHANNELS, FREQ_BINS, DIM_T, COMPLEX];
const SPEC_ELEMENTS: usize = CHANNELS * FREQ_BINS * DIM_T * COMPLEX;
/// `Separator(normalization_threshold=0.9)`: the mix is lowered to this peak
/// before demixing, never raised.
const NORMALIZATION_PEAK: f32 = 0.9;
/// `counter.clamp_(min=1e-10)` before the overlap-add division.
const COUNTER_FLOOR: f32 = 1e-10;
/// `parity.py::Stft.inverse` refuses a window envelope this close to zero.
const ENVELOPE_FLOOR: f32 = 1e-11;
const INPUT_NAME: &str = "spec";
const OUTPUT_NAME: &str = "masked_spec";
const ORT_DLL: &str = "onnxruntime.dll";
const DIRECTML_DLL: &str = "DirectML.dll";

/// Resampler: windowed-sinc polyphase, `2 * RESAMPLE_HALF_TAPS + 1` taps per
/// output sample, Kaiser beta 9 (about 70 dB stopband). The 48 kHz <-> 44.1 kHz
/// ratio is 160:147, so the kernel is tabulated per phase once.
const RESAMPLE_HALF_TAPS: usize = 64;
const KAISER_BETA: f64 = 9.0;
/// Bessel I0 series terms are summed until they fall below this.
const BESSEL_EPSILON: f64 = 1e-12;

/// Which ONNX Runtime provider runs the graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// The production path. `open` fails if this build of the runtime has no
    /// DirectML provider or registering it fails: no silent CPU fallback.
    DirectML,
    /// The CPU provider, selected explicitly (tests; RTF ~7 on this box).
    Cpu,
}

/// Stems at the caller's sample rate and scale, both the input's length.
pub struct Separated {
    pub vocals: [Vec<f32>; CHANNELS],
    pub residual: [Vec<f32>; CHANNELS],
}

pub struct Separator {
    session: Mutex<Session>,
    stft: Stft,
    /// `DirectML.dll` pinned from the pack directory: the runtime loads it by
    /// bare name, and an already-loaded module wins that lookup, so the pack's
    /// copy is the one used whatever else is on the search path.
    _directml: Option<libloading::Library>,
}

impl Separator {
    /// Loads the runtime from `ort_dir` (`onnxruntime.dll` + `DirectML.dll`)
    /// and the model on the DirectML provider.
    pub fn open(model: &Path, ort_dir: &Path) -> Result<Self, String> {
        Self::open_with(model, ort_dir, Provider::DirectML)
    }

    pub fn open_with(model: &Path, ort_dir: &Path, provider: Provider) -> Result<Self, String> {
        let ort_dll = ort_dir.join(ORT_DLL);
        if !ort_dll.is_file() {
            return Err(format!("{} is not a file", ort_dll.display()));
        }
        if !model.is_file() {
            return Err(format!("{} is not a file", model.display()));
        }
        let directml = match provider {
            Provider::DirectML => {
                let path = ort_dir.join(DIRECTML_DLL);
                Some(unsafe { libloading::Library::new(&path) }.map_err(|e| format!("loading {}: {e}", path.display()))?)
            }
            Provider::Cpu => None,
        };
        ort::init_from(&ort_dll)
            .map_err(|e| format!("loading {}: {e}", ort_dll.display()))?
            .with_name("rillio")
            .with_telemetry(false)
            .commit();

        let mut builder = Session::builder().map_err(|e| format!("onnxruntime session options: {e}"))?;
        if provider == Provider::DirectML {
            let dml = ep::DirectML::default();
            let available = dml.is_available().map_err(|e| format!("querying onnxruntime providers: {e}"))?;
            if !available {
                return Err(format!("{} has no DirectML provider (not the DirectML build of onnxruntime)", ort_dll.display()));
            }
            builder = builder
                .with_execution_providers([dml.build().error_on_failure()])
                .map_err(|e| format!("registering the DirectML provider: {e}"))?;
        }
        // The dynamic axes stay dynamic even though every chunk is one batch of
        // DIM_T frames: pinning them with a free-dimension override makes the
        // DirectML provider return garbage for this graph (max abs diff 91 on
        // a 64 peak, fp16 and fp32 alike, measured 2026-09-14), while the CPU
        // provider is unaffected. Without the override DML matches CPU.
        let session = builder.commit_from_file(model).map_err(|e| format!("loading {}: {e}", model.display()))?;
        tracing::info!(model = %model.display(), ?provider, "separator session ready");
        Ok(Self { session: Mutex::new(session), stft: Stft::new(), _directml: directml })
    }

    /// Separates `channels` (left, right; hand the same slice twice for mono)
    /// at `sample_rate` into vocals and residual, both returned at
    /// `sample_rate` with the input's length and scale.
    pub fn separate(&self, channels: [&[f32]; CHANNELS], sample_rate: u32) -> Result<Separated, String> {
        let len = channels[0].len();
        if len == 0 {
            return Err("empty input".into());
        }
        if channels[1].len() != len {
            return Err(format!("channel lengths differ: {len} vs {}", channels[1].len()));
        }
        if sample_rate == 0 {
            return Err("sample rate is 0".into());
        }
        if channels.iter().any(|c| c.iter().any(|s| !s.is_finite())) {
            return Err("input contains non-finite samples".into());
        }

        let to_model = (sample_rate != MODEL_SAMPLE_RATE).then(|| Resampler::new(sample_rate, MODEL_SAMPLE_RATE));
        let model_len = match &to_model {
            Some(_) => ((len as u64 * MODEL_SAMPLE_RATE as u64).div_ceil(sample_rate as u64)) as usize,
            None => len,
        };
        let mut mix: [Vec<f32>; CHANNELS] = [Vec::new(), Vec::new()];
        for (ch, source) in channels.iter().enumerate() {
            mix[ch] = match &to_model {
                Some(r) => r.run(source, model_len),
                None => source.to_vec(),
            };
        }

        // spec_utils.normalize: lower to the peak if above it, in float32.
        let peak = mix.iter().flat_map(|c| c.iter()).fold(0f32, |m, s| m.max(s.abs()));
        let gain = if peak > NORMALIZATION_PEAK { NORMALIZATION_PEAK / peak } else { 1.0 };
        for c in mix.iter_mut() {
            if gain != 1.0 {
                c.iter_mut().for_each(|s| *s *= gain);
            }
            // The chunk schedule assumes at least one full chunk; a shorter
            // input is zero-padded to one and trimmed after.
            if c.len() < CHUNK_SAMPLES {
                c.resize(CHUNK_SAMPLES, 0.0);
            }
        }

        let mut vocals = self.demix(&mix)?;
        for c in vocals.iter_mut() {
            c.truncate(model_len);
            if gain != 1.0 {
                c.iter_mut().for_each(|s| *s /= gain);
            }
        }

        let from_model = to_model.as_ref().map(|_| Resampler::new(MODEL_SAMPLE_RATE, sample_rate));
        let mut out = Separated { vocals: [Vec::new(), Vec::new()], residual: [Vec::new(), Vec::new()] };
        for ch in 0..CHANNELS {
            let v = match &from_model {
                Some(r) => r.run(&vocals[ch], len),
                None => std::mem::take(&mut vocals[ch]),
            };
            out.residual[ch] = channels[ch].iter().zip(&v).map(|(x, s)| x - s).collect();
            out.vocals[ch] = v;
        }
        Ok(out)
    }

    /// `MDXCSeparator.demix`'s RoFormer path: chunk, STFT, graph, iSTFT,
    /// hamming overlap-add. `mix` is at the model rate, normalized, and at
    /// least one chunk long.
    fn demix(&self, mix: &[Vec<f32>; CHANNELS]) -> Result<[Vec<f32>; CHANNELS], String> {
        let len = mix[0].len();
        let starts = chunk_starts(len, CHUNK_SAMPLES, CHUNK_STEP);
        let window = hamming(CHUNK_SAMPLES);
        let mut result = [vec![0f32; len], vec![0f32; len]];
        let mut counter = vec![0f32; len];
        let mut spec = vec![0f32; SPEC_ELEMENTS];
        let mut stem = Vec::with_capacity(CHUNK_SAMPLES);
        let mut session = self.session.lock().map_err(|_| "separator session lock poisoned")?;
        let started = Instant::now();
        let mut graph_time = Duration::ZERO;

        for (i, &start) in starts.iter().enumerate() {
            let end = start + CHUNK_SAMPLES;
            if end > len {
                return Err(format!("chunk {i} at {start} runs past the {len}-sample mix"));
            }
            for (ch, c) in mix.iter().enumerate() {
                self.stft.forward(&c[start..end], ch, DIM_T, &mut spec)?;
            }
            let input = TensorRef::from_array_view((SPEC_SHAPE, spec.as_slice())).map_err(|e| format!("input tensor: {e}"))?;
            let run_started = Instant::now();
            let outputs = session.run(ort::inputs![INPUT_NAME => input]).map_err(|e| format!("chunk {i}: {e}"))?;
            graph_time += run_started.elapsed();
            let (shape, masked) = outputs[OUTPUT_NAME]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("chunk {i}: reading {OUTPUT_NAME}: {e}"))?;
            let dims: &[i64] = shape;
            if dims.len() != SPEC_SHAPE.len() || dims.iter().zip(SPEC_SHAPE).any(|(d, e)| *d != e as i64) {
                return Err(format!("chunk {i}: {OUTPUT_NAME} is {dims:?}, expected {SPEC_SHAPE:?}"));
            }
            for ch in 0..CHANNELS {
                self.stft.inverse(masked, ch, DIM_T, &mut stem)?;
                if stem.len() != CHUNK_SAMPLES {
                    return Err(format!("chunk {i}: iSTFT returned {} samples for {CHUNK_SAMPLES}", stem.len()));
                }
                for (k, s) in stem.iter().enumerate() {
                    result[ch][start + k] += s * window[k];
                }
            }
            for (k, w) in window.iter().enumerate() {
                counter[start + k] += w;
            }
            tracing::debug!(chunk = i + 1, of = starts.len(), elapsed_s = started.elapsed().as_secs_f32(), "separator chunk");
        }
        for c in result.iter_mut() {
            for (s, n) in c.iter_mut().zip(&counter) {
                *s /= n.max(COUNTER_FLOOR);
            }
        }
        let audio_s = len as f64 / MODEL_SAMPLE_RATE as f64;
        tracing::info!(
            chunks = starts.len(),
            rtf = started.elapsed().as_secs_f64() / audio_s,
            graph_rtf = graph_time.as_secs_f64() / audio_s,
            "separator demix done"
        );
        Ok(result)
    }
}

/// `MDXCSeparator._roformer_chunk_starts`: every `step` until the chunk
/// would reach the end, then one tail chunk ending exactly at the end, never
/// repeated.
fn chunk_starts(audio_length: usize, chunk_size: usize, step: usize) -> Vec<usize> {
    let mut starts = Vec::new();
    for offset in (0..audio_length).step_by(step) {
        if offset + chunk_size >= audio_length {
            let tail_start = audio_length.saturating_sub(chunk_size);
            if starts.last() != Some(&tail_start) {
                starts.push(tail_start);
            }
            break;
        }
        starts.push(offset);
    }
    starts
}

/// `scipy.signal.windows.hamming(n)`: symmetric.
fn hamming(n: usize) -> Vec<f32> {
    let denom = (n - 1) as f64;
    (0..n).map(|i| (0.54 - 0.46 * (2.0 * std::f64::consts::PI * i as f64 / denom).cos()) as f32).collect()
}

/// The `torch.stft` / `torch.istft` pair as `BSRoformer` calls them.
struct Stft {
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    /// `torch.hann_window(n_fft)`: periodic.
    window: Vec<f32>,
}

impl Stft {
    fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let window = (0..N_FFT)
            .map(|i| (0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / N_FFT as f64).cos())) as f32)
            .collect();
        Self { forward: planner.plan_fft_forward(N_FFT), inverse: planner.plan_fft_inverse(N_FFT), window }
    }

    /// Frames of a centered STFT over `len` samples (`2 * PAD == N_FFT`).
    fn frames(len: usize) -> usize {
        1 + len / HOP
    }

    /// STFT of one channel into `spec` at layout (channel, bin, frame, re/im)
    /// with `frames` frames; `audio` must be exactly `frames` frames long.
    fn forward(&self, audio: &[f32], channel: usize, frames: usize, spec: &mut [f32]) -> Result<(), String> {
        if Self::frames(audio.len()) != frames {
            return Err(format!("{} samples are {} frames, not {frames}", audio.len(), Self::frames(audio.len())));
        }
        if audio.len() <= PAD {
            return Err(format!("{} samples cannot be reflect-padded by {PAD}", audio.len()));
        }
        // np.pad(mode="reflect") / torch reflect: mirrored without the edge sample.
        let len = audio.len();
        let mut padded = Vec::with_capacity(len + 2 * PAD);
        padded.extend((1..=PAD).rev().map(|i| audio[i]));
        padded.extend_from_slice(audio);
        padded.extend((0..PAD).map(|j| audio[len - 2 - j]));

        let mut input = self.forward.make_input_vec();
        let mut output = self.forward.make_output_vec();
        let mut scratch = self.forward.make_scratch_vec();
        for t in 0..frames {
            let start = t * HOP;
            for (k, x) in input.iter_mut().enumerate() {
                *x = padded[start + k] * self.window[k];
            }
            self.forward.process_with_scratch(&mut input, &mut output, &mut scratch).map_err(|e| format!("fft: {e}"))?;
            for (f, bin) in output.iter().enumerate() {
                let i = ((channel * FREQ_BINS + f) * frames + t) * COMPLEX;
                spec[i] = bin.re;
                spec[i + 1] = bin.im;
            }
        }
        Ok(())
    }

    /// iSTFT of one channel of `spec` (same layout) into `out`:
    /// `HOP * (frames - 1)` samples.
    fn inverse(&self, spec: &[f32], channel: usize, frames: usize, out: &mut Vec<f32>) -> Result<(), String> {
        let total = N_FFT + HOP * (frames - 1);
        let mut acc = vec![0f32; total];
        let mut envelope = vec![0f32; total];
        let mut input = self.inverse.make_input_vec();
        let mut output = self.inverse.make_output_vec();
        let mut scratch = self.inverse.make_scratch_vec();
        let scale = 1.0 / N_FFT as f32;
        for t in 0..frames {
            for (f, bin) in input.iter_mut().enumerate() {
                let i = ((channel * FREQ_BINS + f) * frames + t) * COMPLEX;
                *bin = Complex::new(spec[i], spec[i + 1]);
            }
            // The DC and Nyquist bins of a real signal have no imaginary part;
            // irfft ignores whatever the mask put there, realfft refuses it.
            input[0].im = 0.0;
            input[FREQ_BINS - 1].im = 0.0;
            self.inverse.process_with_scratch(&mut input, &mut output, &mut scratch).map_err(|e| format!("ifft: {e}"))?;
            let start = t * HOP;
            for (k, w) in self.window.iter().enumerate() {
                acc[start + k] += output[k] * w * scale;
                envelope[start + k] += w * w;
            }
        }
        out.clear();
        for i in PAD..total - PAD {
            if envelope[i] < ENVELOPE_FLOOR {
                return Err(format!("window envelope is {} at sample {i}; iSTFT is ill-conditioned", envelope[i]));
            }
            out.push(acc[i] / envelope[i]);
        }
        Ok(())
    }
}

/// Rational-ratio polyphase windowed-sinc resampler (Kaiser window, cutoff at
/// the lower Nyquist). Output sample `n` sits at input position
/// `n * in_rate / out_rate`; samples outside the input read as zero.
struct Resampler {
    /// in_rate / out_rate == down / up, in lowest terms.
    up: usize,
    down: usize,
    /// Kernel per phase (`up` phases), `2 * RESAMPLE_HALF_TAPS + 1` taps each.
    phases: Vec<Vec<f32>>,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        let g = gcd(in_rate, out_rate);
        let up = (out_rate / g) as usize;
        let down = (in_rate / g) as usize;
        let cutoff = if out_rate < in_rate { out_rate as f64 / in_rate as f64 } else { 1.0 };
        let half = RESAMPLE_HALF_TAPS as f64;
        let i0_beta = bessel_i0(KAISER_BETA);
        let phases = (0..up)
            .map(|phase| {
                let frac = phase as f64 / up as f64;
                let mut taps: Vec<f64> = (0..=2 * RESAMPLE_HALF_TAPS)
                    .map(|j| {
                        let x = j as f64 - half - frac;
                        let u = x / half;
                        if u.abs() >= 1.0 {
                            return 0.0;
                        }
                        let kaiser = bessel_i0(KAISER_BETA * (1.0 - u * u).sqrt()) / i0_beta;
                        cutoff * sinc(cutoff * x) * kaiser
                    })
                    .collect();
                let sum: f64 = taps.iter().sum();
                taps.iter_mut().for_each(|t| *t /= sum);
                taps.into_iter().map(|t| t as f32).collect()
            })
            .collect();
        Self { up, down, phases }
    }

    fn run(&self, input: &[f32], out_len: usize) -> Vec<f32> {
        let half = RESAMPLE_HALF_TAPS as isize;
        (0..out_len)
            .map(|n| {
                let position = n * self.down;
                let index = (position / self.up) as isize;
                let taps = &self.phases[position % self.up];
                let mut acc = 0f32;
                for (j, tap) in taps.iter().enumerate() {
                    let i = index + j as isize - half;
                    if i >= 0 && (i as usize) < input.len() {
                        acc += input[i as usize] * tap;
                    }
                }
                acc
            })
            .collect()
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Modified Bessel function of the first kind, order 0 (power series).
fn bessel_i0(x: f64) -> f64 {
    let half = x / 2.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut k = 1.0;
    while term > BESSEL_EPSILON * sum {
        term *= (half / k) * (half / k);
        sum += term;
        k += 1.0;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = r"E:\models\separator\bs_roformer_ep368_fp16.onnx";
    const ORT_DIR: &str = r"E:\tools\ort-dml";
    const FIXTURE: &str = r"E:\datasets\g2\f5";
    /// The fp16 graph's F5 scores (SPEED.md #5a), the line this port is held to.
    const REFERENCE_VOCALS_DB: f64 = 14.891;
    const REFERENCE_RESIDUAL_DB: f64 = 14.941;
    const PASS_TOLERANCE_DB: f64 = 0.5;
    const F5_SECONDS: f64 = 60.0;

    /// Deterministic noise in [-1, 1] (LCG; no rand dependency).
    fn noise(len: usize, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn stft_round_trip_is_exact() {
        let stft = Stft::new();
        for (len, seed) in [(HOP * 20, 1), (CHUNK_SAMPLES, 2)] {
            let frames = Stft::frames(len);
            let audio = noise(len, seed);
            let mut spec = vec![0f32; CHANNELS * FREQ_BINS * frames * COMPLEX];
            stft.forward(&audio, 1, frames, &mut spec).unwrap();
            let mut back = Vec::new();
            stft.inverse(&spec, 1, frames, &mut back).unwrap();
            assert_eq!(back.len(), len);
            let max_err = audio.iter().zip(&back).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
            assert!(max_err < 1e-5, "len {len}: max abs error {max_err}");
        }
    }

    #[test]
    fn chunk_starts_match_audio_separator() {
        // Expected values printed by MDXCSeparator._roformer_chunk_starts(n, 352800, 176400).
        let cases: [(usize, &[usize]); 12] = [
            (0, &[]),
            (1, &[0]),
            (100_000, &[0]),
            (352_799, &[0]),
            (352_800, &[0]),
            (352_801, &[0, 1]),
            (529_200, &[0, 176_400]),
            (529_201, &[0, 176_400, 176_401]),
            (1_000_000, &[0, 176_400, 352_800, 529_200, 647_200]),
            (
                2_646_000,
                &[0, 176_400, 352_800, 529_200, 705_600, 882_000, 1_058_400, 1_234_800, 1_411_200, 1_587_600, 1_764_000, 1_940_400, 2_116_800, 2_293_200],
            ),
            (
                2_822_400,
                &[0, 176_400, 352_800, 529_200, 705_600, 882_000, 1_058_400, 1_234_800, 1_411_200, 1_587_600, 1_764_000, 1_940_400, 2_116_800, 2_293_200, 2_469_600],
            ),
            (
                2_822_401,
                &[
                    0, 176_400, 352_800, 529_200, 705_600, 882_000, 1_058_400, 1_234_800, 1_411_200, 1_587_600, 1_764_000, 1_940_400, 2_116_800, 2_293_200,
                    2_469_600, 2_469_601,
                ],
            ),
        ];
        for (len, expected) in cases {
            assert_eq!(chunk_starts(len, CHUNK_SAMPLES, CHUNK_STEP), expected, "len {len}");
        }
    }

    #[test]
    fn resampler_round_trips_a_tone() {
        let in_rate = 48_000;
        let len = 48_000;
        let tone: Vec<f32> = (0..len).map(|i| (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / in_rate as f64).sin() as f32).collect();
        let down = Resampler::new(in_rate, MODEL_SAMPLE_RATE);
        let model_len = (len as u64 * MODEL_SAMPLE_RATE as u64).div_ceil(in_rate as u64) as usize;
        let at_model = down.run(&tone, model_len);
        assert_eq!(at_model.len(), model_len);
        let back = Resampler::new(MODEL_SAMPLE_RATE, in_rate).run(&at_model, len);
        assert_eq!(back.len(), len);
        let margin = 4 * RESAMPLE_HALF_TAPS;
        let max_err = tone[margin..len - margin]
            .iter()
            .zip(&back[margin..len - margin])
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(max_err < 1e-3, "max abs error {max_err}");
    }

    /// `score.py::si_sdr`, in f64 like the numpy original.
    fn si_sdr(estimate: &[f32], reference: &[f32]) -> f64 {
        let n = estimate.len().min(reference.len());
        let mean = |x: &[f32]| x[..n].iter().map(|v| *v as f64).sum::<f64>() / n as f64;
        let (me, mr) = (mean(estimate), mean(reference));
        let e: Vec<f64> = estimate[..n].iter().map(|v| *v as f64 - me).collect();
        let r: Vec<f64> = reference[..n].iter().map(|v| *v as f64 - mr).collect();
        let dot = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
        let scale = dot(&e, &r) / (dot(&r, &r) + 1e-12);
        let target: Vec<f64> = r.iter().map(|v| scale * v).collect();
        let noise: Vec<f64> = e.iter().zip(&target).map(|(x, t)| x - t).collect();
        10.0 * ((dot(&target, &target) + 1e-12) / (dot(&noise, &noise) + 1e-12)).log10()
    }

    #[test]
    fn si_sdr_of_scaled_reference_plus_orthogonal_noise() {
        // e = 3 r + 0.3 q with q orthogonal to r and both zero-mean:
        // target = 3 r, noise = 0.3 q, SI-SDR = 10 log10(9 / 0.09) = 20 dB.
        let n = 4410;
        let r: Vec<f32> = (0..n).map(|i| (2.0 * std::f64::consts::PI * 10.0 * i as f64 / n as f64).sin() as f32).collect();
        let q: Vec<f32> = (0..n).map(|i| (2.0 * std::f64::consts::PI * 10.0 * i as f64 / n as f64).cos() as f32).collect();
        let e: Vec<f32> = r.iter().zip(&q).map(|(a, b)| 3.0 * a + 0.3 * b).collect();
        let db = si_sdr(&e, &r);
        assert!((db - 20.0).abs() < 1e-4, "{db}");
    }

    /// Reads a 16-bit PCM mono 44.1 kHz WAV as float32 (soundfile's scaling).
    fn read_wav_mono_16(path: &Path) -> Vec<f32> {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(&bytes[0..4], b"RIFF", "{}", path.display());
        assert_eq!(&bytes[8..12], b"WAVE", "{}", path.display());
        let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let mut pos = 12;
        let mut format_ok = false;
        while pos + 8 <= bytes.len() {
            let id = &bytes[pos..pos + 4];
            let size = u32_at(pos + 4) as usize;
            let body = pos + 8;
            match id {
                b"fmt " => {
                    let (tag, channels, rate, bits) = (u16_at(body), u16_at(body + 2), u32_at(body + 4), u16_at(body + 14));
                    assert_eq!((tag, channels, rate, bits), (1, 1, MODEL_SAMPLE_RATE, 16), "{}: not PCM16 mono 44.1k", path.display());
                    format_ok = true;
                }
                b"data" => {
                    assert!(format_ok, "{}: data before fmt", path.display());
                    return bytes[body..body + size].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
                }
                _ => {}
            }
            pos = body + size + (size & 1);
        }
        panic!("{}: no data chunk", path.display());
    }

    fn f5_parity(provider: Provider) {
        let fixture = Path::new(FIXTURE);
        let mix = read_wav_mono_16(&fixture.join("mix.wav"));
        let truth_speech = read_wav_mono_16(&fixture.join("truth_speech.wav"));
        let truth_background = read_wav_mono_16(&fixture.join("truth_background.wav"));
        assert_eq!(mix.len() as f64 / MODEL_SAMPLE_RATE as f64, F5_SECONDS);

        // SEPARATE_MODEL points the run at another graph (the fp32 export, a
        // re-conversion) without a rebuild.
        let model = std::env::var("SEPARATE_MODEL").unwrap_or_else(|_| MODEL.to_string());
        let opened = Instant::now();
        let separator = Separator::open_with(Path::new(&model), Path::new(ORT_DIR), provider).unwrap_or_else(|e| panic!("{e}"));
        println!("[{provider:?}] session ready in {:.1}s", opened.elapsed().as_secs_f64());

        // Warm-up on two chunks: the first run compiles the graph.
        let warm = 2 * CHUNK_SAMPLES;
        let started = Instant::now();
        separator.separate([&mix[..warm], &mix[..warm]], MODEL_SAMPLE_RATE).unwrap_or_else(|e| panic!("{e}"));
        println!("[{provider:?}] warm-up (2 chunks) {:.1}s", started.elapsed().as_secs_f64());

        let started = Instant::now();
        let out = separator.separate([&mix, &mix], MODEL_SAMPLE_RATE).unwrap_or_else(|e| panic!("{e}"));
        let seconds = started.elapsed().as_secs_f64();
        assert_eq!(out.vocals[0].len(), mix.len());
        assert!(out.vocals.iter().chain(out.residual.iter()).all(|c| c.iter().all(|s| s.is_finite())), "non-finite output");

        let mono = |stems: &[Vec<f32>; CHANNELS]| -> Vec<f32> { stems[0].iter().zip(&stems[1]).map(|(a, b)| (a + b) / 2.0).collect() };
        let vocals_db = si_sdr(&mono(&out.vocals), &truth_speech);
        let residual_db = si_sdr(&mono(&out.residual), &truth_background);
        println!(
            "[{provider:?}] vocals_si_sdr {vocals_db:.3} dB (ref {REFERENCE_VOCALS_DB}), residual_si_sdr {residual_db:.3} dB (ref {REFERENCE_RESIDUAL_DB}), seconds {seconds:.1}, rtf {:.3}",
            seconds / F5_SECONDS
        );
        assert!((vocals_db - REFERENCE_VOCALS_DB).abs() <= PASS_TOLERANCE_DB, "vocals {vocals_db:.3} dB vs {REFERENCE_VOCALS_DB}");
        assert!((residual_db - REFERENCE_RESIDUAL_DB).abs() <= PASS_TOLERANCE_DB, "residual {residual_db:.3} dB vs {REFERENCE_RESIDUAL_DB}");
    }

    /// Needs the model, the runtime pack and the F5 fixture on E:\; run with
    /// `cargo test --lib separate -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn f5_parity_directml() {
        f5_parity(Provider::DirectML);
    }

    /// The CPU provider takes minutes (RTF ~7); explicit only.
    #[test]
    #[ignore]
    fn f5_parity_cpu() {
        f5_parity(Provider::Cpu);
    }
}
