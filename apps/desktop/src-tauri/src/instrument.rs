//! The instrument (M3, engine repo `docs/dub-model/m3-instrument-tokens.md`):
//! the speaker reference's VOICE as an invariant, computed by the engine's
//! instrument encoder and projected into [`PATCHES`] pseudo-patches of the
//! dub model's reference segment, which the sidecar prepends to the encoded
//! reference audio. The encoder and the projector travel as one ONNX graph
//! ([`FILE`] in the pack): input `mel` `[1, 64, T]`, the encoder's log-mel;
//! output `patches` `[PATCHES, PATCH_ELEMENTS]`, patch-major, each patch the
//! runtime's own patch layout (frame within the patch slow, latent channel
//! fast), so the floats go on the wire as they come out.
//!
//! The mel is the engine's `instrument_encoder.features` (librosa's
//! `melspectrogram` at 16 kHz, `n_fft` 1280, hop 320, 64 Slaney bands,
//! zero-padded centred frames, then `ln(mel + 1e-6)`), computed by the one
//! mel implementation the speaker encoder also uses (`turns::MelSpectrogram`
//! with [`INSTRUMENT_MEL`]); `instrument_fixture.json` (written by
//! `scripts/instrument_fixture.py` from the Python owner) pins the parity.

#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;

use ort::session::Session;
use ort::value::Tensor;

use crate::turns::{MelParams, MelSpectrogram, RATE};

/// The pack file holding the encoder and the projector (absent until M3's
/// trained projector exists; the pipeline runs without the instrument then).
pub(crate) const FILE: &str = "instrument.onnx";
/// `instrument_encoder.py`: hop 20 ms, `n_fft` four hops, 64 bands over the
/// full band (librosa's default `fmax` = `sr / 2`).
pub(crate) const INSTRUMENT_MEL: MelParams = MelParams { n_fft: 1280, hop: 320, n_channels: 64, fmin_hz: 0.0, fmax_hz: 8000.0 };
/// The encoder reads the first `MAX_CLIP_S` of the clip (`instrument_encoder.MAX_CLIP_S`).
const MAX_CLIP_S: usize = 6;
const LOG_FLOOR: f32 = 1e-6;
/// The contract's K and `feat_dim x patch_size` (64 x 4).
pub(crate) const PATCHES: usize = 4;
pub(crate) const PATCH_ELEMENTS: usize = 256;
const INPUT_NAME: &str = "mel";
const OUTPUT_NAME: &str = "patches";

pub(crate) struct Instrument {
    session: Session,
    mel: MelSpectrogram,
}

impl Instrument {
    pub(crate) fn load(model: &Path) -> Result<Self, String> {
        let session = Session::builder()
            .and_then(|mut b| b.commit_from_file(model))
            .map_err(|e| format!("instrument: cannot load {}: {e}", model.display()))?;
        Ok(Self { session, mel: MelSpectrogram::new(INSTRUMENT_MEL) })
    }

    /// The encoder's log-mel of a 16 kHz float32 clip: `n_channels` x
    /// `frames`, channel-major (the graph's `[1, 64, T]`), plus `frames`.
    pub(crate) fn log_mel(&self, wav16k: &[f32]) -> (Vec<f32>, usize) {
        let wav = &wav16k[..wav16k.len().min(MAX_CLIP_S * RATE)];
        let frames_major = self.mel.compute(wav);
        let n_channels = INSTRUMENT_MEL.n_channels;
        let frames = frames_major.len() / n_channels;
        let mut out = vec![0.0f32; frames_major.len()];
        for t in 0..frames {
            for c in 0..n_channels {
                out[c * frames + t] = (frames_major[t * n_channels + c] + LOG_FLOOR).ln();
            }
        }
        (out, frames)
    }

    /// The reference's instrument as [`PATCHES`] x [`PATCH_ELEMENTS`] floats,
    /// patch-major: what `Tts::synthesize` sends as `instrument_feats`.
    pub(crate) fn patches(&mut self, wav16k: &[f32]) -> Result<Vec<f32>, String> {
        if wav16k.is_empty() {
            return Err("instrument: empty reference".into());
        }
        let (mel, frames) = self.log_mel(wav16k);
        let shape = vec![1i64, INSTRUMENT_MEL.n_channels as i64, frames as i64];
        let input = Tensor::from_array((shape, mel)).map_err(|e| format!("instrument: {e}"))?;
        let outputs = self.session.run(ort::inputs![INPUT_NAME => input]).map_err(|e| format!("instrument: {e}"))?;
        let (out_shape, patches) = outputs[OUTPUT_NAME].try_extract_tensor::<f32>().map_err(|e| format!("instrument: {e}"))?;
        if patches.len() != PATCHES * PATCH_ELEMENTS {
            return Err(format!("instrument: graph returned shape {out_shape:?}, the contract is [{PATCHES}, {PATCH_ELEMENTS}]"));
        }
        Ok(patches.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    /// The 1.24 DirectML build the separator port loads; `ORT_DYLIB_PATH` overrides.
    const ORT_DLL: &str = r"E:\tools\ort-dml\onnxruntime.dll";
    const PLACEHOLDER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/fixtures/instrument_placeholder.onnx");
    /// The Python log-mel of the fixture's tone against ours, per value.
    const MEL_TOL: f32 = 1e-3;

    fn runtime() {
        static RUNTIME: Once = Once::new();
        RUNTIME.call_once(|| {
            let dll = std::env::var("ORT_DYLIB_PATH").unwrap_or_else(|_| ORT_DLL.to_string());
            crate::turns::init_runtime(Path::new(&dll)).expect("onnxruntime.dll loads");
        });
    }

    /// The Python owner's log-mel of a 440 Hz tone (`scripts/instrument_fixture.py`),
    /// frames x bands: the mel contract, pinned to librosa.
    #[test]
    fn log_mel_matches_the_python_owner() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!("instrument_fixture.json")).unwrap();
        let rate = fixture["rate"].as_u64().unwrap() as usize;
        assert_eq!(rate, RATE);
        assert_eq!(fixture["n_fft"].as_u64().unwrap() as usize, INSTRUMENT_MEL.n_fft);
        assert_eq!(fixture["hop"].as_u64().unwrap() as usize, INSTRUMENT_MEL.hop);
        assert_eq!(fixture["n_mels"].as_u64().unwrap() as usize, INSTRUMENT_MEL.n_channels);
        let (hz, seconds, amplitude) = (fixture["tone_hz"].as_f64().unwrap(), fixture["seconds"].as_f64().unwrap(), fixture["amplitude"].as_f64().unwrap());
        let n = (seconds * rate as f64) as usize;
        let tone: Vec<f32> = (0..n).map(|i| (amplitude * (2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64).sin()) as f32).collect();
        let mel = MelSpectrogram::new(INSTRUMENT_MEL);
        let ours = mel.compute(&tone);
        let n_channels = INSTRUMENT_MEL.n_channels;
        assert_eq!(ours.len() / n_channels, fixture["n_frames"].as_u64().unwrap() as usize, "frame count");
        for (t, row) in fixture["log_mel"].as_array().unwrap().iter().enumerate() {
            for (c, want) in row.as_array().unwrap().iter().enumerate() {
                let got = (ours[t * n_channels + c] + LOG_FLOOR).ln();
                let want = want.as_f64().unwrap() as f32;
                assert!((got - want).abs() <= MEL_TOL, "frame {t} band {c}: ours {got} vs python {want}");
            }
        }
    }

    /// The channel-major layout the graph takes: `log_mel` is `compute`
    /// transposed and logged.
    #[test]
    fn log_mel_is_channel_major() {
        runtime();
        let instrument = Instrument::load(Path::new(PLACEHOLDER)).expect("placeholder graph loads");
        let wav: Vec<f32> = (0..RATE).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
        let (mel, frames) = instrument.log_mel(&wav);
        let plain = instrument.mel.compute(&wav);
        assert_eq!(frames, 1 + RATE / INSTRUMENT_MEL.hop);
        for t in [0, 7, frames - 1] {
            for c in [0, 31, 63] {
                assert_eq!(mel[c * frames + t], (plain[t * INSTRUMENT_MEL.n_channels + c] + LOG_FLOOR).ln());
            }
        }
    }

    /// The contract's shape through a graph of the right names and shapes
    /// (random weights): the real `instrument.onnx` replaces the placeholder
    /// without a code change.
    #[test]
    fn placeholder_graph_gives_the_contract_shape() {
        runtime();
        let mut instrument = Instrument::load(Path::new(PLACEHOLDER)).expect("placeholder graph loads");
        let wav: Vec<f32> = (0..2 * RATE).map(|i| (i as f32 * 0.03).sin() * 0.5).collect();
        let patches = instrument.patches(&wav).expect("patches");
        assert_eq!(patches.len(), PATCHES * PATCH_ELEMENTS);
        assert!(patches.iter().all(|x| x.is_finite()));
        // a 20 s clip is read up to MAX_CLIP_S: the same length in, the same out
        let long: Vec<f32> = wav.iter().copied().cycle().take(20 * RATE).collect();
        assert_eq!(instrument.patches(&long).unwrap().len(), PATCHES * PATCH_ELEMENTS);
        assert!(instrument.patches(&[]).is_err());
    }
}
