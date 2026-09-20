"""S2 speed, knob 5: fp16 conversion of the exported graph (CPU-only step).

Uses onnxruntime.transformers.float16.convert_float_to_float16, ORT's copy of
onnxconverter-common's converter (that package is not in the venv). Default
keep_io_types=True: the interface stays float32 (spec in, masked_spec out),
Casts are inserted at the boundary. --block-ops keeps named op types in fp32
(the converter wraps them in Casts): ReduceL2 is the RMSNorm sum of squares,
the one place fp16 can overflow to inf.

Run: E:\\venvs\\dubbing\\Scripts\\python.exe speed_fp16.py [--out <path>] [--block-ops ReduceL2 ...]
"""
import argparse
import sys
import time
from pathlib import Path

import onnx
from onnxruntime.transformers import float16

sys.path.insert(0, str(Path(__file__).resolve().parent))
from export_bs_roformer import ONNX_OUT  # noqa: E402
from speed_mha import toposort  # noqa: E402


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--onnx", type=Path, default=ONNX_OUT)
    ap.add_argument("--out", type=Path)
    ap.add_argument("--block-ops", nargs="*", default=[], help="op types kept in fp32 (added to the converter's default block list)")
    ap.add_argument("--io-fp16", action="store_true", help="keep_io_types=False: the interface becomes float16 too")
    args = ap.parse_args()
    out = args.out or args.onnx.with_name(args.onnx.stem + ("_fp16io" if args.io_fp16 else "_fp16") + ("_block" if args.block_ops else "") + ".onnx")

    t0 = time.perf_counter()
    model = onnx.load(str(args.onnx))
    block = list(float16.DEFAULT_OP_BLOCK_LIST) + list(args.block_ops)
    model16 = float16.convert_float_to_float16(model, keep_io_types=not args.io_fp16, op_block_list=block, disable_shape_infer=True)
    # The converter appends its graph-input Cast after nodes that consume it (Shape(spec) is node 0): re-sort.
    toposort(model16.graph)
    onnx.save(model16, str(out))
    onnx.checker.check_model(str(out), full_check=False)
    casts = sum(1 for n in model16.graph.node if n.op_type == "Cast")
    print({"out": str(out), "bytes": out.stat().st_size, "cast_nodes": casts, "blocked": args.block_ops, "seconds": round(time.perf_counter() - t0, 1)})


if __name__ == "__main__":
    main()
