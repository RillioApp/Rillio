"""S2 speed: shared pieces for speed_bench.py / speed_parity.py.

- wait_for_gpu: the GPU is shared; every CUDA session waits for the vulkan
  measurement's "[vulkan] done" line first.
- make_session: ONNX Runtime session with the CUDA EP capped at 8 GB
  (arena kNextPowerOfTwo), the session-option knobs as arguments.
- profile_summary: aggregate an ORT profile json by op type.
"""
import collections
import json
import os
import sys
import time
from pathlib import Path

import numpy as np
import onnxruntime as ort

GPU_GATE = Path(r"E:\datasets\g1\vulkan-timing.log")
GPU_GATE_LINE = "[vulkan] done"
GPU_MEM_LIMIT = 8 * 1024**3
CUDA = "CUDAExecutionProvider"
OPT_LEVELS = {
    "all": ort.GraphOptimizationLevel.ORT_ENABLE_ALL,
    "extended": ort.GraphOptimizationLevel.ORT_ENABLE_EXTENDED,
    "basic": ort.GraphOptimizationLevel.ORT_ENABLE_BASIC,
    "disable": ort.GraphOptimizationLevel.ORT_DISABLE_ALL,
}


def wait_for_gpu(poll_seconds: float = 30.0) -> None:
    """Block until the vulkan timing log says it is done. Never skipped: the GPU is shared."""
    if os.environ.get("RILLIO_SKIP_GPU_GATE") == "1":
        raise RuntimeError("RILLIO_SKIP_GPU_GATE is not honored: the gate is a hard rule")
    waited = 0.0
    while True:
        text = GPU_GATE.read_text(encoding="utf-8", errors="replace") if GPU_GATE.exists() else ""
        if GPU_GATE_LINE in text:
            if waited:
                print(f"[gate] open after {waited:.0f}s", flush=True)
            return
        if waited == 0:
            print(f"[gate] waiting for '{GPU_GATE_LINE}' in {GPU_GATE}", flush=True)
        time.sleep(poll_seconds)
        waited += poll_seconds


def add_session_args(ap) -> None:
    ap.add_argument("--opt-level", choices=sorted(OPT_LEVELS), default="all")
    ap.add_argument("--no-mem-pattern", action="store_true", help="enable_mem_pattern=False")
    ap.add_argument("--cudnn-conv-algo", choices=["EXHAUSTIVE", "HEURISTIC", "DEFAULT"], default="EXHAUSTIVE")
    ap.add_argument("--log-severity", type=int, default=2, help="0 = VERBOSE (prints node placements to stderr)")
    ap.add_argument("--profile", action="store_true", help="enable ORT profiling, summarize per op type")
    ap.add_argument("--optimized-out", type=Path, help="dump the optimized graph ORT actually runs")
    ap.add_argument("--cuda-graph", action="store_true", help="CUDA EP enable_cuda_graph (needs IO binding + static shapes)")
    ap.add_argument("--tf32", type=int, choices=[0, 1], default=1, help="CUDA EP use_tf32 (1 = ORT default)")


def make_session(onnx_path: Path, args, provider: str = CUDA) -> ort.InferenceSession:
    so = ort.SessionOptions()
    so.graph_optimization_level = OPT_LEVELS[args.opt_level]
    so.enable_mem_pattern = not args.no_mem_pattern
    so.log_severity_level = args.log_severity
    if args.log_severity == 0:
        ort.set_default_logger_severity(0)
    if args.profile:
        so.enable_profiling = True
        so.profile_file_prefix = str(Path(__file__).resolve().parent / "speed_profile")
    if args.optimized_out:
        so.optimized_model_filepath = str(args.optimized_out)
    if provider == CUDA:
        wait_for_gpu()
        cuda_opts = {
            "device_id": 0,
            "gpu_mem_limit": GPU_MEM_LIMIT,
            "arena_extend_strategy": "kNextPowerOfTwo",
            "cudnn_conv_algo_search": args.cudnn_conv_algo,
            "use_tf32": args.tf32,
        }
        if args.cuda_graph:
            cuda_opts["enable_cuda_graph"] = 1
        providers = [(CUDA, cuda_opts), "CPUExecutionProvider"]
    else:
        providers = [provider]
    session = ort.InferenceSession(str(onnx_path), so, providers=providers)
    if provider not in session.get_providers():
        raise RuntimeError(f"{provider} is not active; session runs on {session.get_providers()}")
    return session


def profile_summary(session: ort.InferenceSession, top: int = 15) -> dict:
    """End profiling and aggregate kernel durations by op type (microseconds -> ms)."""
    path = session.end_profiling()
    events = json.loads(Path(path).read_text(encoding="utf-8"))
    by_op = collections.Counter()
    by_provider = collections.Counter()
    by_node = collections.Counter()
    counts = collections.Counter()
    runs = 0
    for ev in events:
        if ev.get("cat") == "Session" and ev.get("name") == "model_run":
            runs += 1
        if ev.get("cat") != "Node" or not ev.get("name", "").endswith("_kernel_time"):
            continue
        a = ev.get("args", {})
        op, prov = a.get("op_name", "?"), a.get("provider", "?")
        by_op[f"{op}@{prov[:4]}"] += ev["dur"]
        by_provider[prov] += ev["dur"]
        by_node[f"{op}:{ev['name'][:-12]}"] += ev["dur"]
        counts[f"{op}@{prov[:4]}"] += 1
    runs = max(runs, 1)
    return {
        "profile": path,
        "model_runs": runs,
        "kernel_ms_per_run": round(sum(by_provider.values()) / 1000 / runs, 1),
        "by_provider_ms_per_run": {k: round(v / 1000 / runs, 1) for k, v in by_provider.items()},
        "by_op_ms_per_run": {k: (round(v / 1000 / runs, 1), counts[k] // runs) for k, v in by_op.most_common(top)},
        "top_nodes_ms_per_run": {k: round(v / 1000 / runs, 1) for k, v in by_node.most_common(10)},
    }


def sync_cuda_ortvalue(shape, dtype=np.float32) -> ort.OrtValue:
    return ort.OrtValue.ortvalue_from_shape_and_type(list(shape), dtype, "cuda", 0)


def log(msg: str) -> None:
    print(msg, flush=True)
    sys.stdout.flush()
