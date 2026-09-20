---
id: dubbing-stage0
tags: [dubbing, ai, tts, translation, audio, research, high]
related_files: [docs/dubbing/README.md]
status: in-progress
last_sync: 2026-09-13
---

# Stage 0: the gates

Pre-registered 2026-09-13 BEFORE any number existed (R5/R10). Every gate names
its instrument, its control, its pass line, and the branch taken per outcome
band. Results are banked in the ledger at the bottom the hour they land (R13),
with the exact command. A corpse stays listed with its cause of death.

Hardware (this box, the only bench): RTX 3090 Ti 24 GB, 28 cores, 96 GB RAM,
CUDA 12.9, Vulkan 1.4 runtime (no SDK), torch 2.11+cu128, Python 3.12.

Cost discipline (R7): all rungs are $0 compute. The only cost is downloads;
each is priced in the checklist before it happens.

## Shared fixtures (one owner each)

- **F1 sentences** - 10 fixed English lines + their Hebrew translations
  (from OPUS, human), used verbatim by every TTS timing measurement so RTFs
  are paired (R11).
- **F2 speech clip** - the 90 s TTS fixture from the auto-sync arc
  (`speech-clip.mp4`, silence-trimmed lines at known times).
- **F3 subtitle slice** - 500 aligned EN-HE lines from OPUS OpenSubtitles
  v2024 (Moses format), seed-fixed sample, held out from any fine-tune.
- **F4 real scene** - 60 s of a real film scene with dialogue over music and
  effects. Candidate: Sintel (Blender Foundation, CC-BY, has dialogue + score +
  official subtitles). NEEDS MICHAEL'S OK to download (~600 MB).
- **F5 synthetic mix** - F2 speech mixed at 0 dB over a CC0 music bed:
  ground truth exists for both stems, so separation gets a NUMBER (SI-SDR),
  not only an ear.

## G1 - TTS: can VoxCPM2 run ahead of playback here, and does the clone hold?

- **Instrument:** wall-clock over F1 (10 lines, EN then HE), 3 runs each,
  report median RTF = generation seconds / audio seconds. Peak VRAM.
  Arms: (a) PyTorch `voxcpm` package; (b) ggml (`llama.cpp-omni`, CUDA
  build); (c) ggml Vulkan (queued: needs the Vulkan SDK to build).
- **Control:** RTF of arm (a) on the SAME lines is the bar for (b)/(c) (paired).
- **Pass:** RTF <= 0.5 on at least one arm. Cloning: Michael's ear on F4's
  separated stem vs the original voice (his GO/NO-GO, not a metric).
- **Decision tree:** (b) or (c) passes -> packs are weights-only, inference
  compiled into the app. Only (a) passes -> a Python sidecar is the honest
  cost; design the pack around it. None passes -> dubbing cannot be
  real-time on this class of GPU; STOP and report (do not degrade to
  batch-only without asking).
- **VERDICT (2026-09-13 22:26):** arm (b), the ggml CUDA build, PASSES at
  RTF 0.27 EN / 0.30 HE on F1 (idle GPU, paired lines); arm (a) PyTorch
  measures 0.53 (fails by 0.03, TF32 knob unvaried); (c) Vulkan not built.
  Branch taken: packs are weights-only, inference compiled into the app.
  Cloning adds a fixed 0.82 s per CLI invocation (reference re-encoded each
  call), zero per audio second: the product runtime must encode the
  reference once per speaker. The clone's quality by ear is still open.

## G2 - Separation: what removes the dialogue and keeps the rest?

- **Instrument:** on F5, SI-SDR of the residual against the true music bed,
  and of the target against the true speech; wall-clock RTF. On F4, by ear.
  Arms: (a) SAM Audio large, prompt "man speaking" / "woman speaking";
  (b) BS-RoFormer vocals model (`audio-separator`); (c) htdemucs.
- **Control:** the unprocessed mix scored against the music bed (SI-SDR of
  doing nothing) - every arm's claim is excess over this.
- **Pass:** residual SI-SDR >= 10 dB excess over control AND RTF <= 0.2.
- **Decision tree:** both pass -> pick the cheaper to ship (weights size,
  runtime). Only SAM passes on quality -> accept its cost, measure VRAM
  alongside G1's. Nothing passes on F4 by ear -> dubbing ships as "narration
  over the original" (ducked, not removed); record as a deliberate policy.

## G3 - Translation: is a ~1B model good enough for subtitle lines?

- **Direction (Michael, 2026-09-13, mid-campaign):** the paths that matter are
  Japanese / Chinese / Korean / German / French -> ENGLISH (anime, other-locale
  films). Primary pair JA->EN; ZH, KO, DE, FR -> EN follow on the same
  harness. EN->HE (the original pre-registration, already in flight when the
  direction changed) is kept and banked as a SECONDARY point: it is the
  hard direction (generating a low-resource RTL language) and bounds the
  reverse-dubbing case from above.
- **Instrument:** chrF++ (sacrebleu) over F3 (500 lines per pair), zero-shot
  with 3 previous SOURCE lines as context and a length hint; per-line latency on
  llama.cpp (prebuilt CUDA and Vulkan builds; the Vulkan build needs no SDK
  at runtime). Candidates (GGUF, Q8 unless noted): Qwen3.5-0.8B,
  Qwen3.5-2B, Qwen3-1.7B, Gemma 4 E2B. Ceiling arm: Qwen3.5-4B (the
  "affordable" bar).
- **Controls (pre-registered):** copy-source (English through unchanged,
  chrF++ vs the Hebrew reference: the floor) and the 4B ceiling. Claims are
  reported as fraction of the ceiling, never raw.
- **Pass:** >= 85% of the ceiling's chrF++ AND median latency <= 0.5 s per
  line. Per-class breakdown (R12): short (<= 6 words) vs long lines, and
  lines with names/idioms flagged by hand.
- **Decision tree:** pass -> use as-is now, fine-tune later for polish.
  70-85% -> Stage 2 (fine-tune on OPUS + teacher distillation) is REQUIRED
  before shipping; ship nothing on it meanwhile. < 70% -> the 2B/4B carries
  the feature; record the VRAM/latency cost against G1's budget.
- **Fine-tune data note:** OPUS OpenSubtitles v2024 is human-translated and
  exactly the target distribution (short, timed lines, many languages); it
  is the primary corpus. Kimi K3 (2.8T MoE, API only) is a teacher for gaps
  and for filtering, not the first source. F3 is held out from all of it.
- **VERDICT (2026-09-13 21:56, all 7 pairs, `g3/verdict.py`):** the 2B is
  the Stage-1 translator. As % of the 4B ceiling, 0.8B / 2B: JA 65 / 80,
  ZH 78 / 85, KO 65 / 82, RU 79 / 88, DE 77 / 87, FR 81 / 90, EN->HE 46 / 67.
  Latency passes everywhere (medians 0.11-0.25 s). Branches taken: 2B ships
  as-is on RU/DE/FR (ZH on the line); JA and KO, the anime pairs, are in the
  fine-tune band, so Stage 2 is REQUIRED before they ship; the 0.8B passes
  nowhere; EN->HE is carried by the 4B only. The ceiling itself is far from
  the human reference on JA (29.7 chrF++, len ratio 0.80), so JA quality is
  a data problem for every size, not a size problem.

## G4 - Duration fit: does the dub land in the cue window?

- **Instrument:** for 50 translated F3 lines with original cue durations,
  ratio = TTS audio length / cue duration, using G1's winning arm.
- **Pass:** >= 80% of lines within [0.7, 1.15].
- **Decision tree:** pass -> no duration control needed beyond a <= 15%
  time-stretch on the tail. Fail -> the translator gets a hard length budget
  (characters per second derived from the cue) and G3 is re-scored under it
  (R9: one variable at a time, the budget is its own arm).
- **VERDICT (2026-09-13 22:28):** FAIL on both arms (CLI 34.6%, PyTorch
  42.3% within the window). Branch taken: the length-budget arm. Post-hoc
  reads that shape it (labelled, not pre-registered): the misses are short
  dubs inside padded windows, real next-cue overruns are 4-6% of lines, and
  the dub runs ~1.3x the original actor's speech, so the budget derives from
  the room to the next cue. A runaway guard on the ggml path is a separate
  product requirement (one CLI line ran to the 200-step cap, 32 s).

## G0 - ASR: does the app's whisper hold up on real anime audio, end to end?

Pre-registered 2026-09-13 23:05, before any number (ASR launched 23:06). Added after the direction
change: the primary paths are JA/ZH/KO -> EN, so the source text comes from
whisper on the film's audio, not from a subtitle file, and G3 measured the
translator on clean text only.

- **Fixture F6:** Tomb Raider King S01E09 (Crunchyroll WEB-DL, from the
  Rillio cache, Michael's copy): 23 min, Japanese stereo AAC, official
  English ASS subtitles (292 cues, 282 spoken after dropping the 10
  top-positioned all-caps typeset cards) plus DE/FR/RU/ZH tracks. Held out
  from any fine-tune. `E:\datasets\g0\` (`ja16k.wav`, `en.srt`, `cues.jsonl`).
- **Instrument:** the PRODUCT path re-implemented 1:1 on the same engine
  (whisper.cpp via pywhispercpp, the app's own `ggml-small-q5_1.bin`, CPU,
  10 s chunks, audio_ctx 768, greedy, no_context, language auto then locked,
  WebRTC VAD gate at 2% voiced, no_speech 0.6, suppress blank/nst), then the
  G3 prompt through llama-server (one owner: `g3/score.translate`) per
  official English cue, with the whisper text that overlaps the cue window
  as the source and the 3 previous cues' source as context. Score: chrF++
  against the official English line, 292 cues, paired across arms. R1
  census: count cues with empty source (ASR gave nothing in the window).
- **Arms:** (a) product construction + Qwen3.5-2B; (b) same ASR + Qwen3.5-4B
  (translation ceiling given this ASR); (c) whisper native construction
  (30 s windows, full audio_ctx, same model) + 2B: the ASR knob varied (R6);
  (d) if a bigger whisper is ever fetched, it slots in as the ASR ceiling.
- **Controls:** copy-source (the whisper Japanese text scored as-is: the
  floor); the same 2B on the official GERMAN track translated to English
  (text-only path on the same cues and references: what the translator
  achieves when ASR is perfect, a different source language but the same
  target lines).
- **Pass:** (a) >= 85% of (b) (translation is not the bottleneck under ASR
  noise) AND (a) >= 85% of (c) (the 10 s / 768 construction costs < 15%).
  Raw chrF++ is reported but has no pre-registered line: the reference is a
  free translation, not a literal one.
- **Decision tree:** both pass -> the product ASR ships as-is for JA; Stage 2
  fine-tunes on ASR-shaped input too (noise-aware). (a) < 85% of (c) -> the
  chunking is the defect: give the dubbing worker a longer window (it runs
  ahead of playhead, it can afford 30 s). (a) < 85% of (b) -> the 2B breaks on
  ASR noise: Stage 2 must train on whisper outputs, not clean text, and the
  4B carries JA until then. Empty-source cues > 10% -> the VAD gate or
  no_speech threshold is dropping real dialogue; vary them before any
  verdict.
- **VERDICT (2026-09-13 23:19):** PASS on both lines. (a) product ASR + 2B
  = 22.02 chrF++, 86.1% of the 4B on the same ASR text and 98.6% of the
  native-window ASR + 2B; 4.6% of cues get no ASR text. Text-only control
  (German track + 2B) = 36.08, so the audio path lands at 61% of what the
  translator does from clean text, the same order as G3's clean-text JA
  numbers: the gap is the JA->EN pair and the free official translation,
  not whisper's chunking. Branch: the product ASR ships as-is for JA; Stage
  2 trains on ASR-shaped input too. ASR mishearings pass straight through
  the translator, so a bigger whisper is the next arm (a priced download).

## Candidates noted, not admitted (each enters only as an arm on the gates above)

- **AuK / AuK-Flash** (Tencent Hunyuan + SJTU, arXiv 2609.08936, released
  2026-09-08, MIT, `tencent/AuK` on HF): 1.5B instruction-driven speech
  generation + editing (zero-shot TTS from a reference, instruct TTS,
  paralinguistic editing by text instruction, numeric speed/pitch edits,
  enhancement/separation). EN + ZH only. Reported peak VRAM ~25 GB bf16
  (16.75 GB with CPU offload); no RTF or latency published; PyTorch only.
  Not admitted to Stage 1: VRAM and no ggml path. Earns a place as an
  offline Stage 2 tool (emotional variants of clean lines for a VoxCPM2 LoRA;
  instruct-TTS teacher) and re-enters as a G1 arm on F1 and a G5 arm on F6
  the day a ggml port exists. Its emotion editing is text-instructed, i.e.
  the tag route; it is not audio-conditioned emotion transfer.
- **SAM Audio** (Meta, gated weights, Michael's access request): a G2 arm on
  F5 the day the weights land; promptable per-speaker extraction is the
  case where it should beat BS-RoFormer (overlapping speakers, cleaner
  per-line prompts for G5). Design already assumes separation runs ONCE
  upstream on the worker: the dialogue stem feeds whisper and the per-line
  prompt, the residual is the bed the dub is mixed over.
- **MossFormer2 (Alibaba ClearerVoice-Studio, Apache-2.0):** the
  overlapped-speech splitter BS-RoFormer lacks. `MossFormer2_SS_16K` is
  audio-only 2-speaker separation (16 kHz); `MossFormer2_SE_48K` enhancement;
  the target-speaker-extraction model (`AV_MossFormer2_TSE_16K`) needs a
  real face video, so it is out for anime. Role in the pipeline: run only on
  cue windows flagged as overlapping (after BS-RoFormer removed the music),
  then assign the two outputs to speakers by embedding similarity to the
  per-speaker references. Admission: a G2b fixture (F7: two F2/F1 lines
  mixed at 0 dB, ground truth per speaker), SI-SDRi per speaker after
  assignment, RTF on the flagged windows only. Not needed for a first ship:
  overlapped lines fall back to the speaker reference alone as their prompt.
- **Emotion transfer (G5, to pre-register after G0):** VoxCPM2's documented
  combination of `reference_wav_path` (speaker identity, encoded once) with
  `prompt_wav_path` + `prompt_text` (the source line's own audio from the
  stem, with whisper's transcript), continued in English. Instruments:
  language ID + whisper WER on the dub (cross-lingual continuation), a
  speech-emotion model (emotion2vec) arousal/valence correlation source vs
  dub against the reference-only control, prompt denoiser as the knob arm,
  Michael's ear on 10 lines. Fixture: F6's 282 cues.

## Handoff (R15: kept current so the next session starts here, not from archaeology)

**State (2026-09-13 23:19): all five gates decided.** G0 PASS (the app's
whisper path on real anime audio: 2B end to end = 86% of the 4B and 99% of
the native window; F6 = Tomb Raider King S01E09 from the Rillio cache, at
`E:\datasets\g0`, harness `g0/transcribe.py` + `g0/score.py` +
`g0/run_arm.ps1`, llama-server shared via `g3/server.ps1`). G1 PASS on the ggml
CUDA arm (RTF 0.27/0.30; PyTorch 0.53); G2 PASS (BS-RoFormer ep368 overlap 2,
+14.9 dB at RTF 0.18); G3: Qwen3.5-2B is the Stage-1 translator (85-90% of
the 4B ceiling on RU/DE/FR/ZH; JA/KO at 80-82% need Stage 2; 0.8B passes
nowhere; EN->HE is 4B-only); G4 FAIL on the window ratio (35-42%), branch =
length-budget arm derived from the room to the next cue. Qwen3-1.7B banked
and retired (under the 2B everywhere, copies German through on 5% of lines).
Nothing is running. `python g3/verdict.py E:\datasets\opus` prints every
banked arm as a fraction of its pair's ceiling with the branch applied; a
new G3 arm across all pairs is `g3/run_pairs.ps1 -Model <gguf> -Label <x>`
(skips banked pairs, never alongside a G1 timing). Every model file is on disk and
size-verified (`E:\models\gguf\<repo>\`, `E:\models\VoxCPM2`,
`E:\models\VoxCPM2-GGUF\VoxCPM2-{BaseLM,Acoustic}-F16.gguf`).

**What Stage 1 inherits from the gates:** inference compiled in (llama.cpp
for the 2B, llama.cpp-omni ggml for VoxCPM2, ONNX Runtime for BS-RoFormer),
weights-only packs; encode the clone reference once per speaker (0.82 s per
call otherwise); a runaway guard on the ggml TTS path (200 steps = 32 s);
translator length budget from the next-cue gap; Stage 2 fine-tune before
JA/KO ship. Still Michael's: by-ear on F4 stems
(`E:\datasets\g2\sintel\f4_bs_roformer_ov2`) and the G1 clone wavs
(`E:\datasets\g1\ggml-cuda-clone\`), the SAM Audio access request, an anime
clip with reference subs for G0.

**Ops runbook (every trap already paid for):** one llama-server at a time on
8080 (`g3/run_arm.ps1` owns it; a second run collides); `hf download` stalls
on big files here, BITS dies on HF's expiring redirect tokens, and a single
curl is capped at ~0.5 MB/s: fetch big files with 4 parallel ranged curls in
64 MB pieces (`--speed-limit 200000 --speed-time 15` aborts a stalled piece,
the retry re-resolves the token), ~9.6 MB/s aggregate, script at
`tools/ranged-download.ps1 -Url <resolve/main url> -Out <path>`; separator timings
are only valid on an idle GPU and must come from the library's own
"Separation duration" line; `python -m pip` inside the venv, never the global
python; never `2>&1 | Tee-Object` a stderr-progress process under Stop;
`Remove-Item` and `cmd /c` never in one command; corrupted `.ckpt` after a
killed download reads as "PytorchStreamReader failed": delete and re-fetch;
VoxCPM's torch.compile dies with "Failed to find C compiler" inside the venv
because triton-windows looks for its bundled TinyCC under the venv's platlib
while triton lives in the global site-packages: every gate script that
compiles calls `gates_env.point_cc_at_triton_tcc()` first (derives CC from
`triton.__file__`), so never launch VoxCPM PyTorch from a bare shell without
it. G1(a)'s F1 lines come from the EN-HE slice (`en-he\f3-slice.jsonl`, src
English / ref Hebrew); the queue once fed it the JA slice, which flips the
languages.

## Ledger (bank as you go)

| when | gate | arm | result | command / path |
|---|---|---|---|---|
| 2026-09-13 20:15 | G3 JA->EN | control copy-source | chrF++ 0.81 (floor) | `g3/score.py --copy`, `E:\datasets\opus\en-ja\g3\copy-source.jsonl` |
| 2026-09-13 20:16 | G3 JA->EN | Qwen3.5-0.8B Q8, llama.cpp CUDA b10944, 99 layers offloaded | chrF++ 19.23 (short 18.92 / long 19.43); latency median 0.121 s, p90 0.208 s; len ratio 0.646; 0 empty. Verdict PENDING the 4B ceiling. | `g3/run_arm.ps1 -Label qwen3.5-0.8b-q8-cuda`, `E:\datasets\opus\en-ja\g3\qwen3.5-0.8b-q8-cuda.jsonl` |
| 2026-09-13 20:17 | G3 JA->EN | Qwen3.5-2B Q8, llama.cpp CUDA | chrF++ 23.72 (short 25.83 / long 22.75); latency median 0.135 s, p90 0.235 s; len ratio 0.686; 0 empty. 0.8B = 81% of 2B. Verdict PENDING the 4B ceiling. | `E:\datasets\opus\en-ja\g3\qwen3.5-2b-q8-cuda.jsonl` |
| 2026-09-13 21:44 | G3 JA->EN | Qwen3.5-4B Q8 CEILING | chrF++ 29.69 (short 31.36 / long 28.94); median 0.190 s, p90 0.249 s; len ratio 0.804; 0 empty. Floor = 3% of ceiling. VERDICT (tree applied): 0.8B = 65% -> FAIL, the 2B/4B carries JA; 2B = 80% -> Stage 2 fine-tune REQUIRED before shipping. Note the 4B itself is short of the human reference by a wide margin (29.7 chrF++, len ratio 0.80: it under-translates), so JA is the hardest pair regardless of size. | `E:\datasets\opus\en-ja\g3\qwen3.5-4b-q8-cuda-ceiling.jsonl`, `g3/verdict.py` |
| 2026-09-13 20:19 | G3 ZH->EN | control copy-source | chrF++ 3.63 (floor; Latin names/numbers in zh_CN refs) | `E:\datasets\opus\en-zh_CN\g3\copy-source.jsonl` |
| 2026-09-13 20:20 | G3 ZH->EN | Qwen3.5-0.8B Q8 | chrF++ 29.71 (short 32.38 / long 28.40); median 0.140 s, p90 0.253 s; len ratio 0.833 | `E:\datasets\opus\en-zh_CN\g3\qwen3.5-0.8b-q8-cuda.jsonl` |
| 2026-09-13 20:22 | G3 ZH->EN | Qwen3.5-2B Q8 | chrF++ 32.43 (short 36.45 / long 30.42); median 0.223 s, p90 0.325 s; len ratio 0.857. 0.8B = 92% of 2B. Ceiling pending. | `E:\datasets\opus\en-zh_CN\g3\qwen3.5-2b-q8-cuda.jsonl` |
| 2026-09-13 21:46 | G3 ZH->EN | Qwen3.5-4B Q8 CEILING | chrF++ 38.08 (short 44.09 / long 35.01); median 0.187 s, p90 0.256 s; len ratio 0.955; 0 empty. Floor = 10% of ceiling. VERDICT (tree applied): 0.8B = 78% -> Stage 2 fine-tune REQUIRED; 2B = 85.2% -> PASS, but ON the line (short 83% / long 87%): treat as a marginal pass, fine-tune for polish. | `E:\datasets\opus\en-zh_CN\g3\qwen3.5-4b-q8-cuda-ceiling.jsonl`, `g3/verdict.py` |
| 2026-09-13 20:23 | G3 KO->EN | control copy-source | chrF++ 1.71 (floor) | `E:\datasets\opus\en-ko\g3\copy-source.jsonl` |
| 2026-09-13 20:23 | G2 (fixtures) | F4 / F5 from Sintel | F4 = 60 s at 107.25 s (33.5 s dialogue, 12 cues); F5 background = 60 s at 630.8 s (cue-free tail: score + effects); F5 mix = F2 speech over it at 0 dB. Control (unprocessed mix): SI-SDR speech -0.05 dB, background +0.06 dB | `fixtures/sintel_windows.py`, `E:\datasets\g2\sintel`, `E:\datasets\g2\f5` |
| 2026-09-13 20:24 | G2 F5 | htdemucs_ft (4-model bag), torch CUDA | vocals SI-SDR 11.21 dB (+11.26 over control); residual 10.04 dB (+9.98, ON the 10 dB line); RTF 0.638 wall (0.57 separation only) - FAILS the 0.2 RTF line. CONFOUND: measured while a G3 llama-server arm ran on the same GPU; re-time idle before any verdict (R9). | `E:\datasets\g2\f5\htdemucs_ft\`, `g2/score.py score --label htdemucs_ft` |
| 2026-09-13 20:25 | G2 F5 | htdemucs (single model) | vocals 9.98 dB (+10.03); residual 9.54 dB (+9.48, 0.5 dB UNDER the line); separator-reported duration 10 s for 60 s = RTF 0.17 (wall 0.95 incl. its first-run model fetch + process start). Same GPU confound as above. | `E:\datasets\g2\f5\htdemucs\` |
| 2026-09-13 20:25 | G3 KO->EN | Qwen3.5-0.8B Q8 | chrF++ 20.75 (short 20.79 / long 20.75); median 0.252 s, p90 0.382 s; len ratio 0.704 | `E:\datasets\opus\en-ko\g3\qwen3.5-0.8b-q8-cuda.jsonl` |
| 2026-09-13 20:28 | G3 KO->EN | Qwen3.5-2B Q8 | chrF++ 26.47 (short 28.11 / long 25.59); median 0.243 s, p90 0.325 s; len ratio 0.780. 0.8B = 78% of 2B (widest gap of the three CJK pairs). Ceiling pending. | `E:\datasets\opus\en-ko\g3\qwen3.5-2b-q8-cuda.jsonl` |
| 2026-09-13 21:48 | G3 KO->EN | Qwen3.5-4B Q8 CEILING | chrF++ 32.10 (short 33.88 / long 31.11); median 0.202 s, p90 0.257 s; len ratio 0.882; 0 empty; census clean (1 echo = an English line in the corpus). Floor = 5% of ceiling. VERDICT (tree applied): 0.8B = 65% -> FAIL, the 2B/4B carries KO; 2B = 82% -> Stage 2 fine-tune REQUIRED before shipping. | `E:\datasets\opus\en-ko\g3\qwen3.5-4b-q8-cuda-ceiling.jsonl`, `g3/verdict.py` |
| 2026-09-13 20:31 | G3 EN->HE (secondary) | control / 0.8B / 2B | floor 3.30; Qwen3.5-0.8B 16.27 @ 0.132 s (len ratio 0.947); Qwen3.5-2B 23.79 @ 0.134 s (len ratio 0.827). 0.8B = 68% of 2B: generating Hebrew is the hard direction, as pre-registered. | `E:\datasets\opus\en-he\g3\` |
| 2026-09-13 21:52 | G3 EN->HE (secondary) | Qwen3.5-4B Q8 CEILING | chrF++ 35.58 (short 37.55 / long 33.47); median 0.237 s, p90 0.338 s; len ratio 0.960; 0 empty; census clean (every hypothesis is Hebrew script). Floor = 9% of ceiling. VERDICT (tree applied): 0.8B = 46% and 2B = 67% -> both FAIL, only the 4B carries EN->HE; sample lines show the 4B still mis-picking words (bracelets -> towels) so even the ceiling is rough. Secondary pair: no product decision rides on it. | `E:\datasets\opus\en-he\g3\qwen3.5-4b-q8-cuda-ceiling.jsonl`, `g3/verdict.py` |
| 2026-09-13 20:38 | G3 RU->EN | control / 0.8B / 2B | floor 3.08; Qwen3.5-0.8B 34.41 (short 33.05 / long 35.21) @ 0.112 s (p90 0.138); Qwen3.5-2B 38.60 (39.68 / 37.99) @ 0.120 s. 0.8B = 89% of 2B. Best pair so far. Ceiling pending. | `E:\datasets\opus\en-ru\g3\` |
| 2026-09-13 21:50 | G3 RU->EN | Qwen3.5-4B Q8 CEILING | chrF++ 43.63 (short 46.51 / long 41.95); median 0.204 s, p90 0.262 s; len ratio 0.885; 0 empty; census clean (1 echo = an English line in the corpus). Floor = 7% of ceiling. VERDICT (tree applied): 0.8B = 79% -> Stage 2 fine-tune REQUIRED; 2B = 88% -> PASS, use as-is now (short 85% / long 91%), fine-tune later. | `E:\datasets\opus\en-ru\g3\qwen3.5-4b-q8-cuda-ceiling.jsonl`, `g3/verdict.py` |
| 2026-09-13 20:42 | G2 F5 (IDLE GPU) | BS-RoFormer ep368 (Viperx-1296), overlap 8 default | vocals 14.91 dB (+14.96); residual 14.96 dB (+14.90): QUALITY PASS by 5 dB. RTF 0.333 (library "Separation duration"): RTF FAIL vs the 0.2 line. Construction knob (overlap) to be varied before any verdict (R6). | `E:\datasets\g2\f5\bs_roformer_ep368\`, `g2/run_arm.ps1` |
| 2026-09-13 20:42 | G2 F5 (IDLE GPU) | htdemucs | vocals +9.86; residual +9.37 (0.6 dB under the line); RTF 0.083 PASS | `E:\datasets\g2\f5\htdemucs_idle\` |
| 2026-09-13 20:42 | G2 F5 (IDLE GPU) | htdemucs_ft | vocals +11.38; residual +9.88 (0.1 dB under); RTF 0.233 (just over) | `E:\datasets\g2\f5\htdemucs_ft_idle\` |
| 2026-09-13 20:48 | G2 F5 (IDLE GPU) | BS-RoFormer ep368, `--mdxc_overlap=2` | vocals 14.89 dB (+14.94); residual 14.94 dB (+14.88); RTF 0.183. Overlap 4 = RTF 0.333 (same quality); batch 4 = no change. **G2 VERDICT (F5): PASS on both lines -> BS-RoFormer ep368 @ overlap 2 is the separator** (the only arm passing both; htdemucs fails quality by 0.6 dB, htdemucs_ft misses both narrowly). Still owed: Michael's by-ear on F4 (real scene). | `E:\datasets\g2\f5\bs_roformer_ov2\` |
| 2026-09-13 21:09 | G3 DE->EN | control / 0.8B / 2B | floor 14.02; Qwen3.5-0.8B 33.63 @ 0.113 s; Qwen3.5-2B 37.91 @ 0.131 s (0.8B = 89% of 2B) | `E:\datasets\opus\de-en\g3\` |
| 2026-09-13 21:54 | G3 DE->EN | Qwen3.5-4B Q8 CEILING | chrF++ 43.52 (short 48.28 / long 41.13); median 0.208 s, p90 0.263 s; len ratio 0.926; 0 empty; census clean (5 echoes = English lines in the corpus). Floor = 32% of ceiling (shared Latin script inflates the copy floor; the excess-over-floor is still 29 points). VERDICT (tree applied): 0.8B = 77% -> Stage 2 fine-tune REQUIRED; 2B = 87% -> PASS, use as-is, fine-tune later. | `E:\datasets\opus\de-en\g3\qwen3.5-4b-q8-cuda-ceiling.jsonl`, `g3/verdict.py` |
| 2026-09-13 21:15 | G3 FR->EN | control / 0.8B / 2B | floor 14.42; Qwen3.5-0.8B 36.02 @ 0.172 s; Qwen3.5-2B 40.39 @ 0.118 s (0.8B = 89% of 2B) | `E:\datasets\opus\en-fr\g3\` |
| 2026-09-13 21:55 | G3 FR->EN | Qwen3.5-4B Q8 CEILING | chrF++ 44.64 (short 43.95 / long 44.97); median 0.200 s, p90 0.254 s; len ratio 0.889; 0 empty; census clean (2 echoes = names / interjections). Floor = 32% of ceiling. VERDICT (tree applied): 0.8B = 81% -> Stage 2 fine-tune REQUIRED; 2B = 90% -> PASS, use as-is, fine-tune later. Best pair of the seven. | `E:\datasets\opus\en-fr\g3\qwen3.5-4b-q8-cuda-ceiling.jsonl`, `g3/verdict.py` |
| 2026-09-13 22:04 | G1(a) F1 (idle GPU) | VoxCPM2 via the `voxcpm` PyTorch package, bf16, torch.compile on (inductor + triton-windows), 10 EN + 10 HE lines x 3 runs | RTF median EN 0.530 (max 0.566), HE 0.526 (max 0.550); peak VRAM 5.3 GB; 48 kHz. FAILS the 0.5 line by 0.03 (arm (a) is the control bar for (b)/(c), not the product path). Unvaried construction knob: TF32 matmul is off (torch warned), so a re-time with `set_float32_matmul_precision('high')` is owed before (a) is called dead (R6). First attempt (queue) died on "Failed to find C compiler" (triton tcc lookup under the venv platlib) and had been fed the JA slice; this run is the EN-HE slice with CC derived from triton. | `E:\datasets\g1\pytorch\summary.json`, `run.log`, 20 wavs |
| 2026-09-13 22:05 | G1(b) first attempt | `voxcpm2-cli.exe` (CUDA build) via `g1/time_voxcpm_cli.ps1` | FAILED before timing: Start-Process joined the argument array unquoted (a multi-word line shifted the positionals, the second word became the BaseLM path) AND the exe read narrow argv so every non-Latin char arrived as '?'. Fixed: single quoted command line in the script; CLI rebuilt with argv re-derived as UTF-8 from the wide command line (`voxcpm2_cli.cpp`). Re-run pending an idle GPU. | `E:\datasets\g1\ggml-cuda\warmup.wav.log.err` |
| 2026-09-13 22:09 | G4 arm: ggml CLI (2B translations of the 52 Sintel DE/FR cues, cloned voice) | `g4/fit.py synth --cli --ref clone_ref.wav` + `score` | 18/52 within [0.7, 1.15] = 34.6%: FAIL on the pre-registered line. Ratio median 0.594, p10 0.345, p90 1.042; 31 too short, 3 too long. Reading the misses: the short ones are one-word cues with 3-4 s windows ("Scales!" 1.12 s / 3.25 s), i.e. the cue window carries reading padding the dub cannot fill; the long tail holds one RUNAWAY (\"Soon over. Shh...\" -> 32.0 s = the CLI's 200-step cap; the PyTorch path has retry_badcase, the CLI has none). Branch per tree: FAIL -> the translator gets a length budget arm. Two post-hoc views added the same hour (labelled as such, not pre-registered): (1) vs the ORIGINAL dialogue seconds per window (WebRTC VAD on the 5.1 center channel, `fit.py speech`): median 1.35, 35/52 too long, 10/52 inside; the dub speaks slower than Sintel's actors, and the VAD under-reads the shortest shouted lines (0.08 s for "Scales!"), so this denominator is noisy at the short end. (2) NEXT-CUE COLLISIONS (audio still playing when the next cue starts, the thing that breaks a dub): 2 of 50 = 4%, one of them the runaway; slack to the next cue median 3.3 s, p10 0.6 s. So the pre-registered miss is mostly padded windows, and the budget arm should be derived from the room to the next cue rather than the cue length. | `E:\datasets\g4\translated-2b-cli.synth.jsonl`, `E:\datasets\g4\speech.jsonl`, `fit.py score --speech` |
| 2026-09-13 22:19 | G1(b) F1 (idle GPU) | `voxcpm2-cli.exe` ggml CUDA (rebuilt with UTF-8 argv), same 10 EN + 10 HE lines x 3 runs, RTF from the CLI's own "Elapsed / Audio" (model load excluded, it reloads per call) | RTF median EN 0.271 (max 0.313), HE 0.300 (max 0.374): PASS, and 2x faster than arm (a)'s 0.53 on the identical lines (paired). Hebrew text reaches the model intact (smoke: "Text: שלום, מה שלומך היום?"). | `E:\datasets\g1\ggml-cuda\summary.json`, per-call `*.wav.log.err` |
| 2026-09-13 22:25 | G1(b) clone F1 (idle GPU) | same CLI with `-r clone_ref.wav` (15 s Sintel voice) | RTF median EN 0.511 (max 0.818), HE 0.631 (max 1.059) as measured per call. Decomposed from the 60 paired logs: clone minus plain elapsed = 0.822 s median (p10 0.65, p90 1.10), 0.84 s on <2 s lines vs 0.82 s on >=3.5 s lines, audio length ratio 1.000: a FIXED per-invocation cost (the reference encode), not a slower synthesis. Net of it the clone arm equals plain. Product requirement banked: encode the reference ONCE per speaker and keep it across lines; the per-call CLI shape is not the product shape. | `E:\datasets\g1\ggml-cuda-clone\summary.json` |
| 2026-09-13 22:26 | G1 VERDICT | (a) 0.53 fail by 0.03 (TF32 unvaried), (b) 0.27-0.30 PASS, (c) Vulkan not built (no SDK) | Tree branch: (b) passes -> packs are weights-only, inference compiled into the app (llama.cpp-omni ggml). Peak VRAM for (a) 5.3 GB; (b) not instrumented for VRAM yet (F16 GGUFs total 4.8 GB on disk). Cloning by ear on F4 stays Michael's call. | `g1/time_voxcpm_cli.ps1`, `g1/time_voxcpm.py` |
| 2026-09-13 22:27 | G4 arm: PyTorch `voxcpm` (same 52 lines, cloned via `reference_wav_path`) | `g4/fit.py synth --pytorch --ref` + `score --speech` | 22/52 within [0.7, 1.15] = 42.3%: FAIL on the pre-registered line (28 too short, 2 too long); runaways 0 (its retry_badcase re-rolled "Soon over. Shh..." to 3.52 s where the CLI ran to 32 s); next-cue collisions 3/50 = 6%; vs original speech median 1.28. Audio lengths otherwise match the CLI arm line for line (median ratio 0.946). | `E:\datasets\g4\translated-2b.synth.jsonl`, `E:\datasets\g4\wav-pytorch\` |
| 2026-09-13 22:28 | G4 VERDICT | both arms | FAIL on the pre-registered window ratio (35-42% within): branch = the translator gets a hard length budget and G3 is re-scored under it as its own arm. Evidence for HOW to set it: the misses are short dubs in padded windows, real overruns are 4-6% of lines, so the budget derives from the room to the next cue (start-to-next-start), not the cue length. Separate product requirement: a runaway guard on the ggml path (the CLI's 200-step cap is 32 s of audio; PyTorch's retry_badcase re-rolls at a 6x length ratio). | `fit.py score --speech`, `next_cue` block |
| 2026-09-13 22:31 | G3 Qwen3-1.7B Q8 (fourth candidate), all 7 pairs, idle GPU | `g3/run_pairs.ps1 -Label qwen3-1.7b-q8-cuda` | As % of ceiling: JA 59, ZH 79, KO 56, RU 81, DE 78, FR 77, EN->HE 46; medians 0.06-0.10 s (fastest arm). Below the Qwen3.5-2B on every pair and below the 0.8B on JA/KO/FR. Census: 0 empty, no think-tag leaks, but 25/500 German lines COPIED THROUGH untranslated (the Qwen3.5 arms' echoes were English lines in the corpus; these are German) and 5/500 EN->HE lines left in English. RETIRED: cause of death = lower quality than a smaller Qwen3.5 plus a source-copy failure mode. | `E:\datasets\opus\*\g3\qwen3-1.7b-q8-cuda.*` |
| 2026-09-13 23:10 | G0 ASR, F6 (23 min JA anime), CPU 8 threads, `ggml-small-q5_1.bin` | (a) product construction: 10 s chunks, audio_ctx 768 (`g0/transcribe.py --construction product`) | language locked "ja" on chunk 1; 139 chunks, 0 skipped by the VAD gate; 401 segments; RTF 0.166 (wall 230 s). Alignment: 13/282 cues (4.6%) get no ASR text, under the 10% tripwire. Hallucination census: 12 exact-repeat neighbours, 4 texts seen >= 3x (a looped line over the cold open's music, "ああ" x5). Output is Japanese script throughout. | `E:\datasets\g0\asr-product.jsonl`, `.summary.json`, `aligned-product.jsonl` |
| 2026-09-13 23:14 | G0 ASR, F6 | (c) native construction: 30 s windows, full audio_ctx, same model/gate/cleaning | RTF 0.168 (wall 232 s): the 10 s / 768 construction buys NO throughput on CPU, it exists for first-output latency. 381 segments; 3/282 cues empty. Census: 7 repeat neighbours, one 7x loop ("電話の音が聞こえない"); non-Japanese garbage tokens appear on loud effects lines ("愢泣� Vitamin", "bench") where the product construction gave clean Japanese. | `E:\datasets\g0\asr-native.jsonl`, `aligned-native.jsonl` |
| 2026-09-13 23:15 | G0 floor | copy-source on the product ASR text | chrF++ 0.47 (13 empty sources carried through as empty). | `E:\datasets\g0\arms\copy-source-product.jsonl` |
| 2026-09-13 23:15 | G0 arm (a) | product ASR + Qwen3.5-2B, G3 prompt, 282 cues | chrF++ 22.02 (short 19.82 / long 23.19); median 0.138 s; len ratio 1.00; 13 empty (the ASR holes). | `E:\datasets\g0\arms\qwen3.5-2b-product-asr.jsonl` |
| 2026-09-13 23:16 | G0 arm (b) | product ASR + Qwen3.5-4B (translation ceiling given this ASR) | chrF++ 25.58 (21.57 / 27.67); median 0.234 s; len ratio 1.28 (the 4B pads ASR fragments into fuller sentences). (a) = 86.1% of (b): PASS the first line. | `E:\datasets\g0\arms\qwen3.5-4b-product-asr.jsonl` |
| 2026-09-13 23:17 | G0 arm (c) | native-construction ASR + Qwen3.5-2B (the ASR knob varied) | chrF++ 22.33 (21.63 / 22.73); 3 empty. (a) = 98.6% of (c): PASS the second line; the 10 s / 768 construction costs 1.4%, and its 13 holes are matched by the native arm's garbage tokens. | `E:\datasets\g0\arms\qwen3.5-2b-native-asr.jsonl` |
| 2026-09-13 23:18 | G0 control | official GERMAN track + Qwen3.5-2B (text-only path, same 282 references) | chrF++ 36.08 (34.77 / 36.72). What the translator reaches from clean text on these exact lines; the JA audio path lands at 61% of it. Same order as G3's clean-text JA (2B 23.7 on OPUS vs 22.0 here end to end), so the loss is the JA->EN pair and the free official translation, not the ASR. | `E:\datasets\g0\arms\qwen3.5-2b-german-track.jsonl` |
| 2026-09-13 23:19 | G0 VERDICT | tree applied | Both pre-registered lines PASS (86.1% >= 85%, 98.6% >= 85%); empty-source 4.6% < 10%. Branch: the product ASR ships as-is for JA; Stage 2 fine-tunes on ASR-shaped input as well as clean text. Sample reads: where ASR is right the 2B and 4B agree with the official line; where ASR misheard a word ("生けてる件" for the sword) both translate the wrong word faithfully, so ASR errors pass through untouched: a whisper `medium`/`large` arm is the next ASR question, priced as a download. | `g0/score.py`, `E:\datasets\g0\arms\` |
| 2026-09-13 21:56 | G3 VERDICT (all 7 pairs, tree applied by `g3/verdict.py`) | 0.8B / 2B as % of the 4B ceiling | JA 65 / 80, ZH 78 / 85, KO 65 / 82, RU 79 / 88, DE 77 / 87, FR 81 / 90, EN->HE 46 / 67. Every latency median <= 0.25 s (pass). Branches: the 0.8B passes NOWHERE (fails outright on JA/KO/HE, fine-tune band elsewhere); the 2B passes RU/DE/FR clean, ZH on the line, and needs Stage 2 fine-tuning for JA/KO (the anime pairs). EN->HE: only the 4B. Decision: Qwen3.5-2B is the Stage-1 translator; Stage 2 (OPUS + teacher distillation) is REQUIRED before JA/KO ship and is the lever for the 0.8B if a smaller pack is ever wanted. | `g3/verdict.py E:\datasets\opus` |
| 2026-09-13 21:20 | G4 (prep) | Sintel DE+FR cues -> EN | 52 cues (26 per track, identical windows) translated by the 2B and by the 0.8B (e.g. "Diese Klinge birgt eine finstere Vergangenheit." -> "This blade holds a dark past.", 0.085 s). Synthesis waits on G1. | `E:\datasets\g4\translated-{2b,0.8b}.jsonl` |
| 2026-09-13 21:20 | ops | downloads | HF CDN caps a single connection at ~0.4-0.7 MB/s here; 4-way ranged curl (`scratchpad/ranged-download.ps1`) pulls 10-16 MB/s PER PART. BITS dies on HF's expiring redirect tokens; `hf download` stalls at 0 MB. | |
| 2026-09-13 | G3 (instrument) | first-run bug | runner merged stderr under Stop: arm died at line 50; class split used SOURCE words (meaningless for JA). Both fixed before any number was banked. | `g3/run_arm.ps1`, `g3/score.py` |
| 2026-09-13 | G1 (setup) | ggml VoxCPM2 CLI | built `voxcpm2-cli.exe` (CUDA 12.9 + VS 18 needs `-DCMAKE_CUDA_FLAGS=-allow-unsupported-compiler`, Ninja, vcvars64); untested until weights land | `E:\src\llama.cpp-omni\build-cuda\bin\voxcpm2-cli.exe` |
| 2026-09-13 | fixtures | F3 JA-EN | 500/1.91M eligible of 2.07M lines; reference has merged/misaligned lines (OPUS alignment noise): all arms share it, the ceiling ratio absorbs it | `E:\datasets\opus\en-ja\f3-slice.jsonl` (seed 1) |
| 2026-09-13 | fixtures | F3 EN-HE (secondary) | 500/52.6M eligible of 58.8M lines (slicer fixed: split on `\n` only) | `E:\datasets\opus\en-he\f3-slice.jsonl` |

Corpses: none yet.
