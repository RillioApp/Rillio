"""S2 parity: PyTorch BS-RoFormer ep368 vs its ONNX export, on the F5 fixture, CPU only.

Both arms consume the same normalized stereo mix, chunked the way
audio-separator's MDXCSeparator chunks a RoFormer (chunk = hop * (dim_t - 1),
step = chunk / overlap, hamming overlap-add, its own chunk schedule), with
overlap 2 (G2's winning arm). Arm (a) runs BSRoformer.forward as-is. Arm (b)
runs a numpy STFT, the ONNX core on ONNX Runtime's CPU provider, and a numpy
iSTFT, with the yaml's STFT parameters. The residual is mix - vocals, as in
MDXCSeparator for a single-target model.

Reported: max abs diff between the two vocals outputs, and SI-SDR of each
arm's vocals vs truth_speech and residual vs truth_background, through G2's
scorer (imported, not reimplemented).

Run: E:\\venvs\\dubbing\\Scripts\\python.exe parity.py [--g2-dir <stage0-gates/g2>]
"""
import argparse
import importlib.util
import json
import sys
import time
from pathlib import Path

import numpy as np
import onnxruntime as ort
import torch
from scipy import signal

from audio_separator.separator.architectures.mdxc_separator import MDXCSeparator
from audio_separator.separator.uvr_lib_v5 import spec_utils

sys.path.insert(0, str(Path(__file__).resolve().parent))
from export_bs_roformer import CKPT, COMPLEX, ONNX_OUT, YAML, load_config, load_model  # noqa: E402

FIXTURE = Path(r"E:\datasets\g2\f5")
G2_DIR = Path(__file__).resolve().parents[2] / "stage0-gates" / "g2"
RESULT = Path(__file__).resolve().parent / "parity_result.json"
OVERLAP = 2
# Separator(normalization_threshold=0.9) default: the mix is lowered to this peak before demixing.
NORMALIZATION_PEAK = 0.9
STEREO = 2
PASS_SI_SDR_TOLERANCE_DB = 0.5


def import_scorer(g2_dir: Path):
    score_py = g2_dir / "score.py"
    if not score_py.exists():
        raise FileNotFoundError(f"G2 scorer not found at {score_py}; pass --g2-dir")
    spec = importlib.util.spec_from_file_location("g2_score", score_py)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Stft:
    """numpy twin of torch.stft/torch.istft as BSRoformer calls them (center, reflect pad, periodic hann, one-sided, not normalized)."""

    def __init__(self, n_fft: int, hop: int, win_length: int):
        if win_length != n_fft:
            raise ValueError(f"win_length {win_length} != n_fft {n_fft}: the model pads no window")
        self.n_fft, self.hop = n_fft, hop
        self.window = 0.5 * (1.0 - np.cos(2.0 * np.pi * np.arange(n_fft) / n_fft))
        expected = torch.hann_window(win_length).numpy()
        if not np.allclose(self.window, expected, atol=1e-7):
            raise RuntimeError("periodic hann window does not match torch.hann_window")

    def forward(self, audio: np.ndarray) -> np.ndarray:
        """(S, L) float -> (S, F, T, 2) float32, T = 1 + L // hop."""
        pad = self.n_fft // 2
        padded = np.pad(audio.astype(np.float64), ((0, 0), (pad, pad)), mode="reflect")
        frames = 1 + (padded.shape[1] - self.n_fft) // self.hop
        idx = np.arange(self.n_fft)[None, :] + self.hop * np.arange(frames)[:, None]
        segments = padded[:, idx] * self.window
        spec = np.fft.rfft(segments, n=self.n_fft, axis=-1)
        spec = np.transpose(spec, (0, 2, 1))
        return np.stack((spec.real, spec.imag), axis=-1).astype(np.float32)

    def inverse(self, spec: np.ndarray) -> np.ndarray:
        """(S, F, T, 2) -> (S, hop * (T - 1)) float32."""
        complex_spec = spec[..., 0].astype(np.float64) + 1j * spec[..., 1].astype(np.float64)
        frames = complex_spec.shape[-1]
        segments = np.fft.irfft(np.transpose(complex_spec, (0, 2, 1)), n=self.n_fft, axis=-1) * self.window
        total = self.n_fft + self.hop * (frames - 1)
        out = np.zeros((spec.shape[0], total))
        envelope = np.zeros(total)
        for t in range(frames):
            start = t * self.hop
            out[:, start : start + self.n_fft] += segments[:, t]
            envelope[start : start + self.n_fft] += self.window ** 2
        pad = self.n_fft // 2
        out, envelope = out[:, pad : total - pad], envelope[pad : total - pad]
        if envelope.min() < 1e-11:
            raise RuntimeError("window envelope has near-zero values; iSTFT is ill-conditioned")
        return (out / envelope).astype(np.float32)


def load_mix(path: Path, sample_rate: int) -> np.ndarray:
    import soundfile as sf

    audio, rate = sf.read(path, dtype="float32", always_2d=True)
    if rate != sample_rate:
        raise ValueError(f"{path} is {rate} Hz, the model wants {sample_rate}")
    mix = audio.T
    if mix.shape[0] == 1:
        mix = np.concatenate([mix, mix], axis=0)
    if mix.shape[0] != STEREO:
        raise ValueError(f"{path} has {mix.shape[0]} channels")
    return spec_utils.normalize(wave=mix, max_peak=NORMALIZATION_PEAK)


def demix(mix: np.ndarray, chunk_size: int, run_chunk, label: str) -> tuple[np.ndarray, float]:
    """MDXCSeparator.demix's RoFormer path with run_chunk in place of the model call."""
    step = chunk_size // OVERLAP
    window = signal.windows.hamming(chunk_size).astype(np.float32)
    result = np.zeros_like(mix)
    counter = np.zeros_like(mix)
    starts = MDXCSeparator._roformer_chunk_starts(mix.shape[1], chunk_size, step)
    t0 = time.perf_counter()
    for i, start in enumerate(starts):
        part = mix[:, start : start + chunk_size]
        length = part.shape[1]
        if length != chunk_size:
            raise RuntimeError(f"chunk {i} is {length} samples, expected {chunk_size}: input shorter than one chunk")
        out = run_chunk(part)
        if out.shape != part.shape:
            raise RuntimeError(f"chunk {i}: model returned {out.shape} for input {part.shape}")
        result[:, start : start + length] += out * window
        counter[:, start : start + length] += window
        print(f"[{label}] chunk {i + 1}/{len(starts)} {time.perf_counter() - t0:.1f}s", flush=True)
    seconds = time.perf_counter() - t0
    return result / np.clip(counter, 1e-10, None), seconds


def torch_arm(model) -> callable:
    def run_chunk(part: np.ndarray) -> np.ndarray:
        with torch.no_grad():
            return model(torch.from_numpy(part).unsqueeze(0))[0].numpy()

    return run_chunk


def onnx_arm(onnx_path: Path, stft: Stft, provider: str) -> callable:
    session = ort.InferenceSession(str(onnx_path), providers=[provider])
    if provider not in session.get_providers():
        raise RuntimeError(f"{provider} is not active; session runs on {session.get_providers()}")

    def run_chunk(part: np.ndarray) -> np.ndarray:
        spec = stft.forward(part)[None]
        (masked,) = session.run(None, {"spec": spec})
        return stft.inverse(masked[0])

    return run_chunk


def score_arm(scorer, vocals: np.ndarray, residual: np.ndarray, truth_speech: np.ndarray, truth_background: np.ndarray) -> dict:
    return {
        "vocals_si_sdr": round(scorer.si_sdr(vocals.mean(axis=0), truth_speech), 3),
        "residual_si_sdr": round(scorer.si_sdr(residual.mean(axis=0), truth_background), 3),
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--g2-dir", type=Path, default=G2_DIR)
    ap.add_argument("--onnx", type=Path, default=ONNX_OUT)
    ap.add_argument("--fixture", type=Path, default=FIXTURE)
    ap.add_argument("--save-dir", type=Path, help="optionally write both arms' stems here")
    ap.add_argument("--provider", default="CPUExecutionProvider", help="ONNX Runtime execution provider for the ONNX arm")
    ap.add_argument("--onnx-only", action="store_true", help="skip the PyTorch arm (timing runs; parity was banked on CPU)")
    args = ap.parse_args()

    scorer = import_scorer(args.g2_dir)
    config = load_config(YAML)
    model_cfg, audio_cfg, inference_cfg = config["model"], config["audio"], config["inference"]
    chunk_size = model_cfg["stft_hop_length"] * (inference_cfg["dim_t"] - 1)
    stft = Stft(model_cfg["stft_n_fft"], model_cfg["stft_hop_length"], model_cfg["stft_win_length"])

    mix = load_mix(args.fixture / "mix.wav", audio_cfg["sample_rate"])
    truth_speech = scorer.load_mono(args.fixture / "truth_speech.wav")
    truth_background = scorer.load_mono(args.fixture / "truth_background.wav")

    audio_seconds = mix.shape[1] / audio_cfg["sample_rate"]
    if args.onnx_only:
        # Timing run on a given provider: the ONNX arm alone, scored against
        # the truth stems so a provider that computes wrongly cannot pass on speed.
        run_chunk = onnx_arm(args.onnx, stft, args.provider)
        demix(mix[:, :chunk_size * 2], chunk_size, run_chunk, "warm-up")
        vocals_onnx, seconds_onnx = demix(mix, chunk_size, run_chunk, args.provider)
        result = {
            "fixture": str(args.fixture), "onnx": str(args.onnx), "provider": args.provider, "audio_seconds": audio_seconds,
            **score_arm(scorer, vocals_onnx, mix - vocals_onnx, truth_speech, truth_background),
            "seconds": round(seconds_onnx, 1), "rtf": round(seconds_onnx / audio_seconds, 3),
        }
        out = RESULT.with_name(f"timing_{args.provider}.json")
        out.write_text(json.dumps(result, indent=2), encoding="utf-8")
        print(json.dumps(result, indent=2))
        return

    model = load_model(CKPT, config)
    vocals_torch, seconds_torch = demix(mix, chunk_size, torch_arm(model), "torch")
    vocals_onnx, seconds_onnx = demix(mix, chunk_size, onnx_arm(args.onnx, stft, args.provider), "onnx")
    residual_torch, residual_onnx = mix - vocals_torch, mix - vocals_onnx

    torch_scores = score_arm(scorer, vocals_torch, residual_torch, truth_speech, truth_background)
    onnx_scores = score_arm(scorer, vocals_onnx, residual_onnx, truth_speech, truth_background)
    deltas = {k: round(abs(onnx_scores[k] - torch_scores[k]), 3) for k in torch_scores}
    result = {
        "fixture": str(args.fixture),
        "onnx": str(args.onnx),
        "chunk_size": chunk_size,
        "overlap": OVERLAP,
        "audio_seconds": audio_seconds,
        "max_abs_diff_vocals": float(np.max(np.abs(vocals_torch - vocals_onnx))),
        "vocals_peak_torch": float(np.max(np.abs(vocals_torch))),
        "torch": {**torch_scores, "seconds": round(seconds_torch, 1), "rtf_cpu": round(seconds_torch / audio_seconds, 3)},
        "onnx_cpu": {**onnx_scores, "seconds": round(seconds_onnx, 1), "rtf_cpu": round(seconds_onnx / audio_seconds, 3)},
        "si_sdr_delta_db": deltas,
        "pass": all(d <= PASS_SI_SDR_TOLERANCE_DB for d in deltas.values()),
    }
    RESULT.write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(json.dumps(result, indent=2))

    if args.save_dir:
        import soundfile as sf

        args.save_dir.mkdir(parents=True, exist_ok=True)
        for name, stem in (("vocals_torch", vocals_torch), ("residual_torch", residual_torch), ("vocals_onnx", vocals_onnx), ("residual_onnx", residual_onnx)):
            sf.write(args.save_dir / f"{name}.wav", stem.T, audio_cfg["sample_rate"])


if __name__ == "__main__":
    main()
