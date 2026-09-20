"""G0 ASR arm: the app's whisper path (`apps/desktop/src-tauri/src/transcribe.rs`)
re-implemented 1:1 on the same engine and weights, so the bench measures what
ships (R2). Constants mirror the product file by name.

    python transcribe.py <16k mono wav> <out.jsonl> [--construction product|native]

product: 10 s chunks, audio_ctx 768, VAD gate, language auto then locked.
native:  whisper's own 30 s window and full audio_ctx (the knob arm, R6);
         same model, gate, cleaning and language handling.
"""
import argparse
import json
import os
import time
from pathlib import Path

import numpy as np
import soundfile as sf
import webrtcvad
from pywhispercpp.model import Model

MODEL_PATH = r"E:\models\whisper\ggml-small-q5_1.bin"  # the file the app downloads
SAMPLE_RATE = 16_000
CONSTRUCTIONS = {"product": (10.0, 768), "native": (30.0, 0)}  # (CHUNK_S, AUDIO_CTX; 0 = whisper default)
MIN_SPEECH_FRACTION = 0.02
NO_SPEECH_THOLD = 0.6
VAD_FRAME_MS = 20
NON_SPEECH_CHARS = set("♪♫. \t")


def threads() -> int:
    return max(2, min(8, (os.cpu_count() or 4) // 2))


def speech_fraction(samples: np.ndarray) -> float:
    vad = webrtcvad.Vad(3)  # Aggressive, as autosync::speech_bins
    frame = SAMPLE_RATE * VAD_FRAME_MS // 1000
    pcm = samples.tobytes()
    n = len(samples) // frame
    if n == 0:
        return 0.0
    voiced = sum(vad.is_speech(pcm[i * frame * 2:(i + 1) * frame * 2], SAMPLE_RATE) for i in range(n))
    return voiced / n


def clean_text(text: str):
    t = text.strip()
    if not t:
        return None
    bracketed = (t[0] == "[" and t[-1] == "]") or (t[0] == "(" and t[-1] == ")")
    if bracketed or all(c in NON_SPEECH_CHARS for c in t):
        return None
    return t


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("wav", type=Path)
    ap.add_argument("out", type=Path)
    ap.add_argument("--construction", choices=CONSTRUCTIONS, default="product")
    args = ap.parse_args()
    chunk_s, audio_ctx = CONSTRUCTIONS[args.construction]

    samples, rate = sf.read(str(args.wav), dtype="int16")
    if rate != SAMPLE_RATE or samples.ndim != 1:
        raise SystemExit(f"{args.wav}: need {SAMPLE_RATE} Hz mono, got {rate} Hz {samples.ndim}-d")
    model = Model(MODEL_PATH, n_threads=threads(), print_progress=False, print_realtime=False, redirect_whispercpp_logs_to=False)
    params = dict(
        translate=False, no_context=True, single_segment=False, print_special=False, print_timestamps=False,
        suppress_blank=True, suppress_nst=True, no_speech_thold=NO_SPEECH_THOLD, greedy={"best_of": 1},
    )
    if audio_ctx:
        params["audio_ctx"] = audio_ctx

    language = None
    chunk_samples = int(chunk_s * SAMPLE_RATE)
    total = (len(samples) + chunk_samples - 1) // chunk_samples
    started = time.perf_counter()
    rows, skipped = [], 0
    with args.out.open("w", encoding="utf-8") as f:
        for chunk in range(total):
            start_s = chunk * chunk_s
            piece = samples[chunk * chunk_samples:(chunk + 1) * chunk_samples]
            if len(piece) < SAMPLE_RATE:
                break
            if speech_fraction(piece) < MIN_SPEECH_FRACTION:
                skipped += 1
                continue
            audio = piece.astype(np.float32) / 32768.0
            segments = model.transcribe(audio, language=language or "auto", **params)
            if language is None:
                # The product locks the language detected on the first chunk
                # that had speech; auto_detect_language returns ((lang, p), all).
                language = model.auto_detect_language(audio)[0][0]
            for s in segments:
                text = clean_text(s.text)
                if text is None:
                    continue
                row = {"chunk": chunk, "start_ms": int(start_s * 1000) + s.t0 * 10, "end_ms": int(start_s * 1000) + s.t1 * 10, "text": text}
                rows.append(row)
                f.write(json.dumps(row, ensure_ascii=False) + "\n")
            print(f"chunk {chunk + 1}/{total} -> {len(segments)} segments", flush=True)
    elapsed = time.perf_counter() - started
    summary = {"construction": args.construction, "chunk_s": chunk_s, "audio_ctx": audio_ctx, "language": language,
               "chunks": total, "chunks_skipped_no_speech": skipped, "segments": len(rows),
               "audio_s": round(len(samples) / SAMPLE_RATE, 1), "wall_s": round(elapsed, 1),
               "rtf": round(elapsed / (len(samples) / SAMPLE_RATE), 3), "threads": threads(), "model": Path(MODEL_PATH).name}
    print(json.dumps(summary, ensure_ascii=False))
    args.out.with_suffix(".summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")


if __name__ == "__main__":
    main()
