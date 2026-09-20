---
id: dubbing-stage1
tags: [dubbing, ai, tts, translation, audio, player, shell, high]
related_files: [apps/desktop/src-tauri/src/transcribe.rs, apps/desktop/src-tauri/src/stream_cb.rs, apps/desktop/src-tauri/src/shell.rs, apps/desktop/src-tauri/src/autosync.rs, apps/web/src/routes/Player/AudioMenu/AudioMenu.tsx, docs/dubbing/stage0-gates/README.md]
status: in-progress
last_sync: 2026-09-13
---

# Stage 1: the dubbing pipeline (pretrained parts, ahead of playhead)

Built on the Stage-0 verdicts (2026-09-13, `../stage0-gates/README.md`): ASR =
the app's whisper (G0), separation = BS-RoFormer ep368 (G2), translation =
Qwen3.5-2B (G3; JA/KO ship only after Stage 2), TTS = VoxCPM2 through the
ggml runtime (G1), duration = a length budget from the room to the next cue
(G4). Goal: an English dub track for a playing stream, produced ahead of the
playhead on this class of GPU, saved to disk so it is never computed twice,
selectable in the Audio menu like any other track, shipped as an opt-in pack
that updates with the app.

## What plays

The dub is a full replacement audio track: the film's residual (everything
but dialogue, from the separator) plus the synthesized English voice, mixed
by the shell into one 48 kHz stereo timeline. mpv selects it as an audio
track (`aid`), so pause, seek, speed, delay and volume all keep working, and
the original track stays one click away. Nothing runs inside the WebView.

## Decisions (each states what it rejects and why)

- **D1 Process model: two GPU sidecars, not in-process linking.** The 2B runs
  on a bundled `llama-server` (exactly the G3 bench: bench/prod parity, R2).
  VoxCPM2 runs on `voxcpm2-server`, the existing CLI turned into a
  long-lived stdio JSON-lines process that keeps the model AND the encoded
  references resident (G1: 0.82 s per call is the reference encode; a
  resident process pays it once per speaker). Rejected: linking llama.cpp
  and llama.cpp-omni into the shell through bindgen (whisper-rs already
  costs a libclang on every build; two more C++ trees make the shell build
  hostile), and a Python sidecar (G1's tree only allows it if ggml fails,
  and ggml passed). Sidecars are crash-isolated and own their VRAM.
- **D1a ASR on the GPU as a third sidecar (2026-09-14, Michael: "make the
  product run on CUDA", with a non-CUDA fallback).** The dub pack's whisper
  runs on a bundled `whisper-server` (whisper.cpp v1.9.4, same engine and
  the same `ggml-small-q5_1.bin` as the app), CUDA build for NVIDIA and a
  Vulkan build for AMD/Intel, chosen at runtime like the other two
  sidecars. The in-app AI-subtitles path stays on CPU whisper-rs: it runs at
  RTF 0.17, works everywhere, and keeps CUDA out of the shell build and CI.
  Measured on F6 (23 min JA): CUDA greedy RTF 0.039 (total 53.8 s: decode
  43.8 s at 7 ms per token, launch-bound at batch 1; mel 0.9 s; encode 1.2 s),
  4.4x the CPU path; beam search (the CLI default) doubles it to 0.085 and
  is not the product setting. Fallback ladder: CUDA -> Vulkan -> CPU for
  ASR; TTS and the translator follow the same ladder minus CPU (real-time
  TTS on CPU is not viable, so a machine with no usable GPU is Michael's
  call: no dub, or a background-prepared one).
  Measured later the same day: Vulkan is a PEER, not a fallback. VoxCPM2 on
  Vulkan RTF 0.29 (CUDA 0.27-0.30); whisper on Vulkan RTF 0.014, three
  times faster than the CUDA build; the 2B translator on Vulkan at
  identical chrF++ and 81 ms median (CUDA 135 ms). D1b, decided: ONE
  Vulkan pack (llama-server, llama-tts-server, whisper-server, all Vulkan
  builds), no CUDA runtime DLLs to bundle (~300 MB), AMD and Intel covered
  by the same binaries; the fallback ladder collapses to Vulkan -> CPU (ASR
  only). CUDA builds remain bench tools.
- **D2 Playback: a `rillio-dub://<key>` stream provider with blocking reads.**
  A second `mpv_stream_cb_add_ro` scheme next to `rillio://`, serving the
  mixed timeline as a WAV stream; a read past the produced frontier BLOCKS
  until the worker gets there (mpv waits, no EOF). Rejected: `--audio-file`
  on a growing WAV (mpv hits EOF and stops), and an mpv filter graph (the
  shell would have to script lavfi against a moving file). The parser for
  the new scheme is its own function: `stream_cb::parse_url` stays exactly
  as strict as it is.
- **D3 One worker, one timeline, one file.** The dub worker mirrors
  `transcribe.rs`: ahead-of-playhead with backfill, per-generation
  cancellation, periodic atomic saves. Its unit is a 30 s WINDOW (whisper's
  native window; G0 showed the 10 s window buys latency, not quality, and
  the dub runs ahead so it can afford the latency): separate the window,
  transcribe it (the existing transcript cache is reused when present),
  translate each line with context and the length budget, synthesize each
  line, place it at the line's start, mix over the residual. Persisted as
  `<app data>/generated-dubs/<key>.wav` (s16 stereo 48 kHz, header patched
  on every save) + `<key>.json` (window map, per-line records, model
  versions). The cache key is `transcribe::cache_key` (one owner).
- **D4 Voices: per-line prompt continuation (G5), no speaker tracking.**
  Each line is synthesized with `prompt = the line's own slice of the
  dialogue stem + whisper's transcript`, continued in English. That carries
  the speaker, the emotion and the pace of the original line without
  diarization or a per-character reference. Fallback when the prompt is too
  short (< 0.8 s) or overlapped: the last good prompt from the same window,
  then voice design. G5 must PASS before this ships (pre-registered below).
  Rejected for v1: one designed English voice for everything (loses the
  cast), and diarization + per-speaker references (a whole subsystem, and
  G5 may make it unnecessary).
- **D4a One separated voice per line (added 2026-09-14 after Michael's
  listen: row 01 took the identity from the right voice and the emotion from
  a second voice that spoke inside the same cue window).** The dialogue stem
  is one mix of every voice, so a window with more than one speaker is
  SPLIT first (MossFormer2's two-speaker model on flagged windows, the
  candidate noted in Stage 0), the stream matching the cue is picked, and
  that single stream is the only thing whisper, the translator's source and
  the TTS prompt ever see. Translation, identity and emotion come from the
  same separated voice, never from the mixed stem. Windows with one voice
  skip the split. Overlap detection is its own instrument (the bench uses an
  uncalibrated embedding-spread heuristic; the product uses the splitter's
  own output energy), gated on the F7 two-speaker fixture (G2b).
- **D3a The line is a speaker turn, not a subtitle cue (Michael, 2026-09-14
  02:50: "we need better voice separation, and follow voice separation and
  activity for chunking").** Inside each 30 s window the worker first
  separates the dialogue stem, then segments it by activity and voice:
  VAD gives the speech runs, speaker embeddings cluster them into turns,
  and overlapped runs go through the two-speaker split (D4a). Every
  downstream step works on turns: whisper transcribes a turn, the translator
  gets a turn with its neighbours as context, the TTS reference is built
  from that speaker's own turns (up to 8 s, breath and silence trimmed), and
  the dub is placed at the turn's start. Subtitle cues, when present, are
  only a timing and text cross-check, never the unit. Consequence for the
  bench: G5 arm (pcs) is built on turns, and the F6 harness gains a
  turn-segmentation step that Michael's rows 04, 07 and 10 verify by ear.
- **D3b The worker's buffer (day 2 evening, written for `dubpipe.rs`).**
  Windows are produced from a ROLLING separated buffer, not separated one by
  one: the separator is chunked with overlap-add, so audio within one chunk
  step (4 s) of a span's edge is estimated from a single chunk; every span
  is decoded and separated with a 4 s margin on each side and the margins
  are discarded. The buffer reaches 17 s past the window's end (the longest
  turn, 12 s, plus the 5 s trailing room a line may run into) so every turn
  that STARTS in the window is complete, its room is known and its take
  lands in the bed; a turn starting in the next window is that window's.
  In steady state each window separates 30 + 8 s of new audio (RTF ~0.25
  of the separator's 0.2). A window that does not continue the buffer (a
  seek, a backfill) starts it fresh 12 s before the window and ALSO dubs the
  turns straddling the window's start, so the first line after a seek is
  spoken, not silent; the translator's context and the produced set reset
  with it. Turns are re-segmented over the whole buffer on each extension
  (the VAD grid is 20 ms and windows are multiples of it, so earlier turns
  come back identical); a turn is produced once, keyed by its absolute
  start. The source language is whisper's detection on the first buffer,
  pinned for the stream; an English source is a loud error, not a
  pass-through. Deferred, with the reason: reuse of the subtitle
  transcript (it is whisper on the MIX at 10 s windows on the CPU; the dub
  transcribes the separated dialogue per turn on the GPU sidecar, a better
  input at a tenth of the cost, so there is nothing worth reusing).
- **D5 Duration: budget the translator, stretch only the tail.** The prompt's
  length budget is `chars_per_second_en * room`, where `room` is the time to
  the next line's start (G4: the misses were padded windows, the overruns
  are what breaks a dub) and `chars_per_second_en` is measured from G1's F1
  wavs (VoxCPM2's own English rate). A line that still overruns by <= 15% is
  time-stretched (rubberband-free: plain resample-free `atempo` through the
  shell's mixer); > 15% is cut at the next line with a 60 ms fade and
  counted. Runaway guard: the server aborts generation past 3x the budgeted
  duration (G4 saw a 32 s line).
  Refined on the full-episode run (day 2, 18 cuts, all fast exchanges):
  room runs to the next turn of a DIFFERENT speaker (same-speaker turns may
  run together) plus 250 ms of tolerated overlap, and the cut fade is 120 ms.
  As built (`dubfit.rs`): the stretch is the shell's own WSOLA (no ffmpeg in
  the product), gated live at the full bound (WER 0.000 on a real take); the
  runaway guard is the client's: a take over 31 s is discarded and re-seeded.
- **D6 Packs: weights only, pinned per release, opt-in.** The shell bakes a
  manifest (file, url, sha256, bytes, pack version). The pack is installed
  from Settings ("AI dubbing", size shown before the download) into
  `<app data>/models/`, fetched with the ranged parallel downloader (HF caps
  a single connection at ~0.5 MB/s: Stage-0 trap) and hash-verified. A new
  app version with a changed manifest re-fetches only changed files.
  Sidecar binaries (revised in D6a, day 2): they travel in the pack too,
  each sha256-pinned by the manifest the signed app bakes in, because three
  Vulkan servers weigh ~250 MB and the feature is opt-in. Rejected:
  bundling weights in the installer (7 GB opt-in vs a 60 MB app), a
  separate updater for weights (the app already updates), and binaries in
  every install.
- **D7 Target language v1 = English.** Source language = whisper's
  detection. Every Stage-0 number is XX -> EN; nothing else is measured.
  JA and KO are gated on Stage 2 and stay behind the flag until it lands.

## The seams, in the order they get verified (each is a gate: instrument, pass line)

1. **S1 external audio over a custom stream.** mpv `audio-add rillio-dub://…`
   on a blocking provider, in the dev shell over CDP: the track appears,
   plays in sync with the video, survives a seek into produced and into
   unproduced regions (the latter waits instead of stopping), and `aid`
   switches back to the original. Pass: all four observed; latency from
   `audio-add` to first audio <= 1 s.
2. **S2 separation outside Python.** BS-RoFormer ep368 exported to ONNX and
   run on ONNX Runtime (CUDA EP) from the shell: SI-SDR on F5 within 0.5 dB
   of the PyTorch number (+14.9), RTF <= 0.2. Fail -> htdemucs ONNX (exists,
   +9.9 dB, 0.6 dB short) as the deliberate policy, banked with a banner.
3. **S3 `voxcpm2-server`.** Resident process: load once, `{"cmd":"ref",...}`
   encodes and caches a reference, `{"cmd":"say",...}` synthesizes with a
   cached reference and/or a per-line prompt, streams PCM back. Pass: per-line
   RTF on F1 equals G1's plain arm (0.27) WITH cloning, i.e. the 0.82 s is gone.
4. **S4 = G5 emotion transfer** (pre-registered below).
5. **S5 end-to-end offline dub of F6.** The whole worker over the Tomb Raider
   King episode to a file, no player: wall-clock RTF of the pipeline <= 0.7
   (leaves headroom for the player and the separator), and Michael's ear on
   the result (GO/NO-GO).
6. **S6 in-player.** Same over the running shell with the track selected.
7. **S7 pack install + update** on a clean profile.

## G5 - emotion and voice transfer by prompt continuation (pre-registered 2026-09-13 23:45)

- **Fixture:** F6's 282 cues; prompt = the cue's slice of the BS-RoFormer
  dialogue stem (16 kHz mono) + the product ASR text; target = arm (a)'s
  English from G0. 40-cue seed-fixed subset for the by-ear part.
- **Arms:** (p) prompt continuation only; (r) reference-only (a fixed 15 s
  reference cut from the same episode: the G4/G1 shape, the control); (pr)
  prompt + reference; (pd) prompt through VoxCPM's denoiser (knob arm).
  Run first (2026-09-13 23:45): (p) and (r) on the resident
  `llama-tts-server`. Not run yet, recorded before the numbers: (pr) because
  the ggml runtime's continuation path takes one audio input (the Python
  package combines both; the C++ runtime would need a combined prefill
  layout), and (pd) because the denoiser lives in the Python package only.
  Each enters only if (p) fails its line: (pr) on speaker similarity, (pd)
  on WER (bleed).
  Added after the first listen (2026-09-14 01:10, post-hoc, Michael's
  hypothesis: "the trouble is converting between languages, not the
  emotion; larger chunks would let the latent settle so the sentences
  transform correctly even when the grammar changes"): (pc) the line's own
  slice as a REFERENCE only, no continuation; (pw) windowed continuation,
  prompt = the 8 s of scene before the line with those cues' English as the
  prompt text; (pcw) windowed reference, the 8 s around the line. Added
  instruments for the failure he heard: content-faithful rate (WER <= 0.2)
  and a runaway count, with per-line heard text on the listening page.
- **Instruments:** (1) language of the output by whisper's detector and
  WER of whisper-small on the dub against the intended English (cross-lingual
  continuation must produce English); (2) emotion2vec+ base (funasr,
  categorical: 9 emotion probabilities) on the source slice vs the dub: mean
  cosine similarity of the two distributions and top-class agreement rate,
  per arm (amended 2026-09-13 23:58 before any number: emotion2vec outputs
  categories, not arousal/valence); (3) speaker similarity source-vs-dub by
  the Resemblyzer GE2E embedding (cosine, per arm); (4) Michael's ear on the 40.
- **Pass:** (p) or (pr): >= 95% English outputs, WER <= 1.5x arm (r)'s WER,
  emotion cosine >= 0.6 and >= (r)'s + 0.15 with top-class agreement >=
  (r)'s + 0.15, speaker similarity higher than (r)'s.
- **Decision tree:** pass -> D4 as written. Emotion passes but English rate
  fails -> prompt with a translated prompt text (English) and re-run (one
  knob). Emotion fails -> D4 falls back to reference-only per speaker and
  a diarization subsystem enters the plan; tags become the emotion route.

## Handoff (2026-09-14 02:30, Michael went to sleep; every process stopped)

**Where the campaign stands.** S1 PASS (playback seam, with the seek
interrupt and reader-first order; see ledger). S2: export exact, DirectML
RTF 2.19 fails, CUDA EP still owed. S3: the resident server exists
(`llama-tts-server`, CUDA and Vulkan builds), per-call cost measured; the
cache design is moot if (pc) wins (per-line reference, no reusable
prefix). G5: (pc) per-line reference is the leading candidate (English
0.98, faithful 0.89, emotion and speaker at the prompt arm's level); (p)
fails the language boundary; (pt) retired; (pw) and (pcw) synthesized
(`E:\datasets\g5\pw`, `pcw`) but NOT scored: score3 was stopped at 203/514
clips. Michael judges by ear on the listening page (artifact "Tomb Raider
King Dub Bench", arms pc / p / pw / pcw / r, heard text under the scored
arms, "voices" score per row). Downloader landed with tests. ASR sidecar:
whisper.cpp CUDA and Vulkan servers built; CUDA greedy RTF 0.039 on F6;
the Vulkan whisper run and G1(c) (VoxCPM2 Vulkan) were stopped mid-way:
`E:\datasets\g1\vulkan-timing.log` holds whisper Vulkan timings and the EN
lines of G1(c) (HE lines 0.42-0.87 RTF before the stop, slower than CUDA).

**Michael's verdicts landed (02:45, see ledger): line reference wins, its
failures are speaker-turn failures.** Next arms, in this order: (pcs)
speaker-turn reference: segment the stem into turns (VAD runs + speaker
embeddings clustered per scene; pyannote only if Michael grants the HF
gate), then reference = the target speaker's own turns nearest the line,
up to 8 s, breath and silence trimmed; (pc+retry) generate, transcribe back,
regenerate when WER > 0.2, keep the best of 3 (RTF 0.3 affords it). A
two-speaker cue becomes two lines with two references. Pass lines stay as
pre-registered plus his ear on rows 04, 06, 07, 10.

**Separation is two problems (Michael, 02:55: "the voices weren't always
perfectly separated, is it the models we chose?").** (a) Dialogue vs
everything else: BS-RoFormer is a MUSIC separator (vocals vs accompaniment);
effects are not music, so breaths, cloth and impacts leak into the vocal
stem, and G2's F5 fixture (speech over a music bed, no effects) could not
show it. G2 follow-up, pre-register before running: F5b = the same speech
over music PLUS CC0 effects, same SI-SDR instrument; arms BS-RoFormer,
Bandit (Sony's open cinematic three-stem separator trained on DnR:
dialogue/music/effects, no gate), MossFormer2 speech enhancement as a
post-clean, and then the fine-tune arm Michael asked for (03:00, "the
current model is almost perfect, maybe with a better fine tuning it could
improve"): BS-RoFormer ep368 domain-adapted from its checkpoint with the
open training code, on DnR plus anime-shaped synthetic mixes (open Japanese
speech corpora over CC0 effects and music, ground truth by construction),
a few epochs on this GPU; judged on F5b AND on F5 again so a music-separation
regression cannot hide. SAM Audio is off the list: Michael does not want to
register, and nothing depends on it. (b) Voice vs voice: no separator does
it; D3a's turn segmentation plus the two-speaker split (D4a/G2b) own it.

**Resume in this order.** (1) Read Michael's verdicts (he replies on the
page). (2) Finish scoring pw/pcw: `run.py score E:\datasets\g5 --arm pw
--arm pcw` (needs no server), then `summarize.py`, rebuild the page. (3)
Apply G5's tree with (pc) or a windowed arm as D4's mechanism; rewrite D4.
(4) Re-run the Vulkan timing chain (`scratchpad/vulkan-timing.cmd` logic:
whisper Vulkan greedy on F6, then `g1/time_voxcpm_cli.ps1 -Cli
...build-vulkan\bin\voxcpm2-cli.exe`) on an idle GPU for G1(c). (5) S2 CUDA
EP: a CUDA-12 `onnxruntime-gpu` in the venv, `parity.py --onnx-only
--provider CUDAExecutionProvider`. (6) Then the dub worker's real form
(separation, whisper server, translator, TTS per line with the chosen
mechanism, mix) and S5 on F6.

**Ops that bit on day 2.** An ONNX Runtime CUDA session (`onnxruntime-gpu`
1.22, CUDA 12) started while the TTS server was synthesizing took the GPU
to 23.9 of 24 GB and 100%: the arena allocator grabs what it can and every
other GPU tenant thrashes (one dub line went from 0.8 s to 44 s). Rule: the
S2 CUDA timing runs on an IDLE GPU only, and the product's separator
session must cap its arena. Kill by command line, never by image name,
when scorers and synthesizers share `python.exe`.

**Ops that bit tonight.** The LunarG installer cannot elevate from a
command line (Michael ran it as admin: `C:\VulkanSDK\1.4.357.0`); Qt IFW
flags `--accept-messages` and `--default-answer` are mutually exclusive.
Compiles starve the CPU scorers (never run both). `llama-tts-server` needs
`--voxcpm2-base-lm/--voxcpm2-acoustic` on its command line. Scorer: emotion2vec
now on CUDA; whisper still CPU (pywhispercpp), the CUDA server is the
replacement when the harness moves to it. The dub track is a pass-through
bed until the worker's real form lands (Michael heard the original audio and
asked; expected).

## Handoff (2026-09-14 ~21:00, end of the first live session): PIVOT

Michael's verdict after an evening of listening on his own machine: "not
perfect, it didn't fix the previous issues and I think even made it worse".
His decision: switch the approach, less heuristic and more model-based, and
research prior art and related research before building further. The shell
plumbing (timeline, sidecars, packs, playback hold) is sound and stays; the
fit/mix/turn heuristics (isochrony by stretching, ducking, residual swap,
VAD turns, per-turn ASR) are the part to replace with models where models
exist. His defect list and design asks are in the memory file's "RESUME
HERE" block and in "Michael's direction" above. Research targets: expressive
speech-to-speech translation (SeamlessExpressive), isochrony-aware MT and
duration-controlled TTS (prosodic alignment work, VideoDubber, StyleDubber /
HPMDubbing), end-to-end dubbing products (ElevenLabs Dubbing, YouTube Aloud,
Deepdub, Papercup), cinematic dialogue separation (Bandit, DnR), long-context
ASR+MT for dialogue. The previous handoff (below) still describes the code.

## Handoff (2026-09-14 evening, day 2)

Where it stands: the dub runs END TO END in the release shell (S6 PASS,
ledger below): pack sidecars on Vulkan, separator on DirectML, turns in
Rust, the timeline over `rillio-dub://`, the Audio menu row. Nothing is
committed. Resume order:

1. Michael's ear: `E:\datasets\s5\pipeline\shell-dub-0-210.mp3` (the shell's
   own output) and the stretch pair `take-plain` / `take-stretched-1.15`.
2. Michael's click: launch a dev shell with `RILLIO_DUB_PACK_DIR=E:\packs\dubbing\1`
   (his own launch, outside the container), play F6, Audio menu -> "AI dub
   (English)": the row should read "Preparing, N s ready" and switch on its
   own once a window is ahead. CDP cannot click it (`s6/probe-input.js`).
3. RTF: steady state 0.45-1.2 per window in the shell (lines dominate).
   Levers in order: pipeline the next window's separation under this
   window's synthesis; cache the encoded reference per speaker on the TTS
   server (S3: it is a KV prefix); shorter references (8 -> 4 s, gate it).
4. Packs: hosting url (Michael), then S7 on a clean profile with a real
   download; the Settings block (status, size, remove).
5. Persist + resume of the window map (`<key>.json`), so a re-opened stream
   does not re-dub what exists.
6. G2 follow-ups (Bandit, F5b, fine-tune) and G2b (two-speaker split, D4a).

Harness: `s6/launch-dev-shell.ps1 [-Release]`, `s6/s6-driver.js <url>`,
`s1/range-server.js <file> 8766`, `s6/eval.js "<expr>"`, `s6/job-probe.ps1`.
Ignored live tests (need the staged pack / servers): `dubpipe::tests::live_f6_windows`
(`RILLIO_LIBMPV` to the debug dll for `--release`), `dubclients::tests::live_round_trip`,
`dubfit::tests::live_stretched_take_stays_faithful`, `separate::tests::f5_parity_*`,
`turns::tests::*` parity.

## Michael's direction from the first live session (2026-09-14 night), in order

1. **Per-speech frontier, no windows.** "Why a hard-coded window? Shouldn't it
   be dynamic per speech?" The unit the player waits on becomes the dubbed
   line: the timeline advances a frontier as each line lands (intervals, not
   window flags), a seek starts a new island. First English seconds after the
   pick; the buffer's separation span shrinks to what the next lines need.
2. **Original audio outside lines.** "The music sounds fucked up": the bed is
   the separator's residual everywhere. Keep the ORIGINAL mix wherever no line
   is dubbed and crossfade to residual + English only inside a line, so the
   separator's artifacts sit under the English voice that masks them.
3. **Envelope transfer.** "The vocals are very dynamic": shape each take's
   loudness along the original line's energy contour (bounded), the dynamics
   become the actor's while the voice stays the synthesizer's. Pitch contour
   later if needed. (YuE2 was considered and rejected: a song generator, no
   reference voice, no separation.)
4. **Actor dictionary.** Persistent per-title / per-series speaker entries keyed
   by embedding, holding curated clean turns; the reference comes from the
   best of the pool (steadier voices, and a cacheable encoded reference on the
   TTS server = the fixed-cost lever). Then per-actor LoRA fine-tuning in the
   background from the same curated pool (Stage 2: needs a training path).
5. Separator on its own thread; then the separator itself (Bandit, fine-tune).
6. Music under dialogue (Stage 2 arms, from Michael's YuE2 pointer): regenerate
   the accompaniment under a line from a music prior conditioned on the
   surrounding seconds (inpainting the separator's hole), and a music codec
   pass over the residual as a cheaper cleaner; gate = an artifact score under
   dialogue, paired against the plain residual.

Later the same night, from his listening: (a) the isochrony SLOW-DOWN ran
on nearly every line (English takes are shorter than Japanese lines) and
the stretcher sounded "synthetic and chopped": slow-down OFF, speed-up
bound 1.10; (b) the residual swap covered only the English take, so the
original Japanese voice came back for the rest of each line: the swap now
covers the whole original line; (c) "relics" heard as "aliens": the Japanese
homophone 遺物 / 異物 misread by whisper on a short turn: the title's name
and synopsis now go to whisper as its initial prompt and to the translator
("The lines are from: ..."), and the translator is the 4B (Stage 0: the 2B
is 80 % of it on JA; latency a quarter second); the 2B stays in the pack;
(d) the hold moved off mpv's stream thread (a pause command from inside a
read callback deadlocked the player) onto the ticker, keyed on the PLAYHEAD
sitting in unmade audio, not on the reader parking (mpv's read-ahead parks
at the frontier constantly); the worker targets the playhead's gap, not the
read-ahead's; (e) a resumed timeline is verified against the file (a saved
map over a zeroed file played 20 minutes of silence: cause not found, the
probe makes it self-heal); (f) the AI subtitles show the dub's English lines.

Done tonight toward the list: item 2 (`swap_to_residual`: the bed is the
original mix, residual only inside a dubbed line with 40 ms crossfades;
"keep original" is now a no-op), the playback hold with a 20 s resume
watermark, isochrony (budget = utterance length, edge trim, slow-down up to
15 %), -8 dB ducking under lines, translator failure loud.

## Open questions for Michael (not blocking the seams)

- Weights hosting: a Rillio Hugging Face org (free, large files, slow single
  connections, hence the ranged downloader) vs GitHub release assets (2 GB
  per file, so VoxCPM2 F16 must ship as parts or as Q8).
- Opt-in surface: the Settings page (size shown, one switch) vs an install
  prompt from the Audio menu row itself. Both can exist; which is first.
- v1 scope: EN only (assumed). RU -> EN is the only non-CJK pair he named
  besides DE/FR; all are covered by the same path.

## Ledger (bank as you go)

| when | seam | arm | result | where |
|---|---|---|---|---|
| 2026-09-13 23:27 | S3 (measure) | `voxcpm2-cli` cold clone call, per-stage timers added to `generate_with_clone` | reference encode 0.46 s, prefill 0.73 s over 110 positions, decode 0.77 s, VAE 0.10 s for 2.9 s of audio. | `E:\datasets\g1\smoke\clone-timing.wav` |
| 2026-09-13 23:31 | S3 (measure) | `llama-tts-server` (exists in the tree: `tools/server/server-voxcpm2.cpp`, target `llama-tts-server`), 4 warm clone calls, same 15 s reference | encode 0.38-0.48 s, prefill 0.37-0.46 s (101-112 positions), decode 0.28-0.73 s, VAE 0.06-0.13 s; wall 1.1-1.8 s per line. The fixed cost is HALF reference encode, HALF the reference's share of prefill (~4 ms per position). Layout is [ref_audio_start][reference frames][ref_audio_end][text][audio_start]: the reference is a PREFIX, so a KV snapshot after it is structurally possible. Cache design is deferred until G5 says whether per-line prompts (no reusable reference) or a per-speaker reference wins. | `E:\datasets\g1\server\server.err`, `g1/tts_server_client.py` |
| 2026-09-13 23:41 | S2 (prep) | BS-RoFormer ep368 ov2 on F6's full 48 kHz mix (PyTorch, `audio-separator`, TTS server resident but idle) | 23 min in 4:58 = RTF 0.216. Stems for the G5 prompts and the S5 bed. | `E:\datasets\g0\stem\` |
| 2026-09-14 00:25 | S2 export | BS-RoFormer ep368 -> ONNX (opset 18, STFT/iSTFT outside the graph, mask multiply as real products, rotary cache disabled for export), parity on F5 CPU EP vs PyTorch fp32 | vocals 14.888 / residual 14.937 dB on BOTH arms, delta 0.000 dB, max abs diff 1.15e-5: PASS the parity line (0.5 dB) with margin; also reproduces G2's CUDA numbers, pinning the fixture handling. CUDA-EP RTF still owed (GPU was busy). | `s2/README.md`, `s2/parity_result.json`, `E:\models\separator\bs_roformer_ep368.onnx` (645 MB) |
| 2026-09-14 00:50 | S2 timing, DirectML EP (`onnxruntime-directml` 1.24.4, `parity.py --onnx-only --provider DmlExecutionProvider`, GPU idle apart from the resident TTS server) | same chunking, scored against the truth stems | vocals 14.888 / residual 14.937 dB (exact, so the provider computes right) but RTF 2.19 for 60 s: ten times PyTorch CUDA's 0.18 and far off the 0.2 line. DirectML is not a shipping path for this graph as exported. Owed: CUDA EP (needs a CUDA-12 `onnxruntime-gpu`), then TensorRT or a leaner export if CUDA EP also misses. Parity was CPU 6.9 RTF, so DML did give a 3x, not the 30x a GPU should. | `s2/timing_DmlExecutionProvider.json` |
| 2026-09-14 00:39 | G5 arm (p) | prompt continuation: prompt = the cue's stem slice + product ASR text, target = G0's 2B English, resident `llama-tts-server`, 269 cues | ENGLISH RATE 0.599 (whisper hears 40% of the dubs as not English: cross-lingual continuation keeps slipping into Japanese); WER median 0.00 / mean 0.317 (the English ones are intelligible); emotion2vec cosine 0.526, top-class agreement 0.494; speaker cosine 0.769 to the source slice; median wall 1.13 s per line, RTF 0.30 with the short prompt (the per-speaker reference cache is moot on this path). Pre-registered line for English rate (>= 95%) FAILS; the tree's branch for "emotion passes, English fails" is an English prompt text, pending arm (r) for the emotion and speaker comparisons. Michael judges emotion and voice by ear on the listening page (24 seed-fixed cues). | `E:\datasets\g5\p.summary.json`, `g5/listening_page.py`, artifact "Tomb Raider King Dub Bench" |
| 2026-09-14 00:58 | G5 arm (r), the control | one fixed 15 s reference, no prompt, same 269 cues | English rate 1.00; WER median 0.00 / mean 0.088; emotion cosine 0.34, top agreement 0.316; speaker cosine 0.487 to the source slice; RTF 0.50 (the 15 s reference re-encoded per call). Against it, arm (p) clears every RELATIVE line: emotion +0.19 cosine (>= +0.15) and +0.18 agreement (>= +0.15), speaker similarity higher (0.77 vs 0.49). It misses the absolute emotion floor (0.53 < 0.6) and the English rate (0.60 < 0.95). Michael's ear on the listening page: "some are good, some are not, and it is the language conversion, not the emotion". Verdict so far: prompting with the line's own audio DOES carry voice and emotion; the defect is the language boundary in cross-lingual continuation. Knobs in flight: (pt) English prompt text (the tree's branch), (pc) per-line reference without continuation. | `E:\datasets\g5\r.summary.json` |
| 2026-09-14 01:48 | G5 arm (pt), RETIRED | English prompt text over the Japanese prompt audio (the tree's knob for a failed English rate) | English 0.51, content-faithful 0.07, 15 runaways, emotion 0.48, speaker 0.77. Cause of death: the text stream claims the Japanese audio said the English line, so the model is asked to continue speech it never heard and produces garbage or never stops. Michael: "stop the bad arms". | `E:\datasets\g5\pt.summary.json` |
| 2026-09-14 01:48 | G5 instruments added | content-faithful rate (WER <= 0.2, Michael heard sentences rewritten), runaway count, per-line heard text on the page, overlap heuristic (bottom tenth of speaker-embedding spread; flags 34/269, row 01 among them) with a clean-subset block per arm | Arm (p) on the clean subset: English 0.58, faithful 0.65, emotion 0.52, speaker 0.77: within noise of the full set, so on this heuristic the mixed-voice cues do not drive the averages; the language and grammar failures sit in single-voice cues too. | `g5/summarize.py`, `E:\datasets\g5\prompts.overlap.jsonl` |
| 2026-09-14 02:10 | ASR sidecar (D1a) | whisper.cpp v1.9.4 built from source with CUDA (`E:\src\whisper.cpp\build-cuda\bin\whisper-server.exe`, `whisper-cli.exe`; releases ship no Windows binaries any more), F6 full episode, `ggml-small-q5_1.bin`, 8 threads | greedy: total 53.8 s = RTF 0.039 (mel 0.88 s, encode 1.18 s / 49 windows, decode 43.8 s / 6303 tokens at 6.95 ms, 10 temperature fallbacks); beam search 5 (CLI default): 117.6 s = RTF 0.085. CPU product path was 0.166. Vulkan SDK 1.4.357 installed by Michael (the LunarG installer refuses to elevate from a command line); Vulkan builds of the TTS server and whisper server in progress. | `E:\datasets\g0\whisper-cuda\asr-cuda-greedy.json`, `E:\src\build-whisper-cuda.log` |
| 2026-09-14 02:12 | G5 arm (pc), per-line REFERENCE | the line's own stem slice as `reference_audio` with no prompt text (VoxCPM2's clone path, nothing continues Japanese audio), 269 cues | English 0.978 (PASS >= 0.95), content-faithful 0.885, 0 runaways, emotion cosine 0.537 / top agreement 0.509 (vs control 0.34 / 0.32: +0.20 / +0.19, PASS the relative lines), speaker cosine 0.727 (vs control 0.487, PASS; the prompt arm had 0.769). Clean (one-voice) subset: 0.974 / 0.877 / 0.528 / 0.723, same picture. Misses only the absolute emotion floor (0.6) on this categorical instrument, as every arm does. Leading candidate for D4: voice and emotion through the reference path, per line, no continuation. Per-line cost: a 1-3 s reference encode per call (no per-speaker cache needed). | `E:\datasets\g5\pc.summary.json`, `pc.scored.jsonl` |
| 2026-09-14 02:45 | G5 by ear (Michael, 5-arm page) | rows 01-24, seed 1 | "Line reference sounds the best most of the time; second place pcw, though the voice is not preserved correctly a lot of the time; pcw's intonation is often slightly better." Row 04 (pc): wrong voice, two voices in the slice, one only breathing, the model invented a third. Row 06 (pc): missed words. Row 07 (pc): consecutive voices merged into one; pcw's intonation better. Row 10 (pc): consecutive voices of different people mixed. READ: the subtitle CUE is the wrong unit for a reference. Cues hold consecutive turns of different speakers (and a second person's breath), so a per-cue slice is often two voices in sequence, not one; a longer window (pcw) buys prosody but leaks more voices. The reference must be built from SPEAKER TURNS: the target speaker's own speech only, and as long as that allows (pcw's prosody with pc's identity). Missed words are a content check the pipeline can verify and retry (ASR-back WER). | Michael, listening page |
| 2026-09-14 (day 2) | D3a instrument: speaker turns on F6 | `g5/turns.py`: WebRTC VAD runs (300 ms gap fill, 400 ms min), long runs cut at the quietest pause, within-run speaker-change cuts by sub-window embedding similarity, Resemblyzer GE2E embeddings clustered greedily per scene at cosine 0.75 | 344 turns, median 2.1 s, 58 speaker ids (over-split: GE2E on 1-2 s turns is noisy, and over-splitting is the safe direction: it shortens references, it never merges voices). Michael's rows mapped to cues: row 01 = cue 23, two turns of two speakers inside the cue (his "emotion from the other voice"); row 06 = cue 76, an 11.5 s run cut to 6.2 s; row 07 = cue 87, two turns the clustering still calls one speaker (the encoder cannot separate this pair, his ear can); row 04 = cue 51, row 10 = cue 111, single turns. Arm (pcs) = the cue's speaker's own turns nearest the cue as the reference, up to 8 s. | `E:\datasets\g5\turns.jsonl` |
| 2026-09-14 (day 2) | G5 arm (pw), RETIRED | windowed continuation: the 8 s before the line as prompt with those cues' English as text, 245 cues | English 0.437, content-faithful 0.045, 27 runaways, emotion 0.27, speaker 0.62. Cause of death: continuation over a multi-speaker window with mismatched text; worse than every other arm on every line. | `E:\datasets\g5\pw.summary.json` |
| 2026-09-14 (day 2) | G5 arm (pcw), windowed reference | the 8 s around the line as `reference_audio`, no prompt text, 269 cues | English 0.996, content-faithful 0.874, 0 runaways, emotion 0.453 / top 0.424, speaker 0.682. Against (pc) line reference: same language and content, but emotion -0.08 and speaker -0.05: the longer window leaks neighbouring voices, exactly Michael's "voice not preserved a lot of the time"; his "intonation slightly better" is not what emotion2vec measures. Read together with (pc): the reference should be LONG but SINGLE-SPEAKER, which is arm (pcs). | `E:\datasets\g5\pcw.summary.json` |
| 2026-09-14 (day 2) | G5 arm (pcs), speaker-turn reference | reference = the cue speaker's own turns (turns.py) nearest the cue, up to 8 s, no prompt text, 269 cues | English 0.989, content-faithful 0.881, 0 runaways, emotion 0.468 / top 0.442, speaker 0.703; clean subset the same. Below (pc) on emotion (-0.07) and speaker (-0.02). INSTRUMENT CAVEAT: both similarities are measured against the cue's own slice, which IS (pc)'s reference, so (pc) is favoured by construction; the honest comparison is Michael's ear on the two-speaker rows (01, 07, 10) where (pc) failed. Mechanism read: a speaker's other turns carry identity but a different emotional state, so the current line's slice must stay in the reference (it is, when the segmentation catches it) and the added context should be short. Candidate next knob if his ear prefers (pcs) on identity but (pc) on emotion: line slice + at most one adjacent same-speaker turn. | `E:\datasets\g5\pcs.summary.json`, page columns 1 vs 2 |
| 2026-09-14 (day 2) | G5 arm (pcsv), speaker-turn reference + verified retry | (pcs) reference; each line transcribed back by whisper-small, re-taken with a new seed when WER > 0.2, best of 3 | English 0.993, content-faithful 0.941 (from 0.881), 0 runaways, emotion 0.472 / top 0.446, speaker 0.703 (unchanged: the reference is the same). Cost: median 1.04 s per line, p90 1.98 s, i.e. most lines pass on the first take. CAVEAT: the take is selected by the same whisper that scores it, so part of the gain is whisper agreeing with itself; Michael's ear on row 06 (his "missed words") is the honest check. Product reading: verify-and-retry is cheap at RTF 0.3 and belongs in the worker regardless of which reference wins. | `E:\datasets\g5\pcsv.summary.json`, page column 1 |
| 2026-09-14 (day 2) | S2 timing, CUDA EP (`onnxruntime-gpu` 1.22 / CUDA 12, GPU idle apart from the resident TTS server) | `parity.py --onnx-only --provider CUDAExecutionProvider` on F5 | vocals 14.888 / residual 14.937 dB (exact) but RTF 1.646 for 60 s: 9x PyTorch CUDA's 0.18 and 8x the 0.2 line. With CPU 6.9 and DirectML 2.2, the export itself is the bottleneck (fp32 throughout, attention as unfused small ops, possible CPU-placed nodes, per-chunk shape re-planning). R6 before the fallback branch: an agent varies the construction (fp16, graph optimization level, IO binding, node placement log, static shapes) on an idle GPU, pass line unchanged. Ops rule from the same afternoon: an ORT CUDA session arena grabs the whole GPU; never alongside another GPU tenant, and cap `gpu_mem_limit` in the product. | `s2/timing_CUDAExecutionProvider.json` |
| 2026-09-14 (day 2) | G1 arm (c), VoxCPM2 on Vulkan (`build-vulkan\voxcpm2-cli.exe`, Vulkan SDK 1.4.357, same F1 lines x 3, idle GPU apart from the resident CUDA TTS server) | `g1/time_voxcpm_cli.ps1 -Cli ...build-vulkan...` plain and clone | plain RTF median EN 0.292 (max 0.316), HE 0.294 (max 0.382): PASS, equal to CUDA's 0.271 / 0.300 within noise. Clone EN 0.467, HE 0.559 (CUDA 0.511 / 0.631): the same fixed reference-encode cost. Vulkan is a full-fidelity peer of CUDA for the TTS on this GPU. (The summary's `arm` label still says cuda: the script hardcoded it; fixed after.) | `E:\datasets\g1\ggml-vulkan\summary.json`, `ggml-vulkan-clone\summary.json` |
| 2026-09-14 (day 2) | ASR on Vulkan (`whisper.cpp build-vulkan\whisper-cli.exe`, F6 full episode, greedy, 8 threads) | same run as the CUDA timing | total 18.8 s = RTF 0.0136: THREE TIMES FASTER than the CUDA build (53.8 s), decode 1.95 ms per token vs 6.95, encode 15 ms vs 24 ms per window. The CUDA build's per-token decode is launch-bound; ggml's Vulkan path batches it better on this driver. Product reading: with TTS at parity and ASR faster, a Vulkan-only pack (no ~300 MB CUDA runtime, AMD/Intel covered) is the simpler ship; the one number still owed for that call is the 2B translator's latency on the Vulkan llama-server (G3's harness has `-Backend vulkan`). | `E:\datasets\g1\vulkan-timing.log`, `E:\datasets\g0\whisper-vulkan\asr-vulkan-greedy.json` |
| 2026-09-14 (day 2) | S5 first end-to-end sample, F6 90-270 s, offline (`s5/dub_offline.py --reference pcs`, verified retry on) | turns from turns.py, whisper CUDA greedy transcript, 2B on llama-server CUDA with a room-derived length budget, VoxCPM2 on the CUDA TTS server, fit (atempo <= 15% / cut), level-matched to the original dialogue, mixed over the 48 kHz residual | 47 turns in the span, 33 with text; 31 fit their room, 2 stretched (x1.07), 0 cut; wall 85.1 s for 180 s = RTF 0.47 (CONTENDED: the S2 speed agent shared the GPU), of which translation 3.1 s. Two bugs on the way, both loud: the separator's stems are 44.1 kHz (resampled once to `residual48k.wav` / `vocals48k.wav`). Michael's ear on the MP3 pair (dub vs original, same span) is the GO/NO-GO. | `E:\datasets\s5\sample-90-270\{dub.wav,lines.jsonl,summary.json,dub-90-270.mp3,original-90-270.mp3}` |
| 2026-09-14 (day 2) | Translator on Vulkan (G3 harness, `run_arm.ps1 -Backend vulkan`, JA->EN, 500 lines, GPU shared with the S2 agent) | Qwen3.5-2B Q8 on the Vulkan llama-server b10944 | chrF++ 23.78 (CUDA 23.72: identical), median latency 0.081 s, p90 0.106 s (CUDA 0.135 / 0.235): faster. DECISION D1b: ONE pack, Vulkan builds only (llama-server, llama-tts-server, whisper-server): TTS at parity, ASR 3x faster, MT faster, AMD and Intel covered, no CUDA runtime to bundle. CUDA builds stay as bench tools. Fallback below Vulkan is CPU for ASR only. | `E:\datasets\opus\en-ja\g3\qwen3.5-2b-q8-vulkan.summary.txt` |
| 2026-09-14 (day 2) | Pack contents, runtime dependencies (dumpbin /DEPENDENTS) | the three Vulkan servers | `llama-tts-server.exe` and `whisper-server.exe` (both built static, BUILD_SHARED_LIBS=OFF) depend only on `vulkan-1.dll` (the system Vulkan loader, installed by every GPU driver) and `VCOMP140.DLL` (the OpenMP runtime: bundle it or build without OpenMP). The prebuilt `llama-server.exe` from the llama.cpp release is a 30-DLL set (impl + ggml-cpu variants + libomp). Tried building `llama-server` from the omni tree: the fork has no such target (it ships `llama-omni-server`, a different program the bench never ran). Decision: the pack carries the upstream prebuilt Vulkan `llama-server.exe` with its DLL set (b10944, the exact binary of every G3 arm: bench/prod parity, R2) plus the two static servers and `vcomp140.dll`. | `E:\src\build-llama-server-vulkan.log` |
| 2026-09-14 (day 2) | D6a, packaging of the sidecar binaries | sizes: llama-tts-server 64 MB, whisper-server 58 MB, llama-server + DLLs ~120 MB | Revises D6's "binaries ship inside the installer": ~250 MB in every install for an opt-in feature is the wrong default. The binaries travel IN the pack, downloaded on opt-in, each pinned by sha256 in the manifest the signed app bakes in (integrity equivalent to signing them individually); the installer stays small. `packs.rs`'s manifest gains the three binaries (+ DLLs as one zip entry, extracted on install) alongside the weights. | this row |
| 2026-09-14 (day 2) | S2 VERDICT: PASS on CUDA EP (agent, `s2/SPEED.md`) | `bs_roformer_ep368_mha_fp16.onnx`: the 24 exported MatMul-Softmax-MatMul chains rewritten as `com.microsoft.MultiHeadAttention` (`speed_mha.py`, 1.3e-7 vs the original on CPU), then fp16 with kept fp32 IO (`speed_fp16.py`); session with `gpu_mem_limit` 8 GB, kNextPowerOfTwo, tf32; IO binding | wall RTF 0.123 (graph 0.083), vocals 14.892 / residual 14.942 dB (+0.004 / +0.005 vs PyTorch): PASS both lines. ROOT CAUSE of the 1.65: the unfused attention materialized 62x8x801x801 fp32 score tensors (1.27 GB each, 12 per chunk); the arena re-extended by gigabytes per run, and under an 8 GB cap the original graph OOMs. Placement was never the issue (CPU ran 3.5 ms of int64 shape glue per run). Of the winner's 7.4 s wall, 2.1 s is the numpy STFT/iSTFT: the Rust port gets that back. Landmines: `OrtValue.update_inplace` on a cuda buffer fed a wrong input at 801 frames (bind a fresh OrtValue per call; no CUDA graphs); MultiHeadAttention has no DirectML kernel, so the unfused fp16 file (`bs_roformer_ep368_fp16.onnx`, RTF 0.129 on CUDA) is the DirectML candidate, timing queued. | `E:\models\separator\bs_roformer_ep368_mha_fp16.onnx`, `s2/SPEED.md`, `s2/speed_*.py`, `s2/timing_mha_fp16*.json` |
| 2026-09-14 (day 2) | S5 full episode, offline (`dub_offline.py --reference pcs --verify-url` on the Vulkan whisper-server) | 344 turns, all 23 min; TTS on the CUDA server, translator on CUDA llama-server, verifier on the Vulkan whisper server | 238 lines dubbed; 209 fit, 11 stretched (<= 15%), 18 cut (7.6%); wall 253.7 s for 1383.1 s = RTF 0.183 (GPU shared only with the idle resident servers; the separator was not in the loop: stems precomputed at RTF 0.22 on PyTorch); translation 23.2 s, synthesis 197.1 s. Against S5's line (pipeline RTF <= 0.7): PASS with room for the separator (0.12 on CUDA ONNX). MP3 sent to Michael; his ear is the GO/NO-GO. | `E:\datasets\s5\full\{dub.wav,lines.jsonl,summary.json,tomb-raider-king-s01e09-dub.mp3}` |
| 2026-09-14 (day 2) | S5 cuts, read | the 18 cut lines of the full episode | All are fast exchanges: room to the next turn 0.75-2.5 s (median 1.6 s) against a synthesis of 1.3-4.5 s; VoxCPM2's shortest output is ~1.1 s even for two words, so a 0.75 s slot cannot hold "General Kiera?". D5 refinement to implement in the worker: room = time to the next turn of a DIFFERENT speaker (same-speaker turns may run together), plus a tolerated overlap of ~250 ms into the next line before cutting, and the cut fade lengthened to 120 ms. Expected to convert most of the 18 into fits or stretches. | `E:\datasets\s5\full\lines.jsonl` (fit starts with "cut") |
| 2026-09-14 (day 2) | S2 on DirectML, the unfused fp16 file (`speed_parity.py --provider DmlExecutionProvider`, GPU idle, `onnxruntime-directml` 1.24) | `bs_roformer_ep368_fp16.onnx` (no MHA contrib op, so it has a DML kernel path) | wall RTF 0.199 (graph 0.162; 1.9 s of the 11.9 s is the numpy STFT/iSTFT that the Rust port replaces), vocals 14.891 / residual 14.941 dB: PASS both lines. DECISION D1c: the separator ships on DirectML for every Windows GPU; with D1b the pack is entirely CUDA-free (ggml parts on Vulkan, ONNX on DirectML). The fused CUDA file (0.123) stays a bench result. | `s2/timing_unfused_fp16_dml.json` |
| 2026-09-14 (day 2) | Build: the worker's sidecar clients (`dubclients.rs`) | `Translator` (G3's SYSTEM prompt verbatim, temperature 0, thinking off, first line kept), `Tts` (`/v1/audio/speech`, base64 16 kHz reference, seed), `Asr` (multipart `/inference`, json), word WER, `synthesize_verified` (pcsv: seeds 42/7/1234, WER <= 0.2 keeps the take); 5 unit tests on stand-in TCP servers | LIVE round trip against the resident servers (Vulkan llama-server on the 2B, CUDA TTS, Vulkan whisper): "待ってください。まだ話は終わっていません。" -> "Please wait. The story isn't over yet." in 0.31 s; verified take 2.72 s of audio, WER 0.000, 0.75 s wall (one take); reference transcribed back in 46 ms. The product client speaks to the exact binaries the bench measured (R2). 83 lib tests pass. | `apps/desktop/src-tauri/src/dubclients.rs` (`live_round_trip`, ignored, env-driven) |
| 2026-09-14 (day 2) | D6a in the manifest | `packs.rs`: 26 binary entries added (two static Vulkan servers, `vcomp140.dll` from the VC143 redistributable, the prebuilt `llama-server` b10944 with its 22 loaded DLLs by `dumpbin /DEPENDENTS`, the `ggml-cpu-*` variants ggml picks at run time); one owner of the base url (`pack_url!`); the pack staged flat at `E:\packs\dubbing\1` (binaries copied, weights hardlinked, 7.57 GB, 31 files) for the eventual upload and for S7 over a local range server | Binaries add 196.8 MB to the 7.38 GB of weights. Flat layout is safe: the static servers import none of the DLLs (only `vulkan-1.dll` and `vcomp140.dll`). `llama-server` also imports the VC runtime (`VCRUNTIME140`, `MSVCP140`), which the shell itself already needs. Not yet: a CI job that builds the two static servers (the pack is published by hand for v1). | `packs.rs` MANIFEST, `E:\packs\dubbing\1` |
| 2026-09-14 (day 2) | Build: fit and mix (`dubfit.rs`), D5 + refinement | `room_ms` (to the next turn of a DIFFERENT speaker + 250 ms tolerated overlap; 5 s trailing), `budget_chars` (13.7 chars/s, floor 12), `fit` (unchanged / WSOLA time-stretch <= 1.15 / cut with a 120 ms fade), `level_gain` (RMS match, <= 4x), `mix_into` (stereo bed, clamped); 7 unit tests (room cases, pitch kept under stretch by zero-crossing rate, fade to silence, gain bound, clamp and overhang) | The stretch is a Rust WSOLA (2048-sample Hann frames, half overlap, +-512 search), replacing the bench's ffmpeg `atempo` (no ffmpeg in the shell). LIVE gate on the resident servers: a 3.52 s take sped up by the full bound to 3.06 s transcribes back at WER 0.000 (plain take 0.000). Pair for Michael's ear: `take-plain.wav` / `take-stretched-1.15.wav`. | `apps/desktop/src-tauri/src/dubfit.rs` (`live_stretched_take_stays_faithful`, ignored) |
| 2026-09-14 (day 2) | Build: turn segmenter in Rust (`turns.rs`, agent), parity to `g5/turns.py` | Resemblyzer's VoiceEncoder exported to ONNX (`g5/export_speaker_encoder.py`, `ge2e_resemblyzer.onnx` 5.7 MB, opset 17; 20 random slices vs `embed_utterance`: min cosine 1.000000); Rust mel (realfft, Slaney filterbank in librosa's f32 order, RAW power mel: Resemblyzer never logs it), partials contract, `segment`, `speaker_reference`; ort `load-dynamic` + `api-24` on the CPU provider | mel worst relative error 1.9e-5; embedding cosine 1.000000; F6 full episode: 344 turns, 344/344 within 40 ms of the Python turns, 58 speaker ids (= reference), release build 18.6 s for 1383 s = RTF 0.013. TRAP: the `webrtc-vad` crate's `Aggressive` is webrtcvad mode 2; turns.py runs mode 3 (`VeryAggressive`): with mode 2 only 24% of turns matched (runs merged across 400-700 ms gaps). Reference quirk kept: a run still open at the stem's end is dropped. Merged into main (6 fast tests, 3 ignored parity tests), ort pinned `=2.0.0-rc.13` with the separator's feature set. | `apps/desktop/src-tauri/src/turns.rs`, `E:\models\speaker\ge2e_resemblyzer.onnx` (in the manifest and staged) |
| 2026-09-14 (day 2) | Build: separator in Rust (`separate.rs`, agent), parity to the Python arms on F5 | ort `load-dynamic` on the DirectML EP, `bs_roformer_ep368_fp16.onnx` (unfused fp16), STFT/iSTFT and the 147:160 windowed-sinc resampler in Rust, chunk schedule reproduced against 12 Python schedules | vocals 14.891 / residual 14.941 dB (targets 14.891 / 14.941): PASS, exact; release wall RTF 0.206 (debug 0.334); CPU EP 14.893 / 14.942 at RTF 7.4. LANDMINE: ort's free-dimension overrides (batch 1, frames 801, meant for DML graph fusion) make DirectML return garbage on this graph (-3.35 dB, fp16 and fp32 alike; a Python probe showed the override alone moves DML 91 off CPU on a 64 peak); without them fp16 matches CPU to 0.156, fp32 to 0.0003. `DirectML.dll` is pinned from the pack dir through libloading before the runtime loads, so the bare-name lookup resolves to the pack's copy. Merged into main. | `apps/desktop/src-tauri/src/separate.rs` (`f5_parity_directml`, ignored), `E:\tools\ort-dml\{onnxruntime,DirectML}.dll` (1.24.4 wheel; in the manifest and staged) |
| 2026-09-14 (day 2) | S5 in the SHELL, first end-to-end run of `dubpipe.rs` (debug build, `live_f6_windows`) | the staged pack's own sidecars (Vulkan `llama-server`, `llama-tts-server`, `whisper-server`, all on Vulkan0 per their logs), DirectML separator, Rust turns; F6 windows 3-4 (90-150 s) | Pipeline open 6-12 s (three sidecars + two ONNX models). 17 lines over 60 s: 16 fit, 1 cut; whisper detected `japanese`. Wall 148 s = RTF 2.47 in debug, of which the LINES were 58 s of window 3's 91 s, and 51 of those 58 s were three lines whose every take ran away (a scream "Uuuuuuuuuu!", "(Scream)", "Waah!": 3 x 32 s of generation each, then discarded). Two bugs on the way, both fixed: (1) turn milliseconds were relative to the buffer on a fresh buffer and absolute after an extension, so window 4's slices were empty and whisper answered 400 (now: turns and slices stay relative to the buffer, keys and line records are absolute); (2) no runaway guard before the server's 200-step cap (now: `max_steps` per request = 3x the room, D5's guard, and a take that reaches it is discarded). Design change from the same run: a turn with nothing to say (a non-speech tag from whisper, an empty translation, every take a runaway) keeps its ORIGINAL voice in the bed instead of going silent. Release-build RTF pending. | `E:\datasets\s5\pipeline\{dub-90-150.wav,lines.jsonl,logs\}` |
| 2026-09-14 (day 2) | S5 in the shell, RELEASE build (`live_f6_windows`, GPU shared only with the idle resident servers) | same windows 3-4 after the runaway guard and the keep-original rule | Pipeline open 6.2 s. Window 3 (fresh buffer: 67 s separated, 9 lines) 49.7 s; window 4 (steady state: 38 s separated, 8 lines) 26.3 s = STEADY RTF 0.88, over S5's 0.7 line. 17 lines: 16 fit, 1 kept original ("(Scream)" is non-speech now), the scream line's three takes now cost 10.3 s (cap 3x room) instead of 29 s. Where the steady window goes: separation ~8 s (RTF 0.206 on 38 s), segmentation ~1 s, lines ~17 s at 1.1-5.2 s each. The TTS server's own timers (`clone timing:` in `tts.stderr.log`) put the REFERENCE ENCODE at 1.0-1.15 s per call for a 54-position (8 s) reference against 0.13-0.78 s of decode: the fixed per-call cost S3 measured, now the largest single term, paid again on every retry take. Two levers, in order: (1) pipeline the next window's separation (DirectML queue) under this window's synthesis (Vulkan queue): hides ~8 s -> ~RTF 0.6; (2) cache the encoded reference per speaker on the TTS server (S3: the reference is a KV PREFIX) or shorten the reference (8 -> 4 s halves the encode; identity cost unmeasured). Neither blocks S6: at 0.88 the worker still runs ahead of real time. | `E:\datasets\s5\pipeline\{dub-90-150.wav,lines.jsonl,logs\tts.stderr.log}` |
| 2026-09-14 (day 2) | Build: the Audio menu row (web) | `useDub.ts` (pack status -> needs-pack with the size, `pack_install` with progress, `dub_start` + `dub_select`, `dub` status events, stop on stream change / other track), `AudioMenu.tsx` row "AI dub (English)" with the phase under it, `ShellVideo.js` marks the shell's external audio track `generated` so it never lists twice; six `AUDIO_DUB_*` strings | Web build green; `tsc` clean on the new files (134 pre-existing errors elsewhere). Opt-in decided by default: the row IS the opt-in (the download size is on it before anything is fetched); a Settings block for pack status and removal stays open. Not yet exercised in the shell (S6). | `apps/web/src/routes/Player/useDub.ts`, `AudioMenu/AudioMenu.tsx`, `packages/video/src/ShellVideo/ShellVideo.js`, `packages/translations/en-US.json` |
| 2026-09-14 (day 2) | S6, first attempt in the dev shell (`s6/s6-driver.js` over CDP, `RILLIO_DUB_PACK_DIR` at the staged pack) | the driver called `dub_start` directly and polled it | The real pipeline ran in the shell (three sidecars spawned on free ports 37213/37215/37224, ORT 1.24.4 loaded from the pack, DirectML session, 67 s separated in 31.7 s in the DEBUG shell, 8 lines dubbed into window 3 in 58 s) but every poll spawned a NEW worker: the web hook received the shell's "preparing"/"running" events for a dub it had not started, its stop-on-other-track rule saw no dub track selected and called `dub_stop`, and the next poll's `dub_start` spawned again (~110 workers, all serialized on the pipeline mutex and re-filling the same window). ROOT CAUSE: the hook stopped a run it did not own. FIX: the hook is ARMED only by its own start; unarmed status events never stop anything. Second finding from the same run: two of the three sidecars (translator, TTS) OUTLIVED the force-killed shell despite the KILL_ON_JOB_CLOSE job object (whisper died); `s6/job-probe.ps1` (IsProcessInJob) is the next instrument, after the rerun. Design change: the hook no longer selects the track at start; the status event carries `aheadS` (dubbed seconds ready from the playhead) and the switch happens once a full window is ready ahead, so the original keeps playing instead of stalling on an unproduced window; the row reads "Preparing, N s ready". | `E:\datasets\s5\pipeline\shell.log` |
| 2026-09-14 (day 2) | S6, second attempt (debug shell, armed hook, driver on the shell commands) | one worker per start | The hook fix holds: one sidecar set, every window filled exactly once (20, 23, 27, 30, 34, 37, 41, 45), no `dub_stop`. But the DEBUG shell's pipeline runs SLOWER than real time (62 s per 30 s window: separator RTF 0.47 unoptimised, debug turns), so the worker chased the playhead window by window (each window finished after the playhead had left it) and "a window ready ahead" never came; the driver died on an mpv `-12` from its late select. Two consequences built in: (1) before the first read, a window the playhead has half crossed is skipped for the next one (it would be gone by the time it is produced), (2) S6 runs on the RELEASE shell (`launch-dev-shell.ps1 -Release`). Also observed: the shell's job object DID kill all three sidecars on a forced kill this time (`s6/job-probe.ps1`: all in a job); the two orphans of the first attempt coincided with the worker storm and did not reproduce. CDP `Input.dispatchKeyEvent` / `dispatchMouseEvent` do not reach this WebView2 (no shortcut fires, the chrome does not wake), so the row's own click stays Michael's. | `E:\datasets\s5\pipeline\shell.log`, `s6/s6-driver.js`, `s6/probe-input.js` |
| 2026-09-14 (day 2) | S6 PASS: the real dub in the RELEASE shell (`launch-dev-shell.ps1 -Release`, `s6-driver.js`, F6 over the range server, staged pack) | the pipeline in the product, end to end, timeline over `rillio-dub://` | Pack: `pack_status` reports all 34 files present and verified (7.85 GB, adopted by hash in the first attempt). `dub_start` -> "preparing" -> "running" in 7.7 s (three sidecars healthy in 0.5 / 1.8 / 3.3 s, ORT + DirectML session). Windows (release): 13.5, 31.4, 13.3, 36.3, 17.9, 15.9, 35.1 s per 30 s (separation 7.0 s per 38 s = RTF 0.18; the spread is the lines: a window with a scream or a retry costs twice). Lead over the playhead grew 6 -> 5 -> 22 -> 15 -> 28 -> 42 s; the switch rule (a full window ready ahead) fired 128 s after the start; `dub_select` put `aid` 2 in 4 ms with NO stall (playback advanced 9.97 s in 10 s, paused-for-cache false throughout). Seek to 600 s (unproduced): playback held at the target, the worker re-targeted (fresh buffer, 67 s separated in 13.4 s), window 19 then 20 produced, playback resumed 63.5 s after the seek. Seek back to 40 s (produced): played at once. `aid` 1 restored in 3 ms. Forced kill of the shell afterwards: zero sidecar survivors. Not exercised: the row's own click (CDP input does not reach the WebView2), so the hook's start/arm/switch path is verified by construction and by the driver's emulation of its rule, not by a click. | `E:\datasets\s5\pipeline\{s6-report.json,shell.log,shell-dub-0-210.mp3,original-0-210.mp3}` |
| 2026-09-14 (day 2, evening) | S6 on Michael's own machine and profile (release shell launched outside the container via `explorer.exe`, `s6/launch-michael.cmd`; the episode over `s6/serve-f6.cmd`) | his click on the row, mid-episode | The whole shell FROZE ("Not Responding", frame stuck under the menu's grey backdrop) about a minute after the pick; every sidecar log stopped at the same second (19:18:06), no status event followed, the ASR sidecar itself still answered a probe in 0.1 s. READ: he picked the row a second time while "running"; the hook's second-pick path called `dub_select` at once, and mpv's `audio-add` opens the track INSIDE the core's playloop, probing the head of the file, i.e. window 0, which the worker had skipped (mid-episode start): the probe read parked on the timeline, the core froze (video too), the player lock stayed held across the command, the worker parked on that lock in `status()` after its window, and mpv's read could never be served: a three-way deadlock. S6 in the container never hit it because the playhead was near 0 s (window 0 first) and the driver selected only after windows existed. FIXES: (1) reads inside the first 1 MiB of the file never wait (silence for an unproduced window 0: never played, the switch waits for the playhead's window, window 0 is backfilled), unit-tested; (2) `dub_select` refuses while nothing is dubbed at the playhead; (3) the worker never touches the player lock: the mpv event loop publishes `time-pos` into an atomic the worker reads; (4) the hook's second pick switches only when something is ready. UX from his questions: the row reads as CHOSEN from the pick (dot + tint) with the sub-line telling the truth ("Preparing, N s ready" -> "Dubbing ahead"); the spinner means "not playing yet", so a running dub (and a running AI-subtitles transcription) shows steady. 110 lib tests. Re-verification: the driver now starts at 300 s, tries an early select (must be refused, player alive), then the normal switch. | `\\localhost\C$\Users\Michael\AppData\Local\com.rillio.desktop\logs\dub` (his sidecar logs, read past the container redirect) |
| 2026-09-14 (day 2, night) | D2 amended by Michael's read of the flow ("it waits for actual playback instead of waiting for cache: check the scrubber position, check what is cached, generate immediately from it") | the switch rule | The wait-for-a-window-ahead rule made the dub chase a moving playhead for minutes (the lead grows a few seconds per window at RTF ~0.9). The playback layer already does what he describes: with the track selected, mpv HOLDS on unmade audio like a slow cache and resumes as it is filled (S1). So the track is selected AT THE PICK: `dub_select` adds it at once (safe now that the file's head never blocks), the player buffers once (about a window), then runs; the row reads "Buffering, N s ready" while the reader is parked (`waiting` in the status event, from a flag the timeline read sets) and "Dubbing ahead" while the audio flows. The subtitle transcriber already works forward from the scrubber position; while a dub runs it takes the dub's lines instead of running a second whisper. Same day, the same run showed two whisper instances (subtitle CPU whisper + the dub) crawling together: 14-52 s per 10 s chunk against 1.7 s alone; the dub now yields/resumes the CPU transcription (`transcribe::yield_to_dub` / `resume_after_dub`). | `dub.rs`, `useDub.ts`, `transcribe.rs` |
| 2026-09-14 (day 2, night) | S6 re-verified with select-at-pick (release shell, `s6-report-3.json`, 30 s windows) | driver: seek to 300 s, `dub_start`, `dub_select` at once | Track selected in 4 ms with the player alive (no freeze: the probe region held); "running" 8.3 s after the start (sidecars resident from the earlier run); the player then HELD on the unmade window: 1.0 s of playback in the 10 s sync sample while the first window (fresh buffer, 67 s separated) was made, `waiting` seen in the status; after the window landed, 8.0 s played in 8 s; seek into a later unmade region held and resumed; `aid` back fine. The hold is the window's cost, 30-40 s here: too long for a pick. WINDOW_S 30 -> 10 (one constant; the window map records its window size so a map from another size is not misread): the pick's hold becomes one 10 s window's cost, ~10-15 s. Owed for "seconds": the separator on its own thread (steady RTF ~0.6), then per-line frontier filling (a line lands as soon as it is synthesized, the hold = separation + one line). | `E:\datasets\s5\pipeline\s6-report-3.json` |
| 2026-09-14 (day 2, night) | Michael's second run (10 s windows, select at the pick) | his click at ~30 s; the row said "Buffering, 0 s ready" for minutes while the video ran to 4 min | The worker WAS producing (stretches 0-100, 120-150, 200-220 s) and the reader was parked, but mpv does NOT hold playback when an already-playing external audio track stalls: video runs on without audio, the playhead runs away, the worker chases it. It held in the driver's runs only because those seeked (a seek waits for the audio demuxer). FIX: the shell pauses the player itself while a dub read is parked and resumes when served (`Timeline::hold_playback`, our own paused-for-cache; a viewer's own pause is left alone), and the player's buffering overlay shows during the hold (`dub.waiting`). Same evening, his other reads: the orange symbol in the loading overlay is not part of the design (removed: the loader is the title's logo, or its name as text, filling up, everywhere); "the sync sounds off, English words seem shorter": ISOCHRONY, as studios fit a line: the translation is budgeted to the ORIGINAL utterance's length (not the room), the take's edge silence is trimmed, and a take shorter than 90 % of the original is slowed toward it by up to 15 %. Also: the lib's test binaries died at load (STATUS_ENTRYPOINT_NOT_FOUND: `TaskDialogIndirect` needs the comctl32 v6 manifest tauri-build gives only the bin); `build.rs` now links the resource object into every target. | `dub.rs`, `dubfit.rs`, `Buffering.tsx`, `build.rs` |
| 2026-09-14 00:23 | S1 (fast worker) | `rillio-dub://` timeline + pass-through worker in the dev shell over CDP, F6 over a local range server (`s1-driver.js`) | Track appears in mpv's track-list as external pcm_s16le 48 kHz stereo with the full duration; `aid` 2 within 9 ms of `audio-add`; first audio 776 ms after select (window 0 took 583 ms); seek into an unproduced region held at the target without a cache pause and resumed when the worker got there; seek back into produced audio played at real time; `aid` 1 restored. PASS on all four lines. Worker: 17 windows in 9 s (pass-through decode RTF 0.02); it re-targeted to the seek's window within 0.6 s. | `E:\datasets\g0\shell.log` |
| 2026-09-14 00:36 | S1 (worker at RTF 0.5, `RILLIO_DUB_MIN_WINDOW_S=15`) | fine sampler, `s1-sampler.js` | mpv's cache is unbounded in time (`cache-secs` 3.6e6, `demuxer-max-bytes` 150 MB = 13 min of PCM), so its read-ahead thread parks at the frontier constantly; playback nonetheless ran at real time (55.8 s in 60 s) with one 1.3 s hitch when the playhead itself caught the frontier: a blocked READ-AHEAD does not stall playback, only the playhead reaching unproduced audio does. FIRST SEEK RUN FAILED: mpv stayed in `seeking` forever because its demuxer thread was parked on window 5 while the worker had moved to window 20 for the new position (deadlock by design). Fixed: a `time-pos` set interrupts parked reads (`dub::interrupt_reads`, one read error mpv absorbs) and the worker produces the READER's window first. | `E:\datasets\g0\shell.log` |
| 2026-09-14 00:38 | S1 (RTF 0.5, seek to 200 s, after the fix) | `s1-sampler.js` | mpv seeked the track to 199.9 s 0.2 s after the command; the in-flight window (0.5 s left) finished, the target window took 15 s, playback resumed 24 s after the seek, then one more 5 s hitch at the next window boundary (the seek landed 10 s before it, less than one window's cost) and real time from there as the lead grew. `paused-for-cache` stayed false throughout: the UI must take its buffering state from the dub status event, not mpv. Product consequences: produce the first window after a seek in smaller pieces (or accept a bounded hitch), abort an in-flight window when the reader moves, and surface waits as buffering. | `E:\datasets\g0\shell.log` |

## Status

See `../../checklists/dubbing-stage1.md`. Decomposition in `decomposition.md`.
