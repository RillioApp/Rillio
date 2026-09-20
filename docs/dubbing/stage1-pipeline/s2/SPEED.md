---
id: dubbing-stage1-s2-speed
tags: [dubbing, ai, audio, onnx, separation, performance]
related_files: [docs/dubbing/stage1-pipeline/s2/README.md, docs/dubbing/stage1-pipeline/s2/speed_mha.py, docs/dubbing/stage1-pipeline/s2/speed_fp16.py, docs/dubbing/stage1-pipeline/s2/speed_parity.py]
status: done
last_sync: 2026-09-14
---

# S2 speed: BS-RoFormer ep368 ONNX on the CUDA execution provider

Verdict (2026-09-14): **PASS.** `E:\models\separator\bs_roformer_ep368_mha_fp16.onnx`
(the exported graph with its 24 attention chains fused into
`com.microsoft.MultiHeadAttention`, then converted to fp16 with float32 IO) runs
the F5 fixture at **wall RTF 0.123** (graph alone 0.083) on the CUDA EP with the
arena capped at 8 GB, scoring **14.892 / 14.942 dB** SI-SDR (vocals / residual),
+0.004 / +0.005 dB against the PyTorch 14.888 / 14.937. Same fixture handling,
chunking (352,800 samples, overlap 2, 14 chunks), overlap-add and scorer as
`parity.py`. RTX 3090 Ti, onnxruntime-gpu 1.22, CUDA 12.

## Root cause: memory, not compute

The unfused export was never slow at computing; it was slow at allocating. At
801 frames the time transformer's attention scores are one tensor of
(62 bands x 8 heads x 801 x 801) fp32 = **1,272,936,384 bytes**, 12 times per
chunk, and the dynamo export keeps two or three of them live (QK^T, softmax
out). Evidence:

- Under the 8 GB cap the original graph fails on its first attention
  `FusedMatMul` (`Available memory of 1072693248 is smaller than requested
  bytes of 1272936448`, the score tensor byte for byte).
- Uncapped it runs, but the BFC arena extends by gigabytes every run (the
  earlier 24 GB grab), and that thrash is the RTF 1.646.
- At 401 frames (score tensors 4x smaller, fit the cap) the SAME graph does a
  4 s chunk in 347 ms: graph RTF 0.089, already faster than PyTorch's 0.18.

So the construction that fixes it is the one PyTorch already had through
SDPA: attention that never materializes the score matrix (knob 6), and fp16
(knob 5) which halves every tensor. Both were measured separately and
together.

## Every construction tried

RTF = wall seconds of the whole demix over the 60 s of audio, as `parity.py`
computes it (numpy STFT + session + numpy iSTFT + overlap-add). Graph RTF = the
session calls alone. SI-SDR from `speed_parity.py` (imports `parity.py`'s
`demix`/`Stft`/`score_arm`, G2's `si_sdr`). Bench ms = `speed_bench.py`, one
801-frame chunk, median of 4-6 IO-bound synchronous runs. All CUDA sessions:
`gpu_mem_limit` 8 GiB, `arena_extend_strategy` kNextPowerOfTwo, created only
after `E:\datasets\g1\vulkan-timing.log` carried `[vulkan] done`, never two
at once.

| # | construction | RTF wall | RTF graph | bench ms / chunk | vocals dB | residual dB | notes |
|---|---|---|---|---|---|---|---|
| 0 | original fp32, uncapped arena (prior record, `timing_CUDAExecutionProvider.json`) | 1.646 | - | - | 14.888 | 14.937 | arena grew to the whole card |
| 1 | original fp32, 8 GB cap | fail | - | OOM | - | - | first attention FusedMatMul asks 1.27 GB with 1.0 GB left |
| 1b | original fp32, 8 GB cap, 401 frames (diagnostic only) | - | 0.089 | 347 | - | - | proves the graph is not compute-bound; profile: MatMul 109 ms, Add 53, Mul 38 of 344 |
| 2 | session options on #5 (all / extended / basic; mem pattern off; cudnn HEURISTIC) | - | - | 354 / 354 / 358 / 353 / 356 | - | - | all within noise; no Conv in the graph so cudnn_conv_algo_search is moot |
| 3 | #5 + IO binding (device input OrtValue, device output) | 0.122 | 0.083 | - | 14.892 | 14.942 | host copies are ~20 ms per chunk (plain 377 ms vs bound 354 ms) |
| 3b | #5 + IO binding + batch 2 | 0.118 | 0.081 | - | 14.892 | 14.942 | best number; the graph is already saturating the GPU at batch 1 |
| 3c | #5 + IO binding + batch 7 | fail | - | OOM | - | - | ReduceL2 allocation exceeds the cap; batch 2 is the ceiling under 8 GB |
| 4 | #5 with batch/frames pinned to 1/801 (`speed_static.py`) | - | - | 351 | - | - | vs 354 dynamic: noise; the dynamic-shape glue costs 0.5 ms per run |
| 5a | original graph converted to fp16 (`speed_fp16.py`, keep_io_types) | 0.129 | 0.091 | 387 | 14.892 | 14.942 | fp16 alone halves the score tensors to 636 MB and fits the cap; passes |
| 5b | #6 converted to fp16, ReduceL2 kept fp32 | - | - | 351 | - | - | identical output to #5 (max abs diff vs CPU ref 0.389 on a 57.46 peak in both); no overflow in RMSNorm, so not needed |
| 6 | MHA rewrite, fp32 (`speed_mha.py`) | 0.249 | 0.170 | 717 | 14.888 | 14.937 | exact (0.000 dB); cutlass EFFICIENT_ATTENTION on the 12 time blocks, MATH path (502 MB workspace) on the 12 freq blocks (seq 62 is below ORT's fp32 minimum of 256) |
| **5** | **#6 converted to fp16 (`bs_roformer_ep368_mha_fp16.onnx`)** | **0.123** | **0.083** | **354** | **14.892** | **14.942** | **winner; all 24 blocks on FLASH_ATTENTION** |
| 6b | `onnxruntime.transformers.optimizer` (bert, MHA enabled) on the original | - | - | - | - | - | fused only BiasGelu x24, no attention pattern matched the dynamo export; superseded by #6 |

Records: `timing_mha_fp32.json`, `timing_mha_fp16.json`, `timing_mha_fp16_iob.json`,
`timing_mha_fp16_b2.json`, `timing_unfused_fp16.json` next to this file.

Honest reading of the winner: 2.1 s of the 7.4 s wall is `parity.py`'s numpy
STFT/iSTFT (a Python loop over 801 frames in `Stft.inverse`); the ONNX core
itself is 5.0 s for 14 chunks (357 ms per 8 s chunk). The shell's Rust STFT
will set its own overhead; the number the seam asked for, the ONNX path on the
CUDA EP, is 0.083 to 0.123 depending on how much of the host work is counted,
both under 0.2.

## Placement findings (knob 1)

ORT 1.22 wrote no `[V:]` placement lines to stderr at `log_severity_level=0`
(only `[I:]`/`[W:]`), so placement was read from the profiler, whose kernel
events carry the provider. In the CUDA session of the original graph (401
frames, 9 runs) the CPU provider ran, per run: Concat x72, Slice x72,
Squeeze x2, Mul x2, Reshape x2, Cast x1, total **3.5 ms** of 344 ms. All are
int64 shape arithmetic (the `sym_size` scalars, the Concat-built reshape
targets, the rotary `Range` length), nothing that carries activations. In the
MHA graph the CPU share is 0.5 ms. There is no fallback to fix.

The 401-frame per-op profile of the original graph (ms per run): MatMul 109
(330 nodes), Add 53, Mul 38, Split 26, Tanh 18, Sigmoid 17, Div 14, ReduceL2
14 (111 nodes, the `F.normalize` RMSNorm), Transpose 11, Softmax 4. ORT's own
optimizer fuses only the attention scale into `FusedMatMul` (x24); GELU
(`Erf`) and RMSNorm stay unfused. `speed_census.py` prints the static op
census (3265 nodes, one tiny `Einsum` for the rotary angles, one `Cast`).

## The MHA rewrite (`speed_mha.py`)

Per exported block: `q4`, `k4` are the post-rotary (b, h, s, d) tensors, `v4`
the third slice of the packed projection; K goes through Reshape ->
Transpose[0,2,1] -> Reshape; both sides are pre-multiplied by
`sqrt(1/sqrt(d))`; then `MatMul -> Softmax(-1) -> MatMul(., v4)` yields
`scaled_dot_product_attention` in (b, h, s, d), consumed by the per-head
sigmoid gate. Replacement, layout-preserving:

```
Q3 = Reshape(Transpose(q4, [0,2,1,3]), [0,0,-1])      (b, s, h*d), same for K3, V3
out3 = MultiHeadAttention(Q3, K3, V3, num_heads=8, scale=0.125)   com.microsoft
attn = Transpose(Reshape(out3, [0,0,8,64]), [0,2,1,3])              (b, h, s, d)
```

The old chain is dead-code eliminated and the graph re-sorted; the script
refuses any block whose scale product is not 1/sqrt(64) or whose K transpose
is not [0,2,1]. Validated on the CPU provider (ORT has a CPU MHA kernel)
against the original at 101 frames: max abs diff 1.3e-7 on a 0.27 peak; on
CUDA at 801 frames against the original graph on CPU: 0.0115 on a 57.46 peak
(TF32 matmuls, `use_tf32=1` default), and the fixture score is bit-identical
to three decimals.

A re-export with `torch.onnx.export(..., custom_translation_table={torch.ops.aten.scaled_dot_product_attention.default: <onnxscript fn emitting com.microsoft.MultiHeadAttention>})`
would give the same graph from the source side; the surgery was chosen
because it operates on the already parity-verified file and validates in
seconds on CPU.

## Landmine: `OrtValue.update_inplace` into a CUDA buffer

`ortvalue_from_shape_and_type(shape, np.float32, "cuda", 0)` followed by
`update_inplace(spec)` fed the session a wrong input at 801 frames (output
max 6.8 instead of 57.5, deterministic), and reading the buffer back after
`update_inplace` returned data that differs from `spec` by the input's own
peak. The same round trip on the CPU device is exact at 6.6, 13 and 26 MB,
and at 401 frames on CUDA it had worked, so it is CUDA-specific and size or
state dependent. `ortvalue_from_numpy(spec, "cuda", 0)` per call and
`bind_cpu_input` both match the plain `session.run` to 0.0
(`speed_bench.py --probe-binding`). Both harnesses use a fresh device
OrtValue per call; a shell-side implementation must not rely on
`update_inplace`. This also rules out `enable_cuda_graph` from Python
(it needs a fixed input address refreshed in place); it was not tried.

## Winning configuration, exact reproduction

```powershell
$py = "E:\venvs\dubbing\Scripts\python.exe"
$s2 = "docs\dubbing\stage1-pipeline\s2"
$M  = "E:\models\separator"
& $py "$s2\speed_mha.py"   --onnx $M\bs_roformer_ep368.onnx        # -> bs_roformer_ep368_mha.onnx, CPU-validated (3 s + 3 s)
& $py "$s2\speed_fp16.py"  --onnx $M\bs_roformer_ep368_mha.onnx    # -> bs_roformer_ep368_mha_fp16.onnx (5 s)
& $py "$s2\speed_parity.py" --g2-dir docs\dubbing\stage0-gates\g2 --onnx $M\bs_roformer_ep368_mha_fp16.onnx --tag mha_fp16 --batched-loop
```

Session, as `speed_common.make_session` builds it:

```python
import onnxruntime as ort
so = ort.SessionOptions()                       # defaults: ORT_ENABLE_ALL, mem pattern on
providers = [("CUDAExecutionProvider", {
    "device_id": 0,
    "gpu_mem_limit": 8 * 1024**3,
    "arena_extend_strategy": "kNextPowerOfTwo",
    "cudnn_conv_algo_search": "EXHAUSTIVE",     # irrelevant, no Conv in the graph
    "use_tf32": 1,
}), "CPUExecutionProvider"]
session = ort.InferenceSession(r"E:\models\separator\bs_roformer_ep368_mha_fp16.onnx", so, providers=providers)
(masked,) = session.run(["masked_spec"], {"spec": spec})   # spec float32 (1, 2, 1025, 801, 2)
```

Interface unchanged from the README: `spec` float32 (batch, 2, 1025, frames, 2)
in, `masked_spec` float32 same shape out, batch and frames dynamic, opset 18
plus `com.microsoft` v1 (MultiHeadAttention is a CUDA and CPU contrib op; the
DirectML EP does not implement it, so the DML path keeps the unfused fp16
file `bs_roformer_ep368_fp16.onnx`, #5a, which passes on CUDA at 0.129).

## Files

- `speed_common.py`: GPU gate, capped CUDA session, profile aggregation.
- `speed_census.py`: static op census of a graph (no session).
- `speed_bench.py`: one-chunk microbenchmark; `--profile`, `--frames`, `--batch`, `--probe-binding`, `--ref-cpu`, `--optimized-out`.
- `speed_parity.py`: `parity.py --onnx-only` with `--batch`, `--io-binding`, `--fp16-io`, `--tag`, the session knobs; writes `timing_<tag>.json`.
- `speed_mha.py`: the attention rewrite + CPU validation.
- `speed_fp16.py`: fp16 conversion through ORT's bundled converter (fixes its topological-order bug).
- `speed_static.py`: pins the dynamic axes in place (knob 4).
- `speed_fuse.py`: the transformers-optimizer trial (knob 6b).
- `speed_log.py`: normalizes ORT's mixed UTF-16/UTF-8 stderr logs and greps placement/arena/error lines.

Artifacts in `E:\models\separator\`: `bs_roformer_ep368_mha.onnx` (615 MB,
fp32 fused, exact), `bs_roformer_ep368_mha_fp16.onnx` (310 MB, the winner),
`bs_roformer_ep368_fp16.onnx` (310 MB, unfused fp16, #5a). Byproducts that can
be deleted: `_fused_bert.onnx`, `_mha_ortopt_cuda.onnx`, `_mha_fp16_block.onnx`,
`_mha_fp16_static_b1_t801.onnx`, `_mha_fp16_static_b2_t801.onnx`.
