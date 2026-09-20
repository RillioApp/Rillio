"""S2 speed, knob 4: pin the dynamic axes of an exported graph to fixed values (CPU-only step).

No re-export needed: the dynamic `batch` and `frames` dims of `spec` /
`masked_spec` are set to constants in the ONNX file. ORT's constant folding
then resolves every Shape / Range / rotary-angle node at session creation,
which is the whole benefit a re-export with fixed shapes would give (the
graph body is identical). A fixed-shape graph is also what enable_cuda_graph
needs.

Run: E:\\venvs\\dubbing\\Scripts\\python.exe speed_static.py --onnx <path> [--batch 1] [--frames 801]
"""
import argparse
import sys
import time
from pathlib import Path

import onnx

sys.path.insert(0, str(Path(__file__).resolve().parent))
from export_bs_roformer import ONNX_OUT  # noqa: E402


def pin(value: onnx.ValueInfoProto, batch: int, frames: int) -> None:
    dims = value.type.tensor_type.shape.dim
    for d, size in ((dims[0], batch), (dims[3], frames)):
        if not d.dim_param:
            raise RuntimeError(f"{value.name}: dim {d} is not symbolic")
        d.ClearField("dim_param")
        d.dim_value = size


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--onnx", type=Path, default=ONNX_OUT)
    ap.add_argument("--out", type=Path)
    ap.add_argument("--batch", type=int, default=1)
    ap.add_argument("--frames", type=int, default=801)
    args = ap.parse_args()
    out = args.out or args.onnx.with_name(args.onnx.stem + f"_static_b{args.batch}_t{args.frames}.onnx")
    t0 = time.perf_counter()
    model = onnx.load(str(args.onnx))
    for value in list(model.graph.input) + list(model.graph.output):
        pin(value, args.batch, args.frames)
    # Drop stale intermediate value_info so ORT infers shapes from the pinned inputs.
    del model.graph.value_info[:]
    onnx.save(model, str(out))
    print({"out": str(out), "batch": args.batch, "frames": args.frames, "bytes": out.stat().st_size, "seconds": round(time.perf_counter() - t0, 1)})


if __name__ == "__main__":
    main()
