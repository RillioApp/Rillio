---
id: dubbing-stage1
tags: [dubbing, ai, tts, translation, audio, player, shell, high]
related_files: [docs/dubbing/stage1-pipeline/README.md, docs/dubbing/stage1-pipeline/decomposition.md]
status: in-progress
last_sync: 2026-09-14
---

# Dubbing Stage 1 - checklist

Evidence ledger: an item is done only on observed output. Seams are gates
(instrument + pass line in the README); a seam that fails takes its
pre-written branch.

## Seams (in order)
- [~] S3 resident TTS server: `llama-tts-server` exists in llama.cpp-omni (CUDA + Vulkan builds under `E:\src\llama.cpp-omni\build-cuda\bin` and `build-vulkan\bin`); warm per-call split measured (encode 0.38 s + reference prefill 0.4 s); cache design deferred: (pc) per-line reference makes a per-speaker prefix moot
- [x] S1 external audio over `rillio-dub://` in the dev shell: PASS (00:23) on all four lines (aid in 9 ms, first audio 776 ms, seek into unproduced waits and resumes, seek into produced plays, aid back). With a worker at RTF 0.5 (00:38): read-ahead blocking does not stall playback; a seek deadlocked until reads were made interruptible on `time-pos` and the reader's window produced first; seek resume = target window cost (+ one hitch until the lead grows); mpv's paused-for-cache does not reflect dub waits (UI takes the dub status)
- [~] S2 BS-RoFormer ONNX: parity PASS on CPU EP (0.000 dB delta on both stems, opset 18, STFT outside the graph); exact but slow on every provider: CPU 6.9, DirectML 2.19, CUDA EP 1.65 (PyTorch CUDA 0.18, line 0.2). PASS after the construction sweep: fused MultiHeadAttention + fp16 = RTF 0.123 on CUDA EP at 14.892 / 14.942 dB (root cause: 1.3 GB unfused attention scores per chunk, allocation-bound). DirectML on the unfused fp16 file: RTF 0.199 at 14.891 / 14.941 dB, PASS. DECIDED D1c: separator ships on DirectML; the pack is fully CUDA-free. Next: the Rust port (`ort` crate, DML EP, STFT/iSTFT in Rust) with parity to the Python stems
- [~] S4 = G5, seven arms scored: (pc) line reference English 0.98 / faithful 0.89 / emotion 0.54 / speaker 0.73; (pcs) speaker-turn reference 0.99 / 0.88 / 0.47 / 0.70 (instrument favours pc by construction; his ear on rows 01, 07, 10 decides); (pcsv) + verified retry 0.99 / 0.94 / 0.47 / 0.70 at 1.04 s median per line; (pcw) windowed reference 1.00 / 0.87 / 0.45 / 0.68; (p) prompt 0.60 / 0.65; (pt) and (pw) RETIRED (runaways, garbage). Page published with pcsv, pcs, pc, pcw, r; Michael's second listen pending; D3a (turn-based chunking) and D4a (one separated voice per line) in the design
- [x] ASR sidecar (D1a): whisper.cpp v1.9.4 CUDA + Vulkan servers built (`E:\src\whisper.cpp\build-cuda\bin`, `build-vulkan\bin`); F6 greedy RTF: CUDA 0.039, Vulkan 0.014 (3x faster). G1(c) VoxCPM2 Vulkan: plain 0.29 / 0.29 (= CUDA), clone 0.47 / 0.56. Translator on Vulkan: chrF++ identical, 81 ms median (CUDA 135). DECIDED D1b: one Vulkan pack, CUDA builds stay bench tools
- [~] S5 offline dub: first sample rendered (F6 90-270 s, `s5/dub_offline.py`, speaker-turn reference + verified retry): 33 lines, 31 fit / 2 stretched / 0 cut, RTF 0.47 contended (compute alone: MT 3 s + TTS 24 s for 180 s); MP3 pair sent; Michael's ear GO/NO-GO pending; full-episode run and an idle-GPU RTF still owed
- [x] S6 in-player run on the dev shell: PASS on the release shell (running in 7.7 s; windows 13-36 s per 30 s; seek into unproduced held then resumed; aid back; no sidecar survives a forced kill). Michael's own run (real profile via explorer.exe) found the head-of-file freeze (fixed: non-blocking probe region, worker off the player lock) and the flow: the track is now selected AT THE PICK and the player buffers like a cache; subtitles yield to the dub. Owed: re-verification of the new flow (driver: hold at pick, first audio, sync), his click, the Settings pack block
- [~] S7 pack install + update on a clean profile: adopt-by-hash over the staged 7.85 GB verified in the shell (`pack_status` installed, 34/34 verified); a real download still needs the hosting url

- [ ] G2b two-speaker split (D4a): F7 fixture (two F2/F1 lines mixed at 0 dB), MossFormer2_SS_16K, SI-SDRi per speaker after assignment, RTF on flagged windows only; overlap detector calibrated on it

## Build (each traces to a decomposition leaf)
- [~] Sidecars: `sidecar.rs` supervisor (free-port allocation, spawn with per-sidecar logs, health contract per kind, check/restart/stop, Windows job object kill-on-close; 7 tests, no GPU) merged; the three Vulkan servers exist as binaries and are in the manifest (D6a: 26 flat entries, no zip needed since the static servers import none of llama-server's DLLs; staged at `E:\packs\dubbing\1`); `dubclients.rs` (translator with the G3 prompt, TTS with reference, ASR, verified take) passes 5 unit tests and a LIVE round trip on the resident servers; still to do: wire the supervisor into the dub worker, runaway guard on the TTS path
- [x] Separation: ONNX export + parity test (done); Rust `separate.rs` on ort DirectML merged: 14.891 / 14.941 dB exact, release RTF 0.206 (free-dimension-override landmine banked)
- [x] Turns: `turns.rs` merged (GE2E ONNX export min cosine 1.000000; 344/344 turns within 40 ms of turns.py, 58 ids; release RTF 0.013; VAD mode 3 trap banked)
- [~] Dub worker: `dubpipe.rs` runs END TO END on the staged pack (F6 windows 3-4: 17 lines, sidecars on Vulkan, separator on DirectML, turns in Rust; debug RTF 2.47 dominated by runaway screams, now guarded by `max_steps` = 3x room; non-speech turns keep the original voice); wired into `dub.rs` (resident pipeline on the state, "preparing" status, NotReady vs Failed, pass-through as `RILLIO_DUB_PASSTHROUGH`); transcript reuse DEFERRED (D3b); OPEN: release-build RTF, persist + resume of the window map
- [~] Playback: rillio-dub:// parse/register/provider, blocking frontier, seek interrupt, shell commands + events (done); allowlist untouched (audio-add is shell-issued)
- [~] Web UI: Audio menu row + states and `useDub` hook written (web build green, tsc clean on the new files); the row is the opt-in (size shown before download); Settings block for pack status/removal still open; not yet exercised in the shell (S6)
- [~] Packs: ranged downloader with resume + hash (done, 6 tests); `packs.rs` with the baked manifest (34 files: weights incl. the fp16 separator and the GE2E encoder, ONNX Runtime + DirectML DLLs, 26 sidecar binaries; real sizes and sha256; `RILLIO_DUB_PACK_DIR` dev knob), state.json, `pack_status` / `pack_install` (progress events, adopt-by-hash, orphan pruning) / `pack_remove`, 8 tests, Android stubs; staged at `E:\packs\dubbing\1` (7.6 GB). OPEN: hosting urls (Michael: HF org vs GitHub release parts)

## Open (Michael)
- [ ] Weights hosting (HF org vs GitHub release parts)
- [ ] Opt-in surface first (Settings vs Audio menu prompt)
- [ ] By-ear: F4 stems, G1 clone wavs, the G5 listening page (5 arms), S5's episode
- [ ] No usable GPU: no dub, or a background-prepared one (G1's tree leaves this choice to him)
