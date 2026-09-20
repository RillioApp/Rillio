---
id: dubbing
tags: [dubbing, player, ai, audio, tts, translation, high]
related_files: [apps/desktop/src-tauri/src/transcribe.rs, apps/desktop/src-tauri/src/autosync.rs, apps/web/src/routes/Player/useSubtitles.ts]
status: in-progress
last_sync: 2026-09-14
---

# Dubbing (real-time AI translation + cloned-voice speech)

The front door for everything about replacing a stream's dialogue with a
locally generated, translated, cloned-voice track. Started 2026-09-13, right
after AI subtitles (whisper.cpp, v0.1.44) shipped; this reuses that arc's
shapes: the ahead-of-playhead worker, the shadow-mpv PCM decode, and the
per-stream on-disk cache.

## Goal (Michael, 2026-09-13)

1. **Now:** dubbing = real-time translation with a local ~1B LLM, spoken in the
   original speaker's cloned voice, over the untouched music and effects.
   Real-time (ahead of the playhead), and saved to a file so nothing is
   processed twice.
2. **Later:** same-language voice replacement; MuseTalk lipsync.
3. **Packaging:** an optional install ("addon") that updates automatically
   with the app: weights-only feature packs pinned per app release (see
   `packs` once it exists). Inference code compiles into the app, as
   whisper.cpp does.
4. **Model:** newest ~1B (Qwen / Kimi families preferred), improved by
   fine-tuning on distilled movie-subtitle translations from a big teacher
   (Kimi K3 class, API only) and/or human-translated subtitle corpora.

## The five measurands (each with a pretrained part, each cached per stream)

| # | Measurand | Part | Cache artifact |
|---|---|---|---|
| 1 | what is said, when | whisper track / subtitles (done) | `generated-subtitles/<key>.json` |
| 2 | what it means in the target language | ~1B LLM (gate G3) | translated lines per language |
| 3 | how the speaker would say it | VoxCPM2 zero-shot cloning, prompt = separated stem (G1) | dub speech per language |
| 4 | everything that is not the dialogue | SAM Audio residual or BS-RoFormer stem (G2) | background stem, shared across languages |
| 5 | the track the player plays | mix (3)+(4), external audio track via the streaming server | dub track per language |

Cross-cutting: **duration fit** (a dubbed line must land in its cue window; G4).

## Status

- `stage0-gates/` - DECIDED 2026-09-13: G1 TTS PASS (ggml CUDA VoxCPM2,
  RTF 0.27-0.30), G2 separation PASS (BS-RoFormer ep368, +14.9 dB at RTF
  0.18), G3 translation = Qwen3.5-2B (85-90% of the 4B ceiling on RU/DE/FR/ZH;
  JA/KO need Stage 2 first), G4 duration FAIL (35-42% in window) -> the
  translator gets a length budget from the next-cue gap. Ledger, handoff and
  the product requirements the gates produced are in its README.
- `stage1-pipeline/` - IN PROGRESS (2026-09-13): the shipping pipeline. Two
  GPU sidecars (llama-server for the 2B, a resident `voxcpm2-server`),
  separation on ONNX Runtime, one ahead-of-playhead worker writing a
  replacement audio track served to mpv through `rillio-dub://`, per-line
  voice + emotion by prompt continuation (gate G5), weights-only opt-in
  packs. Seven seams verified in order before UI work.
- `stage1-pipeline/` - BUILT and run live on Michael's machine (2026-09-14),
  quality REJECTED ("didn't fix the issues, maybe worse"). The plumbing
  (sidecars, packs, `rillio-dub://` timeline, hold/resume, Audio-menu row,
  remembered choice, subtitles from the dub) stays; the heuristic
  fit/mix/turn/per-turn-ASR stack is what Stage 2 replaces. Nothing committed.
- Stage 2 (model-based) - MOVED OUT (2026-09-14) to the private engine repo
  `F:\Projects\Code\dub-engine` (GitHub pek100/dub-engine): prior-art
  surveys, the synthesis, gates G6-G12, the teacher bake-off harness and its
  ledger. Rillio keeps the shell plumbing and will call the engine over a
  local HTTP API (contract in that repo's README). Headline of the research:
  nobody stretches audio (duration goes INTO the TTS), length is budgeted in
  syllables not chars, separation is a dialogue-first cascade with a
  DnR-trained second stage, ASR is scene-level, the mix sets dialogue level
  and never touches the M&E.
- The translator fine-tune (OPUS + a teacher) is folded into Stage 2 gate G7
  (syllable-budget translation; GRPO if prompting fails).

## Index

- [stage0-gates/README.md](stage0-gates/README.md) - the gates, their instruments, the ledger.
- [stage1-pipeline/README.md](stage1-pipeline/README.md) - the built pipeline, its ledger, the pivot handoff.
- Stage 2 and onwards: the private engine repo (`F:\Projects\Code\dub-engine`, `docs/stage2-model-based.md` there).
- Checklists: `checklists/dubbing-stage0.md`, `checklists/dubbing-stage1.md` (Stage 2's is in the engine repo).
- Memory (cross-session): `ai-dubbing-narration-parked.md` (facts + decisions).
