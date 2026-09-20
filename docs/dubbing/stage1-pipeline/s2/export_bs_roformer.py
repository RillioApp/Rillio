"""S2: export the BS-RoFormer ep368 network core to ONNX.

The model is built by audio-separator's own RoformerLoader from the checkpoint
and its yaml (one owner of the architecture). The STFT and iSTFT stay OUTSIDE
the graph: the exported core takes the real-view spectrogram of a stereo
chunk and returns the masked spectrogram of the Vocals stem. The graph is the
body of BSRoformer.forward between torch.stft and torch.istft, with the
complex mask multiply written in real arithmetic (view_as_complex has no ONNX
lowering).

ONNX interface (float32):
  input  spec        (B, 2, 1025, T, 2)  view_as_real(stft(chunk)) per channel
  output masked_spec (B, 2, 1025, T, 2)  spec * mask, same layout
B and T are dynamic. opset 18 (the dynamo exporter's floor; its down-conversion
to 17 fails on ReduceL2's axes input and leaves the graph at 18 anyway).

Run: E:\\venvs\\dubbing\\Scripts\\python.exe export_bs_roformer.py
"""
import argparse
import json
import sys
import time
from pathlib import Path

import numpy as np
import onnx
import onnxruntime as ort
import torch
import yaml
from einops import rearrange
from rotary_embedding_torch import RotaryEmbedding
from torch import nn

from audio_separator.separator.roformer.roformer_loader import RoformerLoader

MODEL_DIR = Path(r"E:\models\separator")
CKPT = MODEL_DIR / "model_bs_roformer_ep_368_sdr_12.9628.ckpt"
YAML = MODEL_DIR / "model_bs_roformer_ep_368_sdr_12.9628.yaml"
ONNX_OUT = MODEL_DIR / "bs_roformer_ep368.onnx"
OPSET = 18
COMPLEX = 2
# Frames per chunk at the model's inference dim_t; the export sample and the
# dynamic-axis check use this and a shorter T.
EXPORT_FRAMES = 801
CHECK_FRAMES = 401
# fp32 PyTorch vs ONNX Runtime on the same input: the wrapper and the runtime
# must agree to float precision, anything larger is a graph bug.
SMOKE_MAX_ABS_DIFF = 1e-3


def load_config(path: Path) -> dict:
    # FullLoader: the yaml carries `!!python/tuple`, exactly as Separator.load_model_data_from_yaml reads it.
    with open(path, encoding="utf-8") as fh:
        return yaml.load(fh, Loader=yaml.FullLoader)


def load_model(ckpt: Path, config: dict) -> nn.Module:
    result = RoformerLoader().load_model(model_path=str(ckpt), config=config, device="cpu")
    if not result.success or result.model is None:
        raise RuntimeError(f"RoformerLoader failed: {result.error_message}")
    model = result.model.eval()
    if model.num_stems != 1 or model.audio_channels != 2:
        raise RuntimeError(f"expected a 1-stem stereo model, got num_stems={model.num_stems} channels={model.audio_channels}")
    for module in model.modules():
        if isinstance(module, RotaryEmbedding):
            # The rotary angle cache would be traced as a constant sized to the export T.
            module.cache_if_possible = False
            module.cached_freqs = None
    return model


class BSRoformerSpecCore(nn.Module):
    """BSRoformer.forward from after torch.stft to before torch.istft."""

    def __init__(self, model: nn.Module):
        super().__init__()
        self.model = model

    def forward(self, spec: torch.Tensor) -> torch.Tensor:
        m = self.model
        # b s f t c -> b (f s) t c: stereo interleaved into the frequency axis, frequency leading (as in the model).
        stft_repr = rearrange(spec, "b s f t c -> b (f s) t c")
        x = rearrange(stft_repr, "b f t c -> b t (f c)")
        x = m.band_split(x)

        for time_transformer, freq_transformer in m.layers:
            b, t, f, d = x.shape
            x = x.permute(0, 2, 1, 3).reshape(b * f, t, d)
            x = time_transformer(x)
            x = x.reshape(b, f, t, d).permute(0, 2, 1, 3).reshape(b * t, f, d)
            x = freq_transformer(x)
            x = x.reshape(b, t, f, d)

        x = m.final_norm(x)
        mask = m.mask_estimators[0](x)
        mask = rearrange(mask, "b t (f c) -> b f t c", c=COMPLEX)

        spec_re, spec_im = stft_repr[..., 0], stft_repr[..., 1]
        mask_re, mask_im = mask[..., 0], mask[..., 1]
        out = torch.stack((spec_re * mask_re - spec_im * mask_im, spec_re * mask_im + spec_im * mask_re), dim=-1)
        return rearrange(out, "b (f s) t c -> b s f t c", s=m.audio_channels)


def sample_spec(frames: int, freqs: int, channels: int, seed: int) -> torch.Tensor:
    generator = torch.Generator().manual_seed(seed)
    return torch.randn(1, channels, freqs, frames, COMPLEX, generator=generator) * 0.1


def export(core: nn.Module, spec: torch.Tensor, out: Path) -> None:
    from torch.export import Dim

    onnx_program = torch.onnx.export(
        core,
        (spec,),
        dynamo=True,
        opset_version=OPSET,
        input_names=["spec"],
        output_names=["masked_spec"],
        dynamic_shapes={"spec": {0: Dim("batch"), 3: Dim("frames")}},
        external_data=False,
        optimize=True,
    )
    onnx_program.save(str(out))


def check_graph(out: Path) -> dict:
    onnx.checker.check_model(str(out), full_check=True)
    model = onnx.load(str(out), load_external_data=False)
    opsets = {imp.domain or "ai.onnx": imp.version for imp in model.opset_import}
    ops = sorted({node.op_type for node in model.graph.node})
    shapes = {}
    for value in list(model.graph.input) + list(model.graph.output):
        dims = [d.dim_param or d.dim_value for d in value.type.tensor_type.shape.dim]
        shapes[value.name] = dims
    return {"opsets": opsets, "ops": ops, "io": shapes, "bytes": out.stat().st_size}


def smoke_test(core: nn.Module, out: Path, specs: list[torch.Tensor]) -> list[dict]:
    session = ort.InferenceSession(str(out), providers=["CPUExecutionProvider"])
    reports = []
    for spec in specs:
        with torch.no_grad():
            expected = core(spec).numpy()
        t0 = time.perf_counter()
        (got,) = session.run(None, {"spec": spec.numpy()})
        elapsed = time.perf_counter() - t0
        if got.shape != expected.shape:
            raise RuntimeError(f"shape mismatch at T={spec.shape[3]}: onnx {got.shape} vs torch {expected.shape}")
        diff = float(np.max(np.abs(got - expected)))
        if diff > SMOKE_MAX_ABS_DIFF:
            raise RuntimeError(f"ONNX vs PyTorch core max abs diff {diff} at T={spec.shape[3]} exceeds {SMOKE_MAX_ABS_DIFF}")
        reports.append({"frames": int(spec.shape[3]), "max_abs_diff": diff, "ort_seconds": round(elapsed, 2)})
    return reports


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=ONNX_OUT)
    ap.add_argument("--skip-export", action="store_true", help="only check + smoke-test an existing file")
    args = ap.parse_args()

    config = load_config(YAML)
    model = load_model(CKPT, config)
    core = BSRoformerSpecCore(model).eval()
    freqs = config["model"]["dim_freqs_in"]
    channels = model.audio_channels

    if not args.skip_export:
        t0 = time.perf_counter()
        export(core, sample_spec(EXPORT_FRAMES, freqs, channels, seed=0), args.out)
        print(json.dumps({"exported": str(args.out), "seconds": round(time.perf_counter() - t0, 1)}))

    graph = check_graph(args.out)
    smoke = smoke_test(core, args.out, [sample_spec(EXPORT_FRAMES, freqs, channels, seed=1), sample_spec(CHECK_FRAMES, freqs, channels, seed=2)])
    print(json.dumps({"graph": graph, "smoke": smoke}, indent=2))


if __name__ == "__main__":
    sys.exit(main())
