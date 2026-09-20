---
id: dubbing-stage1
tags: [dubbing, ai, tts, translation, audio, player, shell, high]
related_files: [docs/dubbing/stage1-pipeline/README.md]
status: in-progress
last_sync: 2026-09-13
---

Feature: AI dubbing (Stage 1)

AI dubbing
├── Sidecars
│   ├── voxcpm2-server (C++, llama.cpp-omni tree)
│   │   ├── stdio JSON-lines loop (load once, cmd dispatch) ✓ atomic
│   │   ├── reference cache (encode once per id, hold latents) ✓ atomic
│   │   ├── say: prompt (wav+text) and/or reference id -> PCM s16 48 kHz out ✓ atomic
│   │   └── runaway guard: abort past 3x the budgeted duration ✓ atomic
│   ├── llama-server (bundled binary, 2B GGUF, port from the shell) ✓ atomic
│   └── Rust sidecar supervisor (spawn, health, restart, kill on exit) ✓ atomic
├── Separation
│   ├── BS-RoFormer ONNX export + parity test on F5 (S2) ✓ atomic
│   ├── ORT session in the shell (CUDA EP, fallback CPU with a loud warning) ✓ atomic
│   └── window separate: 30 s in -> dialogue stem + residual out ✓ atomic
├── Dub worker (Rust, mirrors transcribe.rs)
│   ├── job state: generation, done windows, frontier, retry_after ✓ atomic
│   ├── next window: ahead of playhead then backfill ✓ atomic
│   ├── window decode: decode_pcm at 48 kHz stereo (the mix bed) + 16 kHz mono (ASR, prompt) ✓ atomic
│   ├── transcript: reuse the generated-subtitles cache or run whisper ✓ atomic
│   ├── translate: G3 prompt + context + length budget (D5) via llama-server ✓ atomic
│   ├── synthesize: per line, prompt slice + text (D4), fallback rules ✓ atomic
│   ├── place + fit: start at line start, atempo <= 15%, cut + fade past it ✓ atomic
│   ├── mix: residual + voice, ducking, clip guard ✓ atomic
│   └── persist: wav header patch + json map, atomic writes, resume ✓ atomic
├── Playback
│   ├── rillio-dub:// scheme: parse (own function), register, provider ✓ atomic
│   ├── blocking reads at the frontier; seek into unproduced regions re-targets the worker ✓ atomic
│   ├── mpv allowlist: audio-add / audio-remove / aid for the dub track only ✓ atomic
│   └── shell commands: dub_start, dub_stop, dub_select, status events ✓ atomic
├── Web UI
│   ├── Audio menu row "AI dub (English)" with states: not installed / installing % / generating % / ready ✓ atomic
│   ├── useDub hook (events, start/stop/select, stream-change reset) ✓ atomic
│   └── Settings: AI dubbing pack switch with size, progress, remove ✓ atomic
├── Packs
│   ├── manifest baked per release (file, url, sha256, bytes, version) ✓ atomic
│   ├── ranged parallel downloader with resume + hash verify (Rust) ✓ atomic
│   └── install / verify / remove commands + status events ✓ atomic
└── Verification
    ├── S1 external audio over the custom stream (dev shell, CDP) ✓ atomic
    ├── S3 resident server RTF on F1 with cloning ✓ atomic
    ├── S4 = G5 emotion transfer on F6 ✓ atomic
    ├── S5 offline dub of F6 to a file, pipeline RTF, by ear ✓ atomic
    ├── S6 in-player run ✓ atomic
    └── S7 pack install + update on a clean profile ✓ atomic

Atomic Units (order of execution = the seam order in README):
1. voxcpm2-server loop, reference cache, say, runaway guard (S3)
2. S1 probe: scheme + provider + audio-add in the dev shell
3. S2: ONNX export + parity + ORT session + window separate
4. G5 arms and instruments (S4)
5. Dub worker: state, next window, decode, transcript, translate, synthesize, place + fit, mix, persist
6. S5 offline run on F6
7. Playback commands + allowlist + UI row + hook
8. S6 in-player run
9. Packs: manifest, downloader, install/remove, Settings switch
10. S7 clean-profile install + update
