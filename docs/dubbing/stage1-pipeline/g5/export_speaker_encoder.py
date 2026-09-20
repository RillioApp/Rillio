"""Export Resemblyzer's GE2E VoiceEncoder (the 3-layer LSTM in
resemblyzer/pretrained.pt) to ONNX for the Rust speaker-turn port
(apps/desktop/src-tauri/src/turns.rs).

    python export_speaker_encoder.py [--out E:\\models\\speaker\\ge2e_resemblyzer.onnx]
                                     [--stem E:\\datasets\\g5\\stem16k.wav] [--slices 20]
                                     [--dump E:\\datasets\\g5\\parity]

The graph holds ONLY the network: mel frames in, one L2-normalised embedding
out. The mel spectrogram and the partial-utterance slicing stay outside it
(librosa here, a Rust reimplementation there). Everything below is therefore
the contract the Rust side must reproduce:

AUDIO
  16 kHz mono float32; int16 samples / 32768.0 (turns.py's convention).

MEL (resemblyzer.audio.wav_to_mel_spectrogram, librosa 1.0 defaults spelled out)
  n_fft = 400 (25 ms), hop = 160 (10 ms), win_length = n_fft, window = periodic
  Hann (scipy get_window("hann", 400, fftbins=True): w[k] = 0.5 - 0.5 cos(2 pi k / 400)),
  center = True with zero ("constant") padding of n_fft // 2 = 200 samples on
  each side, so frame t covers samples [t*160 - 200, t*160 + 200) and the frame
  count is 1 + len(wav) // 160. Spectrum = |rfft|^2 (power = 2.0, 201 bins).
  Filterbank = librosa.filters.mel(sr=16000, n_fft=400, n_mels=40, fmin=0,
  fmax=8000, htk=False, norm="slaney"): 42 points evenly spaced on the Slaney
  mel scale (linear below 1000 Hz at 200/3 Hz per mel, log above with
  step ln(6.4)/27 per mel), triangular weights on the 201 rfft bin centres
  (k * 8000 / 200 Hz), each row scaled by 2 / (f[m+2] - f[m]). Output is the
  raw power mel (frames, 40) as float32: NO log, NO dB, NO normalisation.

PARTIALS (VoiceEncoder.compute_partial_slices with rate=1.3, min_coverage=0.75)
  samples_per_frame = 160; n_frames = ceil((n_samples + 1) / 160);
  frame_step = round(16000 / 1.3 / 160) = 77; steps = max(1, n_frames - 160 + 77 + 1);
  partial i (for i in range(0, steps, 77)) = mel frames [i, i + 160), i.e. wav
  samples [i*160, (i+160)*160). If (n_samples - last_start*160) / 25600 < 0.75
  and there is more than one partial, the last one is dropped. When the last
  partial's wav end >= n_samples the wav is zero-padded up to that end. The mel
  is computed ONCE over the padded wav and then sliced (never per partial: the
  centre padding would differ).

EMBEDDING
  Each partial goes through the graph as one row; the utterance embedding is
  the mean of the partial embeddings, L2-normalised. Graph I/O: input "mels"
  float32 [batch, frames, 40] (both dynamic; the encoder trained on 160-frame
  partials, feed it those), output "embedding" float32 [batch, 256], each row
  ReLU'd and L2-normalised (so every coordinate is in [0, 1]).

Validation: 20 random slices of the stem (0.6 s to 8 s, covering the padded
single-partial case and the coverage rule) embedded through the ONNX graph with
the slicing above must match VoiceEncoder.embed_utterance to cosine >= 0.999.

--dump writes, for one fixed 3 s slice, the reference the Rust parity tests
read: wav.f32 (the raw slice), mel.f32 (wav_to_mel_spectrogram of the raw
slice, frames x 40 row-major), embedding.f32 (embed_utterance of the slice,
256), and manifest.json with the shapes and the slice position.
"""
import argparse
import json
from pathlib import Path

import numpy as np
import onnxruntime
import soundfile as sf
import torch
from resemblyzer import VoiceEncoder, audio
from resemblyzer.hparams import mel_n_channels, mel_window_step, partials_n_frames, sampling_rate

RATE = sampling_rate
PARTIAL_RATE = 1.3
MIN_COVERAGE = 0.75
MIN_COSINE = 0.999
OPSET = 17
INPUT_NAME = "mels"
OUTPUT_NAME = "embedding"
# The dumped slice: 3 s from 24.0 s (inside a turn of turns.jsonl, so it is speech).
DUMP_START_S = 24.0
DUMP_LEN_S = 3.0
SLICE_MIN_S = 0.6
SLICE_MAX_S = 8.0
SEED = 0


def partial_slices(n_samples: int) -> list[tuple[int, int]]:
    """compute_partial_slices reproduced as (mel_start, mel_end) frame pairs."""
    samples_per_frame = int(RATE * mel_window_step / 1000)
    n_frames = int(np.ceil((n_samples + 1) / samples_per_frame))
    frame_step = int(np.round((RATE / PARTIAL_RATE) / samples_per_frame))
    steps = max(1, n_frames - partials_n_frames + frame_step + 1)
    slices = [(i, i + partials_n_frames) for i in range(0, steps, frame_step)]
    last_start = slices[-1][0] * samples_per_frame
    coverage = (n_samples - last_start) / (partials_n_frames * samples_per_frame)
    if coverage < MIN_COVERAGE and len(slices) > 1:
        slices = slices[:-1]
    return slices


def embed_onnx(session: onnxruntime.InferenceSession, wav: np.ndarray) -> np.ndarray:
    samples_per_frame = int(RATE * mel_window_step / 1000)
    slices = partial_slices(len(wav))
    max_wave_length = slices[-1][1] * samples_per_frame
    if max_wave_length >= len(wav):
        wav = np.pad(wav, (0, max_wave_length - len(wav)), "constant")
    mel = audio.wav_to_mel_spectrogram(wav)
    mels = np.stack([mel[s:e] for s, e in slices]).astype(np.float32)
    partials = session.run([OUTPUT_NAME], {INPUT_NAME: mels})[0]
    raw = partials.mean(axis=0)
    return raw / np.linalg.norm(raw)


def cosine(a: np.ndarray, b: np.ndarray) -> float:
    return float(a @ b / (np.linalg.norm(a) * np.linalg.norm(b)))


def export(encoder: VoiceEncoder, out: Path) -> None:
    out.parent.mkdir(parents=True, exist_ok=True)
    dummy = torch.zeros(1, partials_n_frames, mel_n_channels)
    # The TorchScript exporter maps nn.LSTM onto the ONNX LSTM op with a free
    # sequence axis; the dynamo exporter decomposes it.
    torch.onnx.export(
        encoder, (dummy,), str(out), dynamo=False, opset_version=OPSET,
        input_names=[INPUT_NAME], output_names=[OUTPUT_NAME],
        dynamic_axes={INPUT_NAME: {0: "batch", 1: "frames"}, OUTPUT_NAME: {0: "batch"}},
        do_constant_folding=True,
    )


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=Path(r"E:\models\speaker\ge2e_resemblyzer.onnx"))
    ap.add_argument("--stem", type=Path, default=Path(r"E:\datasets\g5\stem16k.wav"))
    ap.add_argument("--slices", type=int, default=20)
    ap.add_argument("--dump", type=Path, default=None)
    a = ap.parse_args()

    encoder = VoiceEncoder("cpu", verbose=False)
    encoder.eval()
    export(encoder, a.out)
    print(f"exported -> {a.out} ({a.out.stat().st_size} bytes)")

    samples, rate = sf.read(str(a.stem), dtype="int16")
    if rate != RATE or samples.ndim != 1:
        raise SystemExit(f"{a.stem}: need {RATE} Hz mono, got {rate} Hz {samples.ndim}-d")
    wav = samples.astype(np.float32) / 32768.0

    session = onnxruntime.InferenceSession(str(a.out), providers=["CPUExecutionProvider"])
    rng = np.random.default_rng(SEED)
    worst = 1.0
    for k in range(a.slices):
        n = int(rng.uniform(SLICE_MIN_S, SLICE_MAX_S) * RATE)
        start = int(rng.integers(0, len(wav) - n))
        piece = wav[start:start + n]
        ref = encoder.embed_utterance(piece)
        got = embed_onnx(session, piece)
        c = cosine(ref, got)
        worst = min(worst, c)
        print(f"slice {k:2d}: start {start / RATE:8.2f} s, {n / RATE:5.2f} s, {len(partial_slices(n))} partials, cosine {c:.6f}")
    print(f"min cosine over {a.slices} slices: {worst:.6f}")
    if worst < MIN_COSINE:
        raise SystemExit(f"ONNX embedding drifts from VoiceEncoder: min cosine {worst:.6f} < {MIN_COSINE}")

    if a.dump is not None:
        a.dump.mkdir(parents=True, exist_ok=True)
        start = int(DUMP_START_S * RATE)
        n = int(DUMP_LEN_S * RATE)
        piece = wav[start:start + n]
        mel = audio.wav_to_mel_spectrogram(piece)
        embedding = encoder.embed_utterance(piece).astype(np.float32)
        piece.astype(np.float32).tofile(a.dump / "wav.f32")
        mel.astype(np.float32).tofile(a.dump / "mel.f32")
        embedding.tofile(a.dump / "embedding.f32")
        manifest = {
            "start_sample": start, "n_samples": n, "mel_frames": int(mel.shape[0]),
            "n_mels": int(mel.shape[1]), "embedding_dim": int(embedding.shape[0]),
            "partials": partial_slices(n), "onnx_cosine": cosine(embedding, embed_onnx(session, piece)),
        }
        (a.dump / "manifest.json").write_text(json.dumps(manifest, indent=1), encoding="utf-8")
        print(f"dumped {n} samples, mel {mel.shape}, embedding {embedding.shape} -> {a.dump}")


if __name__ == "__main__":
    main()
