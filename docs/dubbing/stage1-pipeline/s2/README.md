---
id: dubbing-stage1-s2
tags: [dubbing, ai, audio, onnx, separation]
related_files: [docs/dubbing/stage1-pipeline/README.md, docs/dubbing/stage0-gates/g2/score.py]
status: done
last_sync: 2026-09-14
---

# S2: BS-RoFormer ep368 outside Python (ONNX export + CPU parity)

Verdict (2026-09-14): **PASS.** The ONNX core on ONNX Runtime's CPU provider
reproduces the PyTorch separator on F5 to 0.000 dB on both stems (SI-SDR
14.888 dB vocals, 14.937 dB residual, both arms), max abs waveform diff
1.15e-5 on a 0.66 peak. These are G2's numbers (14.89 / 14.94, CUDA fp32,
`--mdxc_overlap=2`) reproduced on CPU. The RTF half of S2 (CUDA EP, <= 0.2)
is not measured here: another measurement owned the GPU, and CPU RTF (~7) is
not the number the seam asks for.

## What was exported

`E:\models\separator\bs_roformer_ep368.onnx` (645,372,138 bytes, weights
embedded, opset 18, `ai.onnx` domain only, checked with
`onnx.checker.check_model(full_check=True)`).

| tensor | shape | meaning |
|---|---|---|
| input `spec` | `(batch, 2, 1025, frames, 2)` float32 | `view_as_real(torch.stft(chunk))` per stereo channel: channel, frequency bin, frame, (re, im) |
| output `masked_spec` | `(batch, 2, 1025, frames, 2)` float32 | `spec * mask` for the single Vocals stem, same layout; iSTFT it to get the stem |

`batch` and `frames` are dynamic axes (verified: the same session ran 801
and 401 frames against the PyTorch wrapper, max abs diff 3.1e-6 and 3.6e-6).

The graph is the body of `BSRoformer.forward` between `torch.stft` and
`torch.istft`: band split, 12 x (time transformer, freq transformer), final
RMSNorm, mask estimator, and the complex mask multiply written in real
arithmetic. Ops in the graph: Add, Cast, Clip, Concat, Cos, Div, Einsum,
Erf, Expand, Gather, MatMul, Mul, Neg, Range, ReduceL2, Reshape, Shape,
Sigmoid, Sin, Slice, Softmax, Split, Squeeze, Sub, Tanh, Transpose,
Unsqueeze. Attention is SDPA decomposed to MatMul + Softmax (no fused
Attention op at opset 18); rotary angles are built in-graph from `Range`,
so any frame count works.

The model is constructed by audio-separator's own `RoformerLoader` from the
checkpoint + yaml (`E:\models\separator\model_bs_roformer_ep_368_sdr_12.9628.*`),
the same path `MDXCSeparator.load_model` takes; the export script only wraps
it. fp32 throughout (G2 also ran fp32: the log shows no native-fp16 line).

## What stayed outside the graph, and why

- `torch.stft` / `torch.istft` / `view_as_complex`: no ONNX lowering for
  complex tensors. Kept outside. `parity.py::Stft` is the numpy twin with
  the yaml's parameters (n_fft 2048, hop 441, win 2048, periodic hann,
  center + reflect pad, one-sided, not normalized); it asserts its window
  equals `torch.hann_window` and refuses an ill-conditioned envelope.
  A chunk of `hop * (dim_t - 1)` = 352,800 samples gives exactly 801 frames
  and the iSTFT returns exactly 352,800 samples, so the overlap-add is
  unchanged.
- The complex mask multiply moved INTO the graph as four real products, so
  the runtime side is only STFT in, iSTFT out.
- Chunking, hamming overlap-add and `mix - vocals` for the residual stay on
  the caller (the shell will own them); `parity.py` reuses
  `MDXCSeparator._roformer_chunk_starts` for the schedule.

## Export blockers met

1. **Opset 17 is below the dynamo exporter's floor.** torch 2.11's
   `torch.onnx.export` (dynamo=True) emits opset 18 and tries to
   down-convert; the ONNX C version converter fails on `ReduceL2` (opset 18
   takes `axes` as an input, 17 as an attribute) and leaves the graph at 18.
   Handled: opset pinned to 18 (meets "17 or later").
2. **Rotary angle cache.** audio-separator's `rotate_queries_or_keys` caches
   angles sized to the first sequence length; traced, that would bake the
   export length into the graph. Handled: `cache_if_possible=False` on the
   two `RotaryEmbedding` modules before export, so angles come from `Range`
   over the dynamic frame axis.
3. `einops.pack/unpack` inside the transformer loop were replaced in the
   wrapper by explicit permute/reshape (same math); `rearrange` traced fine.

Nothing else blocked: SDPA under `sdpa_kernel`, RMSNorm (`F.normalize`),
GLU, GELU, `split`/`stack`/`unbind` all lowered.

## Parity (F5, CPU only, overlap 2, chunk 352,800, 14 chunks)

`parity_result.json` next to this file is the raw record.

| arm | vocals SI-SDR | residual SI-SDR | seconds / RTF (CPU) |
|---|---|---|---|
| PyTorch `BSRoformer.forward` (fp32, CPU) | 14.888 dB | 14.937 dB | 462 s / 7.70 |
| numpy STFT -> ONNX Runtime CPU EP -> numpy iSTFT | 14.888 dB | 14.937 dB | 412 s / 6.87 |
| delta | 0.000 dB | 0.000 dB | |

Pass line: SI-SDR within 0.5 dB of PyTorch on both stems. Max abs diff
between the two vocals waveforms: 1.15e-5 (peak 0.656). G2's CUDA run of the
same arm scored 14.89 / 14.94 (rounded to 2 dp), so the CPU PyTorch arm also
reproduces G2, which pins the fixture handling (mono -> stereo, peak 0.9
normalization, hamming overlap-add) as identical to audio-separator's.

## Commands

```powershell
$env:CUDA_VISIBLE_DEVICES = ""   # both scripts are CPU only; the GPU was busy
$py = "E:\venvs\dubbing\Scripts\python.exe"
$s2 = "docs\dubbing\stage1-pipeline\s2"
& $py "$s2\export_bs_roformer.py"                # ~200 s export + checker + 2 smoke runs (~100 s)
& $py "$s2\parity.py" --g2-dir docs\dubbing\stage0-gates\g2   # ~15 min on CPU
```

`parity.py` defaults `--g2-dir` to `../../stage0-gates/g2` relative to
itself and fails loud if `score.py` is not there (it imports `si_sdr` and
`load_mono` from it). `--save-dir <dir>` also writes both arms' stems as
wavs; `export_bs_roformer.py --skip-export` re-runs only the checker and
smoke test on the existing file.

## What S2 still owes

- RTF on the CUDA execution provider from the shell (the seam's <= 0.2
  line). CPU RTF ~7 on this box is a non-number for the seam.
- The shell-side STFT/iSTFT (Rust) must match `parity.py::Stft`; that file is
  the reference, parameters from the yaml.
