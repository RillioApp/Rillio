"""S2 speed: single-chunk microbenchmark of the ONNX core on one provider.

Takes the first 352,800-sample chunk of the F5 mix, STFTs it exactly as
parity.py does, and times the ONNX core alone (IO binding, device-resident
input/output, run_with_iobinding is synchronous) so the number is the graph,
not the host copies. Also times a plain session.run for the copy overhead.

Diagnostics: --log-severity 0 prints ORT's node placement to stderr (redirect
it to a file and grep "placed on"); --profile writes an ORT profile and
prints the per-op-type sum. --frames N slices the spectrogram to N frames to
see how the cost scales; --batch B stacks B copies of the chunk.

Run: E:\\venvs\\dubbing\\Scripts\\python.exe speed_bench.py [--onnx <path>] [--profile] [--log-severity 0]
"""
import argparse
import json
import sys
import time
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
from export_bs_roformer import ONNX_OUT, YAML, load_config  # noqa: E402
from parity import FIXTURE, Stft, load_mix  # noqa: E402
from speed_common import CUDA, add_session_args, log, make_session, profile_summary, sync_cuda_ortvalue  # noqa: E402


def chunk_spec(config: dict, frames: int | None, batch: int) -> np.ndarray:
    model_cfg, audio_cfg, inference_cfg = config["model"], config["audio"], config["inference"]
    chunk_size = model_cfg["stft_hop_length"] * (inference_cfg["dim_t"] - 1)
    stft = Stft(model_cfg["stft_n_fft"], model_cfg["stft_hop_length"], model_cfg["stft_win_length"])
    mix = load_mix(FIXTURE / "mix.wav", audio_cfg["sample_rate"])
    spec = stft.forward(mix[:, :chunk_size])[None]
    if frames is not None:
        spec = np.ascontiguousarray(spec[:, :, :, :frames])
    if batch > 1:
        spec = np.ascontiguousarray(np.repeat(spec, batch, axis=0))
    return spec


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--onnx", type=Path, default=ONNX_OUT)
    ap.add_argument("--provider", default=CUDA)
    ap.add_argument("--frames", type=int)
    ap.add_argument("--batch", type=int, default=1)
    ap.add_argument("--warmup", type=int, default=2)
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--input-name", default="spec")
    ap.add_argument("--output-name", default="masked_spec")
    ap.add_argument("--fp16-io", action="store_true", help="feed/collect float16 (for a model converted without keep_io_types)")
    ap.add_argument("--output-mode", choices=["prealloc", "device"], default="device", help="IO binding output: pre-allocated OrtValue, or device-allocated by ORT per run")
    ap.add_argument("--probe-binding", action="store_true", help="try three input-binding variants in the same session and diff each against the plain run")
    ap.add_argument("--ref-cpu", type=Path, help="also run this (original, parity-verified) graph on the CPU provider and diff both CUDA outputs against it")
    add_session_args(ap)
    args = ap.parse_args()

    config = load_config(YAML)
    spec = chunk_spec(config, args.frames, args.batch)
    dtype = np.float16 if args.fp16_io else np.float32
    spec = spec.astype(dtype)
    log(f"[bench] input {spec.shape} {spec.dtype} provider {args.provider}")

    t0 = time.perf_counter()
    session = make_session(args.onnx, args, args.provider)
    log(f"[bench] session created in {time.perf_counter() - t0:.1f}s, providers {session.get_providers()}")

    import onnxruntime as ort

    device = "cuda" if args.provider == CUDA else "cpu"
    if device == "cuda":
        # ortvalue_from_numpy(..., "cuda"), not from_shape_and_type + update_inplace: see --probe-binding.
        x = ort.OrtValue.ortvalue_from_numpy(spec, "cuda", 0)
        y = sync_cuda_ortvalue(spec.shape, dtype)
    else:
        x = ort.OrtValue.ortvalue_from_numpy(spec)
        y = ort.OrtValue.ortvalue_from_numpy(np.empty_like(spec))
    binding = session.io_binding()
    binding.bind_ortvalue_input(args.input_name, x)
    if args.output_mode == "prealloc":
        binding.bind_ortvalue_output(args.output_name, y)
    else:
        # ORT allocates the output on the device per run; copy_outputs_to_cpu fetches it.
        binding.bind_output(args.output_name, device)

    def fetch_bound() -> np.ndarray:
        if args.output_mode == "prealloc":
            return y.numpy().astype(np.float32)
        return binding.copy_outputs_to_cpu()[0].astype(np.float32)

    for _ in range(args.warmup):
        session.run_with_iobinding(binding)
    graph_times = []
    bound_outputs = []
    for _ in range(args.runs):
        t = time.perf_counter()
        session.run_with_iobinding(binding)
        graph_times.append(time.perf_counter() - t)
        if len(bound_outputs) < 2:
            bound_outputs.append(fetch_bound())
    out_bound = bound_outputs[-1]

    plain_times = []
    plain_outputs = []
    for _ in range(max(2, args.runs // 2)):
        t = time.perf_counter()
        (out_plain,) = session.run([args.output_name], {args.input_name: spec})
        plain_times.append(time.perf_counter() - t)
        if len(plain_outputs) < 2:
            plain_outputs.append(out_plain.astype(np.float32))
    out_plain = plain_outputs[-1]

    def maxdiff(a, b) -> float:
        return float(np.max(np.abs(a - b)))

    max_diff = maxdiff(out_bound, out_plain)
    determinism = {
        "bound_run_vs_bound_run": maxdiff(bound_outputs[0], bound_outputs[-1]),
        "plain_run_vs_plain_run": maxdiff(plain_outputs[0], plain_outputs[-1]),
    }
    if args.probe_binding and device == "cuda":
        import onnxruntime as ort

        determinism["x_device_max_abs"] = float(np.max(np.abs(x.numpy())))
        determinism["bound_out_max_abs"] = float(np.max(np.abs(out_bound)))
        probes = {}
        # (a) fresh device OrtValue per run
        b = session.io_binding()
        xv = ort.OrtValue.ortvalue_from_numpy(spec, "cuda", 0)
        b.bind_ortvalue_input(args.input_name, xv)
        b.bind_output(args.output_name, "cuda")
        session.run_with_iobinding(b)
        probes["fresh_cuda_ortvalue"] = maxdiff(b.copy_outputs_to_cpu()[0].astype(np.float32), out_plain)
        # (b) CPU numpy bound as input, ORT copies
        b = session.io_binding()
        b.bind_cpu_input(args.input_name, spec)
        b.bind_output(args.output_name, "cuda")
        session.run_with_iobinding(b)
        probes["cpu_numpy_input"] = maxdiff(b.copy_outputs_to_cpu()[0].astype(np.float32), out_plain)
        # (c) from_shape_and_type + update_inplace (the path that was wrong at 801 frames), output to cpu
        b = session.io_binding()
        xu = sync_cuda_ortvalue(spec.shape, dtype)
        xu.update_inplace(spec)
        probes["update_inplace_roundtrip_max_abs_diff"] = maxdiff(xu.numpy().astype(np.float32), spec.astype(np.float32))
        b.bind_ortvalue_input(args.input_name, xu)
        b.bind_output(args.output_name, "cpu")
        session.run_with_iobinding(b)
        probes["update_inplace_cpu_out"] = maxdiff(b.copy_outputs_to_cpu()[0].astype(np.float32), out_plain)
        determinism["binding_probes_vs_plain"] = probes
    if args.ref_cpu:
        import onnxruntime as ort

        ref_session = ort.InferenceSession(str(args.ref_cpu), providers=["CPUExecutionProvider"])
        t = time.perf_counter()
        (ref,) = ref_session.run([args.output_name], {args.input_name: spec.astype(np.float32)})
        determinism["cpu_ref_seconds"] = round(time.perf_counter() - t, 1)
        determinism["ref_peak"] = float(np.max(np.abs(ref)))
        determinism["bound_vs_cpu_ref"] = maxdiff(out_bound, ref)
        determinism["plain_vs_cpu_ref"] = maxdiff(out_plain, ref)
    chunk_seconds = (spec.shape[3] - 1) * config["model"]["stft_hop_length"] / config["audio"]["sample_rate"] * spec.shape[0]
    graph_ms = 1000 * float(np.median(graph_times))
    result = {
        "onnx": str(args.onnx), "provider": args.provider, "input_shape": list(spec.shape), "dtype": str(spec.dtype),
        "opt_level": args.opt_level, "mem_pattern": not args.no_mem_pattern, "cudnn_conv_algo": args.cudnn_conv_algo, "tf32": args.tf32,
        "output_mode": args.output_mode,
        "graph_ms_median": round(graph_ms, 1), "graph_ms_all": [round(1000 * t, 1) for t in graph_times],
        "plain_run_ms_median": round(1000 * float(np.median(plain_times)), 1),
        "graph_rtf": round(graph_ms / 1000 / chunk_seconds, 4),
        "bound_vs_plain_max_abs_diff": max_diff,
        "determinism": determinism,
        "output_finite": bool(np.isfinite(out_bound).all()),
    }
    if args.profile:
        result["profile"] = profile_summary(session)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
