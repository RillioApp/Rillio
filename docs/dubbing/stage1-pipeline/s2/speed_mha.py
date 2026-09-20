"""S2 speed, knob 6: fuse the 24 exported SDPA chains into com.microsoft.MultiHeadAttention.

Why: at 801 frames the unfused time attention materializes (62 bands x 8
heads x 801 x 801) fp32 score matrices, 1.27 GB each. Under an 8 GB arena
they do not fit at all; uncapped, ORT re-extends the arena by gigabytes every
run, which is the whole 1.6 RTF. MultiHeadAttention never materializes them
(flash / memory-efficient kernels on CUDA), as PyTorch's SDPA does not.

The exported chain per block (dynamo export, opset 18):
  q4 = rotary(q) (b, h, s, d)      k4 = rotary(k) (b, h, s, d)      v4 (b, h, s, d)
  kT = Reshape(Transpose(Reshape(k4), [0, 2, 1]))   (b, h, d, s)
  scores = MatMul(q4 * c, kT * c), c = sqrt(1 / sqrt(d))
  attn = MatMul(Softmax(scores, -1), v4)            (b, h, s, d)   name kept
Replacement:
  Q3 = Reshape(Transpose(q4, [0, 2, 1, 3]), [0, 0, -1])  (b, s, h*d), same for K3, V3
  out3 = MultiHeadAttention(Q3, K3, V3, num_heads=h, scale=1/sqrt(d))  (b, s, h*d)
  attn = Transpose(Reshape(out3, [0, 0, h, d]), [0, 2, 1, 3])
Every consumer of `attn` (the per-head sigmoid gate, the out projection) is untouched;
the orphaned chain is removed by dead-code elimination.

Validation: the rewritten graph is run on the CPU provider (ORT has a CPU
MultiHeadAttention kernel) against the original at --check-frames frames and
must agree to --max-abs-diff.

Run: E:\\venvs\\dubbing\\Scripts\\python.exe speed_mha.py [--onnx <path>] [--out <path>] [--fp16]
"""
import argparse
import json
import sys
import time
from pathlib import Path

import numpy as np
import onnx
import onnxruntime as ort
from onnx import TensorProto, helper, numpy_helper

sys.path.insert(0, str(Path(__file__).resolve().parent))
from export_bs_roformer import ONNX_OUT, YAML, load_config, sample_spec  # noqa: E402

MSFT = "com.microsoft"


def const_scalar(graph, initializers: dict, name: str) -> float:
    if name not in initializers:
        raise RuntimeError(f"{name} is not an initializer")
    return float(numpy_helper.to_array(initializers[name]).reshape(-1)[0])


def rewrite(model: onnx.ModelProto, heads: int, head_dim: int) -> int:
    graph = model.graph
    nodes = list(graph.node)
    initializers = {i.name: i for i in graph.initializer}
    producer = {o: n for n in nodes for o in n.output}
    consumers = {}
    for n in nodes:
        for i in n.input:
            consumers.setdefault(i, []).append(n)

    def only_consumer(name: str, op: str):
        cs = consumers.get(name, [])
        if len(cs) != 1 or cs[0].op_type != op:
            raise RuntimeError(f"{name}: expected a single {op} consumer, got {[c.op_type for c in cs]}")
        return cs[0]

    def prod(name: str, op: str):
        n = producer.get(name)
        if n is None or n.op_type != op:
            raise RuntimeError(f"{name}: expected producer {op}, got {None if n is None else n.op_type}")
        return n

    expected_scale = 1.0 / np.sqrt(head_dim)
    new_nodes = []
    shape_hd = helper.make_tensor("mha_shape_bs_hd", TensorProto.INT64, [3], [0, 0, -1])
    shape_bshd = helper.make_tensor("mha_shape_bshd", TensorProto.INT64, [4], [0, 0, heads, head_dim])
    graph.initializer.extend([shape_hd, shape_bshd])
    fused = 0
    for softmax in [n for n in nodes if n.op_type == "Softmax"]:
        scores = prod(softmax.input[0], "MatMul")
        q_scaled, kt_scaled = prod(scores.input[0], "Mul"), prod(scores.input[1], "Mul")
        c_q, c_k = const_scalar(graph, initializers, q_scaled.input[1]), const_scalar(graph, initializers, kt_scaled.input[1])
        if not np.isclose(c_q * c_k, expected_scale, rtol=1e-5):
            raise RuntimeError(f"{softmax.name}: scale {c_q} * {c_k} != 1/sqrt({head_dim})")
        q4 = q_scaled.input[0]
        kt = prod(kt_scaled.input[0], "Reshape")
        kt_t = prod(kt.input[0], "Transpose")
        if list(helper.get_attribute_value(kt_t.attribute[0])) != [0, 2, 1]:
            raise RuntimeError(f"{softmax.name}: K transpose perm is not [0, 2, 1]")
        k4 = prod(kt_t.input[0], "Reshape").input[0]
        av = only_consumer(softmax.output[0], "MatMul")
        if av.input[0] != softmax.output[0]:
            raise RuntimeError(f"{softmax.name}: AV MatMul does not take the softmax as its left input")
        v4, attn_out = av.input[1], av.output[0]
        tag = f"mha{fused}"

        def to_bs_hd(src: str, what: str) -> str:
            t = f"{tag}_{what}_t"
            new_nodes.append(helper.make_node("Transpose", [src], [t], perm=[0, 2, 1, 3], name=f"{tag}_{what}_transpose"))
            r = f"{tag}_{what}_3d"
            new_nodes.append(helper.make_node("Reshape", [t, shape_hd.name], [r], name=f"{tag}_{what}_reshape"))
            return r

        q3, k3, v3 = to_bs_hd(q4, "q"), to_bs_hd(k4, "k"), to_bs_hd(v4, "v")
        out3 = f"{tag}_out3"
        new_nodes.append(helper.make_node("MultiHeadAttention", [q3, k3, v3], [out3], domain=MSFT, num_heads=heads, scale=float(expected_scale), name=f"{tag}_attention"))
        out4 = f"{tag}_out4"
        new_nodes.append(helper.make_node("Reshape", [out3, shape_bshd.name], [out4], name=f"{tag}_out_reshape"))
        new_nodes.append(helper.make_node("Transpose", [out4], [attn_out], perm=[0, 2, 1, 3], name=f"{tag}_out_transpose"))
        # The old AV MatMul produced attn_out; drop it so the name has one producer, then DCE removes its chain.
        av.output[0] = f"{tag}_dead_attn"
        fused += 1

    # Insert each new group right before the first consumer of attn_out: simplest is to append the new
    # nodes at the position of the old AV MatMul, then do a topological sort + DCE.
    graph.node.extend(new_nodes)
    dce(graph)
    toposort(graph)
    if not any(imp.domain == MSFT for imp in model.opset_import):
        model.opset_import.append(helper.make_opsetid(MSFT, 1))
    return fused


def dce(graph: onnx.GraphProto) -> int:
    outputs = {o.name for o in graph.output}
    removed = 0
    while True:
        used = set(outputs)
        for n in graph.node:
            used.update(n.input)
        keep = [n for n in graph.node if any(o in used for o in n.output)]
        if len(keep) == len(graph.node):
            return removed
        removed += len(graph.node) - len(keep)
        del graph.node[:]
        graph.node.extend(keep)


def toposort(graph: onnx.GraphProto) -> None:
    available = {i.name for i in graph.initializer} | {i.name for i in graph.input} | {""}
    pending = list(graph.node)
    ordered = []
    while pending:
        progressed = False
        rest = []
        for n in pending:
            if all(i in available for i in n.input):
                ordered.append(n)
                available.update(n.output)
                progressed = True
            else:
                rest.append(n)
        pending = rest
        if not progressed:
            raise RuntimeError(f"toposort stuck: {[n.name for n in pending[:5]]}")
    del graph.node[:]
    graph.node.extend(ordered)


def cpu_check(original: Path, rewritten: Path, frames: int, freqs: int, channels: int, max_abs_diff: float) -> dict:
    spec = sample_spec(frames, freqs, channels, seed=3).numpy()
    so = ort.SessionOptions()
    ref = ort.InferenceSession(str(original), so, providers=["CPUExecutionProvider"]).run(None, {"spec": spec})[0]
    t0 = time.perf_counter()
    got = ort.InferenceSession(str(rewritten), so, providers=["CPUExecutionProvider"]).run(None, {"spec": spec})[0]
    diff = float(np.max(np.abs(got - ref)))
    if got.shape != ref.shape:
        raise RuntimeError(f"shape mismatch {got.shape} vs {ref.shape}")
    if not np.isfinite(got).all():
        raise RuntimeError("rewritten graph produced non-finite values")
    if diff > max_abs_diff:
        raise RuntimeError(f"rewritten graph differs from the original by {diff} > {max_abs_diff} at {frames} frames")
    return {"frames": frames, "max_abs_diff": diff, "ref_peak": float(np.max(np.abs(ref))), "cpu_seconds_rewritten": round(time.perf_counter() - t0, 1)}


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--onnx", type=Path, default=ONNX_OUT)
    ap.add_argument("--out", type=Path)
    ap.add_argument("--check-frames", type=int, default=101)
    ap.add_argument("--max-abs-diff", type=float, default=1e-3)
    ap.add_argument("--skip-check", action="store_true")
    args = ap.parse_args()
    out = args.out or args.onnx.with_name(args.onnx.stem + "_mha.onnx")

    config = load_config(YAML)
    heads, head_dim = config["model"]["heads"], config["model"]["dim_head"]
    t0 = time.perf_counter()
    model = onnx.load(str(args.onnx))
    before = len(model.graph.node)
    fused = rewrite(model, heads, head_dim)
    if fused != 2 * config["model"]["depth"]:
        raise RuntimeError(f"fused {fused} attention blocks, expected {2 * config['model']['depth']}")
    onnx.save(model, str(out))
    report = {"out": str(out), "fused": fused, "nodes_before": before, "nodes_after": len(model.graph.node), "bytes": out.stat().st_size, "seconds": round(time.perf_counter() - t0, 1)}
    if not args.skip_check:
        report["cpu_check"] = cpu_check(args.onnx, out, args.check_frames, config["model"]["dim_freqs_in"], 2, args.max_abs_diff)
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
