"""Test fixtures for `src/instrument.rs` (M3, the app's section of
dub-engine/docs/dub-model/m3-instrument-tokens.md):

  - `src/instrument_fixture.json`: the encoder's log-mel (`instrument_encoder
    .features`: librosa melspectrogram at 16 kHz, n_fft 1280, hop 320, 64
    mels, its defaults otherwise, then log(mel + 1e-6)) of a 1 s 440 Hz tone,
    first FRAMES_KEPT frames, the parity truth the Rust mel is checked against;
  - `scripts/fixtures/instrument_placeholder.onnx`: a stand-in for the trained
    `instrument.onnx` with the contract's names and shapes (input `mel`
    [1, 64, T] with T dynamic, output `patches` [K, 256]) and random weights,
    so the app's loading and shape handling are tested before the real file
    exists. The real file replaces it without a code change.

Run with the dubbing venv (has librosa 1.0 and onnx):
  E:\\venvs\\dubbing\\Scripts\\python.exe scripts\\instrument_fixture.py
"""
import json
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
FIXTURE_JSON = HERE.parent / "src" / "instrument_fixture.json"
PLACEHOLDER_ONNX = HERE / "fixtures" / "instrument_placeholder.onnx"

# instrument_encoder.py: PROSODY_RATE, HOP_MS, N_MELS; n_fft = 4 * hop.
RATE = 16_000
HOP = RATE * 20 // 1000
N_FFT = 4 * HOP
N_MELS = 64
LOG_FLOOR = 1e-6
# m3-instrument-tokens.md: K pseudo-patches of feat_dim x patch_size floats.
K = 4
PATCH_ELEMENTS = 256
INSTRUMENT_DIM = 32

TONE_HZ = 440.0
TONE_S = 1.0
TONE_AMPLITUDE = 0.5
FRAMES_KEPT = 3
SEED = 20260919


def tone():
    n = np.arange(int(TONE_S * RATE))
    return (TONE_AMPLITUDE * np.sin(2 * np.pi * TONE_HZ * n / RATE)).astype(np.float32)


def log_mel(x):
    import librosa

    mel = librosa.feature.melspectrogram(y=x, sr=RATE, n_fft=N_FFT, hop_length=HOP, n_mels=N_MELS)
    return np.log(mel + LOG_FLOOR).astype(np.float32)


def write_fixture():
    lm = log_mel(tone())  # (N_MELS, T)
    assert lm.shape[0] == N_MELS
    fixture = {
        "rate": RATE,
        "tone_hz": TONE_HZ,
        "seconds": TONE_S,
        "amplitude": TONE_AMPLITUDE,
        "n_fft": N_FFT,
        "hop": HOP,
        "n_mels": N_MELS,
        "n_frames": int(lm.shape[1]),
        "frames_kept": FRAMES_KEPT,
        # frames x bands, the Rust mel's layout
        "log_mel": [[float(v) for v in lm[:, t]] for t in range(FRAMES_KEPT)],
    }
    FIXTURE_JSON.write_text(json.dumps(fixture, indent=1) + "\n", encoding="utf-8")
    print(f"wrote {FIXTURE_JSON}: {lm.shape[1]} frames, {FRAMES_KEPT} kept")


def write_placeholder():
    import onnx
    from onnx import TensorProto, helper, numpy_helper

    rng = np.random.default_rng(SEED)
    w1 = numpy_helper.from_array((rng.standard_normal((N_MELS, INSTRUMENT_DIM)) * 0.1).astype(np.float32), "w1")
    w2 = numpy_helper.from_array((rng.standard_normal((INSTRUMENT_DIM, K * PATCH_ELEMENTS)) * 0.1).astype(np.float32), "w2")
    shape = numpy_helper.from_array(np.array([K, PATCH_ELEMENTS], dtype=np.int64), "patches_shape")
    nodes = [
        helper.make_node("ReduceMean", ["mel"], ["pooled"], axes=[2], keepdims=0),
        helper.make_node("MatMul", ["pooled", "w1"], ["z0"]),
        helper.make_node("Relu", ["z0"], ["z"]),
        helper.make_node("MatMul", ["z", "w2"], ["flat"]),
        helper.make_node("Reshape", ["flat", "patches_shape"], ["patches"]),
    ]
    graph = helper.make_graph(
        nodes,
        "instrument_placeholder",
        [helper.make_tensor_value_info("mel", TensorProto.FLOAT, [1, N_MELS, "T"])],
        [helper.make_tensor_value_info("patches", TensorProto.FLOAT, [K, PATCH_ELEMENTS])],
        initializer=[w1, w2, shape],
    )
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)], producer_name="rillio instrument_fixture.py")
    model.ir_version = 8
    onnx.checker.check_model(model)
    PLACEHOLDER_ONNX.parent.mkdir(parents=True, exist_ok=True)
    onnx.save(model, str(PLACEHOLDER_ONNX))
    print(f"wrote {PLACEHOLDER_ONNX}: {PLACEHOLDER_ONNX.stat().st_size} bytes")


if __name__ == "__main__":
    write_fixture()
    write_placeholder()
