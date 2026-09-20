---
id: dubbing-stage0
tags: [dubbing, ai, tts, translation, audio, research, high]
related_files: [docs/dubbing/stage0-gates/README.md]
status: in-progress
last_sync: 2026-09-13
---

# Dubbing Stage 0 - checklist

Evidence ledger: an item is done only on observed output (numbers banked in
the gates ledger), never on "it installed".

## Direction change (2026-09-13, mid-run)
Primary pairs are JA/ZH/KO/DE/FR/RU -> EN (anime, other-locale films) and RU<->EN;
EN->HE demoted to a secondary banked point. Gates doc updated.

## Setup (downloads priced before they happen)
- [x] llama.cpp b10944 prebuilt: CUDA 12.4 (242 MB) + cudart (373 MB) + Vulkan (30 MB) -> `E:\tools\llama.cpp`
- [x] venv `E:\venvs\dubbing`: voxcpm, sacrebleu, audio-separator[gpu] (+audioread, ninja)
- [x] OPUS en-he (1.5 GB), en-ja (52 MB), en-ko (801 MB), en-zh_CN (567 MB), en-ru (1.65 GB), de-en (1.6 GB), en-fr (2 GB) unpacked, slices + floors made
- [x] GGUFs landed and size-verified: Qwen3.5-0.8B Q8 (0.78 GB), Qwen3.5-2B Q8 (1.87 GB), Qwen3-1.7B Q8 (1.83 GB), Qwen3.5-4B Q8 (4.48 GB, ceiling) via `tools/ranged-download.ps1` (hf stalls, BITS dies on token expiry). Gemma 4 E2B not fetched (G3 decided without it)
- [x] VoxCPM2 weights landed (model.safetensors 4.58 GB) and converted: `E:\models\VoxCPM2-GGUF\VoxCPM2-{BaseLM,Acoustic}-F16.gguf` (3.1 + 1.7 GB)
- [x] llama.cpp-omni `voxcpm2-cli.exe` built (CUDA; nvcc unsupported-compiler override), rebuilt with UTF-8 argv (narrow argv turned Hebrew/Japanese into '?')
- [ ] SAM Audio: gated, Michael has not requested access yet. G2 decided on BS-RoFormer ep368; SAM Audio enters as a G2 arm on F5 (and the G5 per-line prompt source) when the weights land. Design: separation runs ONCE upstream on the worker, stem -> whisper + prompt, residual -> the bed
- [x] F4: Sintel 1080p (1.1 GB, CC-BY) at `E:\datasets\sintel`; F4/F5 windows cut (fixtures/sintel_windows.py)
- [x] F5: F2 speech over the dialogue-free Sintel segment at 630.8 s

## Gates
- [x] G3 translation, DECIDED (21:56): 4B ceilings banked on all 7 pairs; `g3/verdict.py` applied the tree. 2B = 80-90% of ceiling into English (passes RU 88 / DE 87 / FR 90, ZH 85 on the line; JA 80 / KO 82 = Stage 2 fine-tune REQUIRED); 0.8B passes nowhere (65-81%); EN->HE only the 4B (2B = 67%). All medians <= 0.25 s. Qwen3.5-2B is the Stage-1 translator. Qwen3-1.7B banked and RETIRED (56-81% of ceiling, below the 2B everywhere, copies German through on 5% of lines)
- [x] G4 prep: 52 Sintel DE/FR cues translated (2B, 0.8B); original speech seconds per window (`fit.py speech`, VAD on the center channel)
- [x] G1 TTS, DECIDED (22:26): arm (b) ggml CUDA PASSES (RTF 0.27 EN / 0.30 HE, idle GPU, paired lines); arm (a) PyTorch 0.53 (fail by 0.03, TF32 unvaried); clone = plain + a fixed 0.82 s per call (reference encode: cache it per speaker in the product). Branch: weights-only packs, inference compiled in. Open: clone by ear on F4 (Michael); (c) Vulkan needs the SDK
- [x] G2 separation on F5: control ~0 dB; BS-RoFormer ov2 +14.9/+14.9 dB @ RTF 0.18 (PASS both); htdemucs +9.9/+9.4 @ 0.08; htdemucs_ft +11.4/+9.9 @ 0.23. Winner: BS-RoFormer ep368 overlap 2. Open: F4 by ear (Michael); SAM Audio when access granted
- [x] G4 duration fit, DECIDED (22:28): FAIL on both arms (CLI 34.6%, PyTorch 42.3% within [0.7, 1.15]); misses are short dubs in padded windows, real next-cue overruns 4-6%, dub ~1.3x the actor's speech; 1 CLI runaway (200-step cap = 32 s), PyTorch's retry caught it. Branch: length-budget arm derived from the room to the next cue, G3 re-scored under it. Product requirement: runaway guard on the ggml path
- [~] G0 (pre-registered 23:05): F6 = Tomb Raider King S01E09 from the Rillio cache (JA audio, official EN/DE/FR/RU/ZH subs, 282 spoken cues) at `E:\datasets\g0`. Product whisper path re-implemented 1:1 on pywhispercpp + the app's `ggml-small-q5_1.bin` (`g0/transcribe.py`, product vs native construction); per-cue alignment + G3 prompt + chrF++ vs official EN (`g0/score.py`); arms 2B/4B on product ASR, 2B on native ASR, German-track text-only control, copy floor. DECIDED (23:19): PASS both lines (2B = 86.1% of the 4B on the same ASR; product construction = 98.6% of native; 4.6% cues without ASR text). Raw 22.0 vs 36.1 for the German text-only control: the loss is the JA pair, not the chunking. Next ASR arm: a bigger whisper (download to price)

## Handoff
- [x] Decision-tree branches applied mechanically, written under each gate (VERDICT bullets) and in the ledger
- [x] Memory `ai-dubbing-narration-parked.md` updated with all four verdicts + the traps (BITS claim corrected in place)
