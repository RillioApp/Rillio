"""S2 speed: parity.py's --onnx-only timing harness plus the speed knobs.

Same fixture handling, chunk schedule, hamming overlap-add and scorer as
parity.py (imported, not copied): F5, overlap 2, chunk 352,800, 14 chunks,
SI-SDR of vocals vs truth_speech and mix - vocals vs truth_background. What
is added, all as flags:

  --batch B       run B chunks per session call (dynamic batch axis)
  --io-binding    device-resident input/output OrtValues; the graph time is
                  reported separately from the host<->device copies
  --fp16-io       feed/collect float16 (models converted without keep_io_types)
  --tag NAME      write timing_<NAME>.json next to this file
  session knobs   --opt-level, --no-mem-pattern, --cudnn-conv-algo, --profile,
                  --log-severity, --cuda-graph, --tf32 (speed_common.py)

The CUDA EP is always capped at 8 GB (kNextPowerOfTwo) and every CUDA session
waits for the shared-GPU gate. "rtf" is wall time of the whole demix over the
60 s of audio, as parity.py reports it; "graph_rtf" is the session calls only.

Run: E:\\venvs\\dubbing\\Scripts\\python.exe speed_parity.py --g2-dir <stage0-gates/g2> --tag baseline
"""
import argparse
import json
import sys
import time
from pathlib import Path

import numpy as np
from scipy import signal

sys.path.insert(0, str(Path(__file__).resolve().parent))
from audio_separator.separator.architectures.mdxc_separator import MDXCSeparator  # noqa: E402
from export_bs_roformer import ONNX_OUT, YAML, load_config  # noqa: E402
from parity import FIXTURE, G2_DIR, OVERLAP, PASS_SI_SDR_TOLERANCE_DB, Stft, demix, import_scorer, load_mix, score_arm  # noqa: E402
from speed_common import CUDA, add_session_args, log, make_session, profile_summary  # noqa: E402

# parity_result.json, PyTorch arm (== G2): the SI-SDR every construction is held to.
REFERENCE_VOCALS_DB = 14.888
REFERENCE_RESIDUAL_DB = 14.937


class OnnxCore:
    """The ONNX core as a callable over a batch of real-view spectrograms, with optional device-resident IO."""

    def __init__(self, session, batch: int, spec_shape: tuple, io_binding: bool, dtype, input_name: str, output_name: str):
        self.session, self.dtype = session, dtype
        self.input_name, self.output_name = input_name, output_name
        self.graph_seconds = 0.0
        self.calls = 0
        self.binding = None
        self.io_binding = io_binding

    def __call__(self, spec: np.ndarray) -> np.ndarray:
        spec = np.ascontiguousarray(spec.astype(self.dtype, copy=False))
        if self.io_binding:
            # A fresh device OrtValue per call: OrtValue.update_inplace into a preallocated
            # device buffer fed the graph a wrong input at 801 frames (speed_bench.py --probe-binding),
            # while ortvalue_from_numpy(..., "cuda") matches the plain run to 0.0. The output is
            # device-allocated by ORT and copied out after the run.
            import onnxruntime as ort

            x = ort.OrtValue.ortvalue_from_numpy(spec, "cuda", 0)
            binding = self.session.io_binding()
            binding.bind_ortvalue_input(self.input_name, x)
            binding.bind_output(self.output_name, "cuda")
            t = time.perf_counter()
            self.session.run_with_iobinding(binding)
            self.graph_seconds += time.perf_counter() - t
            self.calls += 1
            return binding.copy_outputs_to_cpu()[0]
        t = time.perf_counter()
        (out,) = self.session.run([self.output_name], {self.input_name: np.ascontiguousarray(spec)})
        self.graph_seconds += time.perf_counter() - t
        self.calls += 1
        return out


def demix_batched(mix: np.ndarray, chunk_size: int, stft: Stft, core: OnnxCore, batch: int, label: str) -> tuple[np.ndarray, float]:
    """parity.demix with the model calls grouped B chunks at a time; the accumulation order is unchanged."""
    step = chunk_size // OVERLAP
    window = signal.windows.hamming(chunk_size).astype(np.float32)
    result = np.zeros_like(mix)
    counter = np.zeros_like(mix)
    starts = MDXCSeparator._roformer_chunk_starts(mix.shape[1], chunk_size, step)
    t0 = time.perf_counter()
    stft_seconds = 0.0
    for b0 in range(0, len(starts), batch):
        group = starts[b0 : b0 + batch]
        parts = [mix[:, s : s + chunk_size] for s in group]
        for i, part in enumerate(parts):
            if part.shape[1] != chunk_size:
                raise RuntimeError(f"chunk {b0 + i} is {part.shape[1]} samples, expected {chunk_size}")
        t = time.perf_counter()
        specs = np.stack([stft.forward(p) for p in parts])
        stft_seconds += time.perf_counter() - t
        masked = core(specs)
        if masked.shape != specs.shape:
            raise RuntimeError(f"group at chunk {b0}: model returned {masked.shape} for input {specs.shape}")
        t = time.perf_counter()
        outs = [stft.inverse(m.astype(np.float32)) for m in masked]
        stft_seconds += time.perf_counter() - t
        for i, (start, out) in enumerate(zip(group, outs)):
            if out.shape != parts[i].shape:
                raise RuntimeError(f"chunk {b0 + i}: iSTFT returned {out.shape} for {parts[i].shape}")
            result[:, start : start + chunk_size] += out * window
            counter[:, start : start + chunk_size] += window
        log(f"[{label}] chunks {b0 + 1}-{b0 + len(group)}/{len(starts)} {time.perf_counter() - t0:.1f}s")
    seconds = time.perf_counter() - t0
    log(f"[{label}] stft+istft {stft_seconds:.1f}s of {seconds:.1f}s")
    demix_batched.last_stft_seconds = stft_seconds
    return result / np.clip(counter, 1e-10, None), seconds


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--g2-dir", type=Path, default=G2_DIR)
    ap.add_argument("--onnx", type=Path, default=ONNX_OUT)
    ap.add_argument("--fixture", type=Path, default=FIXTURE)
    ap.add_argument("--provider", default=CUDA)
    ap.add_argument("--batch", type=int, default=1)
    ap.add_argument("--io-binding", action="store_true")
    ap.add_argument("--fp16-io", action="store_true")
    ap.add_argument("--input-name", default="spec")
    ap.add_argument("--output-name", default="masked_spec")
    ap.add_argument("--tag", required=True)
    ap.add_argument("--batched-loop", action="store_true", help="use demix_batched even for --batch 1 (it logs the stft+istft seconds)")
    ap.add_argument("--save-dir", type=Path, help="optionally write the vocals/residual wavs here")
    add_session_args(ap)
    args = ap.parse_args()
    if args.io_binding and args.provider != CUDA:
        raise ValueError("--io-binding is the CUDA device-resident path")

    scorer = import_scorer(args.g2_dir)
    config = load_config(YAML)
    model_cfg, audio_cfg, inference_cfg = config["model"], config["audio"], config["inference"]
    chunk_size = model_cfg["stft_hop_length"] * (inference_cfg["dim_t"] - 1)
    frames = inference_cfg["dim_t"]
    stft = Stft(model_cfg["stft_n_fft"], model_cfg["stft_hop_length"], model_cfg["stft_win_length"])

    mix = load_mix(args.fixture / "mix.wav", audio_cfg["sample_rate"])
    truth_speech = scorer.load_mono(args.fixture / "truth_speech.wav")
    truth_background = scorer.load_mono(args.fixture / "truth_background.wav")
    audio_seconds = mix.shape[1] / audio_cfg["sample_rate"]

    t0 = time.perf_counter()
    session = make_session(args.onnx, args, args.provider)
    session_seconds = time.perf_counter() - t0
    log(f"[session] {session_seconds:.1f}s providers {session.get_providers()}")
    dtype = np.float16 if args.fp16_io else np.float32
    core = OnnxCore(session, args.batch, (2, model_cfg["dim_freqs_in"], frames, 2), args.io_binding, dtype, args.input_name, args.output_name)

    if args.batch == 1 and not args.batched_loop:
        def run_chunk(part: np.ndarray) -> np.ndarray:
            return stft.inverse(core(stft.forward(part)[None])[0].astype(np.float32))

        demix(mix[:, : chunk_size * 2], chunk_size, run_chunk, "warm-up")
        core.graph_seconds, core.calls = 0.0, 0
        vocals, seconds = demix(mix, chunk_size, run_chunk, args.tag)
    else:
        demix_batched(mix[:, : chunk_size * (args.batch + 1)], chunk_size, stft, core, args.batch, "warm-up")
        core.graph_seconds, core.calls = 0.0, 0
        vocals, seconds = demix_batched(mix, chunk_size, stft, core, args.batch, args.tag)

    scores = score_arm(scorer, vocals, mix - vocals, truth_speech, truth_background)
    deltas = {
        "vocals": round(abs(scores["vocals_si_sdr"] - REFERENCE_VOCALS_DB), 3),
        "residual": round(abs(scores["residual_si_sdr"] - REFERENCE_RESIDUAL_DB), 3),
    }
    result = {
        "tag": args.tag, "fixture": str(args.fixture), "onnx": str(args.onnx), "provider": args.provider,
        "batch": args.batch, "io_binding": args.io_binding, "fp16_io": args.fp16_io,
        "opt_level": args.opt_level, "mem_pattern": not args.no_mem_pattern, "cudnn_conv_algo": args.cudnn_conv_algo,
        "tf32": args.tf32, "cuda_graph": args.cuda_graph,
        "audio_seconds": audio_seconds, **scores,
        "si_sdr_delta_vs_torch_db": deltas, "pass_si_sdr": all(d <= PASS_SI_SDR_TOLERANCE_DB for d in deltas.values()),
        "seconds": round(seconds, 1), "rtf": round(seconds / audio_seconds, 3),
        "graph_seconds": round(core.graph_seconds, 1), "graph_rtf": round(core.graph_seconds / audio_seconds, 3),
        "session_calls": core.calls, "session_create_seconds": round(session_seconds, 1),
        "stft_istft_seconds": round(getattr(demix_batched, "last_stft_seconds", float("nan")), 1),
        "pass_rtf": seconds / audio_seconds <= 0.2,
        "output_finite": bool(np.isfinite(vocals).all()),
    }
    if args.profile:
        result["profile"] = profile_summary(session)
    out = Path(__file__).resolve().parent / f"timing_{args.tag}.json"
    out.write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(json.dumps(result, indent=2))

    if args.save_dir:
        import soundfile as sf

        args.save_dir.mkdir(parents=True, exist_ok=True)
        sf.write(args.save_dir / f"vocals_{args.tag}.wav", vocals.T, audio_cfg["sample_rate"])
        sf.write(args.save_dir / f"residual_{args.tag}.wav", (mix - vocals).T, audio_cfg["sample_rate"])


if __name__ == "__main__":
    main()
