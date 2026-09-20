"""S2 speed, step 0: static census of the exported graph (no session, no GPU).

Counts nodes per op type, lists every Einsum equation and every Cast target,
and prints the op chain around each Softmax (is attention exported as an
unfused MatMul/Softmax chain?). Run before any GPU work: it tells which ops
to expect on the CUDA EP placement log.

Run: E:\\venvs\\dubbing\\Scripts\\python.exe speed_census.py [--onnx <path>]
"""
import argparse
import collections
import json
import sys
from pathlib import Path

import onnx
from onnx import TensorProto

sys.path.insert(0, str(Path(__file__).resolve().parent))
from export_bs_roformer import ONNX_OUT  # noqa: E402


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--onnx", type=Path, default=ONNX_OUT)
    args = ap.parse_args()

    model = onnx.load(str(args.onnx), load_external_data=False)
    graph = model.graph
    counts = collections.Counter(node.op_type for node in graph.node)
    print(json.dumps({"nodes": len(graph.node), "by_op": dict(sorted(counts.items(), key=lambda kv: -kv[1]))}, indent=2))

    einsums = collections.Counter()
    casts = collections.Counter()
    for node in graph.node:
        if node.op_type == "Einsum":
            einsums[next(a.s.decode() for a in node.attribute if a.name == "equation")] += 1
        elif node.op_type == "Cast":
            to = next(a.i for a in node.attribute if a.name == "to")
            casts[TensorProto.DataType.Name(to)] += 1
    print("einsum equations:", dict(einsums))
    print("cast targets:", dict(casts))

    producer = {out: node for node in graph.node for out in node.output}
    consumers = collections.defaultdict(list)
    for node in graph.node:
        for inp in node.input:
            consumers[inp].append(node)

    softmaxes = [n for n in graph.node if n.op_type == "Softmax"]
    print(f"softmax nodes: {len(softmaxes)}")
    if softmaxes:
        node = softmaxes[0]
        chain = []
        cur = node
        for _ in range(8):
            chain.append(cur.op_type)
            prev = producer.get(cur.input[0])
            if prev is None:
                break
            cur = prev
        chain.reverse()
        after = [c.op_type for c in consumers[node.output[0]]]
        print("first softmax: upstream chain", " -> ".join(chain), "| consumers", after)

    by_dtype = collections.Counter()
    for init in graph.initializer:
        n = 1
        for d in init.dims:
            n *= d
        by_dtype[TensorProto.DataType.Name(init.data_type)] += n
    print("initializer elements by dtype:", dict(by_dtype))
    dyn = [(v.name, [d.dim_param or d.dim_value for d in v.type.tensor_type.shape.dim]) for v in list(graph.input) + list(graph.output)]
    print("io:", dyn)


if __name__ == "__main__":
    main()
