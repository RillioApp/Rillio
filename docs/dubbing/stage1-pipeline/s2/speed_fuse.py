"""S2 speed, knob 6: does ORT's transformers optimizer fuse anything in this graph? (CPU-only step)

onnxruntime.transformers.optimizer.optimize_model runs the attention / gelu /
layernorm / skip-layernorm fusions for a given model_type and reports what it
fused. The dynamo-exported SDPA is a MatMul -> Mul -> Softmax -> MatMul chain
with rotary embeddings in between the projections; the BERT patterns may not
match. This script tries and prints the fusion census, and saves the result.

Run: E:\\venvs\\dubbing\\Scripts\\python.exe speed_fuse.py [--model-type bert] [--out <path>]
"""
import argparse
import collections
import sys
import time
from pathlib import Path

from onnxruntime.transformers import optimizer
from onnxruntime.transformers.fusion_options import FusionOptions

sys.path.insert(0, str(Path(__file__).resolve().parent))
from export_bs_roformer import ONNX_OUT  # noqa: E402


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--onnx", type=Path, default=ONNX_OUT)
    ap.add_argument("--out", type=Path)
    ap.add_argument("--model-type", default="bert")
    ap.add_argument("--num-heads", type=int, default=8)
    ap.add_argument("--hidden", type=int, default=512)
    ap.add_argument("--fp16", action="store_true", help="also convert to fp16 through the optimizer (keep_io_types=True)")
    args = ap.parse_args()
    out = args.out or args.onnx.with_name(args.onnx.stem + f"_fused_{args.model_type}" + ("_fp16" if args.fp16 else "") + ".onnx")

    t0 = time.perf_counter()
    options = FusionOptions(args.model_type)
    options.use_multi_head_attention = True
    m = optimizer.optimize_model(str(args.onnx), model_type=args.model_type, num_heads=args.num_heads, hidden_size=args.hidden,
                                 optimization_options=options, opt_level=0, use_gpu=True)
    census = m.get_fused_operator_statistics()
    print("fused:", {k: v for k, v in census.items() if v})
    if args.fp16:
        m.convert_float_to_float16(keep_io_types=True)
    m.save_model_to_file(str(out), use_external_data_format=False)
    ops = collections.Counter(n.op_type for n in m.model.graph.node)
    print({"out": str(out), "bytes": out.stat().st_size, "nodes": len(m.model.graph.node), "seconds": round(time.perf_counter() - t0, 1)})
    print("ops:", dict(ops.most_common(30)))


if __name__ == "__main__":
    main()
