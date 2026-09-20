"""G5: does VoxCPM2 carry the original line's voice and emotion into the English
dub when the line's own audio is the prompt? Runs against the resident
llama-tts-server (the S3 process), on F6's cues.

    python run.py prompts <cues.jsonl> <aligned_asr.jsonl> <stem_16k_mono.wav> <out_dir>
        -> <out_dir>/prompts/<id>.wav (the cue's slice of the dialogue stem) + prompts.jsonl
    python run.py synth <prompts.jsonl> <english_arm.jsonl> <out_dir> --arm p|r [--reference ref.wav] [--port 8090] [--limit N]
        -> <out_dir>/<arm>/<id>.wav + <arm>.jsonl (per line: wall, audio_s, prompt_s)
    python run.py score <out_dir> --arm p --arm r ...
        -> instruments per arm: english rate + WER (whisper small), emotion2vec cosine + top agreement, speaker cosine

Arms: p = prompt continuation (prompt = the line's stem slice + its ASR text,
target = the English line); r = reference-only cloning with one fixed 15 s
reference (the control).
"""
import argparse
import base64
import io
import json
import statistics
import time
import wave
from pathlib import Path

import numpy as np
import requests
import soundfile as sf

MIN_PROMPT_S = 0.8
MAX_PROMPT_S = 12.0
# Windowed arms: the scene before the line (continuation) or around it
# (reference), long enough for the latent to settle on voice and register.
WINDOW_BEFORE_S = 8.0
WINDOW_AROUND_S = 8.0
PROMPT_RATE = 16_000
OUT_RATE = 48_000
WHISPER_MODEL = r"E:\models\whisper\ggml-small-q5_1.bin"
EMOTION_MODEL = "iic/emotion2vec_plus_base"
# A dub whose heard text differs from the intended line by more than this
# changed the sentence (dropped, reordered or invented words), not just an accent.
FAITHFUL_WER = 0.2
# The CLI's 200-step cap is 32 s of audio: anything that long never stopped.
RUNAWAY_S = 31.0


def read_jsonl(p: Path) -> list:
    return [json.loads(l) for l in p.read_text(encoding="utf-8").split("\n") if l.strip()]


def write_jsonl(p: Path, rows: list) -> None:
    p.write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows), encoding="utf-8")


def wav_bytes(samples: np.ndarray, rate: int) -> bytes:
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1); w.setsampwidth(2); w.setframerate(rate)
        w.writeframes((np.clip(samples, -1, 1) * 32767).astype(np.int16).tobytes())
    return buf.getvalue()


def cmd_prompts(a):
    cues = {r["id"]: r for r in read_jsonl(Path(a.cues))}
    asr = read_jsonl(Path(a.aligned))
    stem, rate = sf.read(a.stem, dtype="float32")
    if rate != PROMPT_RATE or stem.ndim != 1:
        raise SystemExit(f"{a.stem}: need {PROMPT_RATE} Hz mono, got {rate} Hz {stem.ndim}-d")
    out = Path(a.out) / "prompts"
    out.mkdir(parents=True, exist_ok=True)
    rows = []
    for r in asr:
        c = cues[r["id"]]
        seg = stem[int(c["start_ms"] / 1000 * rate):int(c["end_ms"] / 1000 * rate)]
        seconds = len(seg) / rate
        usable = bool(r["src"]) and MIN_PROMPT_S <= seconds <= MAX_PROMPT_S
        path = out / f"{r['id']:03d}.wav"
        if usable:
            sf.write(path, seg, rate)
        rows.append({"id": r["id"], "prompt_s": round(seconds, 2), "usable": usable, "src": r["src"], "ref": r["ref"],
                     "prompt_wav": str(path) if usable else None, "start_ms": c["start_ms"], "end_ms": c["end_ms"]})
    write_jsonl(Path(a.out) / "prompts.jsonl", rows)
    print(f"{sum(r['usable'] for r in rows)}/{len(rows)} usable prompts -> {out}")


OVERLAP_SUBWINDOW_S = 0.8
# Below this minimum cosine between sub-window speaker embeddings the slice is
# flagged as holding more than one voice. UNCALIBRATED heuristic: on F6 the
# scores run 0.49 to 0.85 (median 0.69) because 0.8 s windows are noisy for
# GE2E; 0.60 flags the most suspect tenth and catches the cue Michael heard
# (row 01 = 0.585). A real detector is the speaker-split model itself.
OVERLAP_MIN_COSINE = 0.60


def cmd_overlap(a):
    """Flag prompt slices that hold more than one speaker: Resemblyzer embeddings
    on overlapping sub-windows; the minimum pairwise cosine is the score."""
    from resemblyzer import VoiceEncoder, preprocess_wav

    encoder = VoiceEncoder()
    rows = read_jsonl(Path(a.prompts))
    out = []
    for p in rows:
        if not p["usable"]:
            continue
        wav = preprocess_wav(Path(p["prompt_wav"]))
        step = int(OVERLAP_SUBWINDOW_S * 16_000)
        pieces = [wav[i:i + step] for i in range(0, max(1, len(wav) - step + 1), step // 2)]
        pieces = [x for x in pieces if len(x) >= step // 2]
        if len(pieces) < 2:
            out.append({"id": p["id"], "overlap_score": None, "overlapped": False})
            continue
        embeds = np.stack([encoder.embed_utterance(x) for x in pieces])
        embeds /= np.linalg.norm(embeds, axis=1, keepdims=True) + 1e-9
        sims = embeds @ embeds.T
        score = float(sims[np.triu_indices(len(pieces), 1)].min())
        out.append({"id": p["id"], "overlap_score": round(score, 3), "overlapped": score < OVERLAP_MIN_COSINE})
    write_jsonl(Path(a.prompts).with_name("prompts.overlap.jsonl"), out)
    print(f"{sum(r['overlapped'] for r in out)}/{len(out)} slices flagged as more than one voice")


def window_wav(stem_path: str, start_s: float, end_s: float) -> bytes:
    stem, rate = sf.read(stem_path, dtype="float32")
    seg = stem[max(0, int(start_s * rate)):int(end_s * rate)]
    return wav_bytes(seg, rate)


REFERENCE_MAX_S = 8.0
_STEM_CACHE: dict = {}


def speaker_reference(stem_path: str, turns: list, start_ms: int, end_ms: int) -> bytes | None:
    """The cue's speaker's own turns, nearest the cue first, concatenated up to
    REFERENCE_MAX_S. None when no turn overlaps the cue."""
    if stem_path not in _STEM_CACHE:
        _STEM_CACHE[stem_path] = sf.read(stem_path, dtype="float32")
    stem, rate = _STEM_CACHE[stem_path]
    overlapping = [t for t in turns if t["start_ms"] < end_ms and t["end_ms"] > start_ms]
    if not overlapping:
        return None
    speaker = max(overlapping, key=lambda t: min(t["end_ms"], end_ms) - max(t["start_ms"], start_ms))["speaker"]
    mid = (start_ms + end_ms) / 2
    own = sorted((t for t in turns if t["speaker"] == speaker), key=lambda t: abs((t["start_ms"] + t["end_ms"]) / 2 - mid))
    pieces, total = [], 0.0
    for t in own:
        seg = stem[t["start_ms"] * rate // 1000:t["end_ms"] * rate // 1000]
        if total + len(seg) / rate > REFERENCE_MAX_S and pieces:
            break
        pieces.append(seg)
        total += len(seg) / rate
    # Chronological order so the reference reads as speech, not a shuffle.
    ordered = [seg for _, seg in sorted(zip([own[i]["start_ms"] for i in range(len(pieces))], pieces), key=lambda x: x[0])]
    return wav_bytes(np.concatenate(ordered), rate)


RETRY_TAKES = 3
RETRY_SEEDS = (42, 7, 1234)


class Verifier:
    """Whisper-back check of a take: what a listener would hear, as WER."""

    def __init__(self):
        import jiwer
        from pywhispercpp.model import Model

        self.model = Model(WHISPER_MODEL, n_threads=8, print_progress=False, print_realtime=False, redirect_whispercpp_logs_to=False)
        self.tr = jiwer.Compose([jiwer.ToLowerCase(), jiwer.RemovePunctuation(), jiwer.RemoveMultipleSpaces(), jiwer.Strip()])
        self.wer = jiwer.wer

    def score(self, audio: np.ndarray, text: str) -> float:
        idx = np.arange(0, len(audio), OUT_RATE / PROMPT_RATE)
        mono = np.interp(idx, np.arange(len(audio)), audio).astype(np.float32)
        segs = self.model.transcribe(mono, language="en", translate=False, no_context=True, print_progress=False, print_realtime=False)
        heard = self.tr(" ".join(s.text.strip() for s in segs))
        return self.wer(self.tr(text), heard) if heard else 1.0


class ServerVerifier(Verifier):
    """The same check through whisper-server (whisper.cpp, GPU): POST /inference
    with the take as a 16 kHz mono WAV; the CPU model is never loaded."""

    def __init__(self, url: str):
        import jiwer

        self.url = url.rstrip("/")
        self.tr = jiwer.Compose([jiwer.ToLowerCase(), jiwer.RemovePunctuation(), jiwer.RemoveMultipleSpaces(), jiwer.Strip()])
        self.wer = jiwer.wer
        if requests.get(f"{self.url}/health", timeout=5).json().get("status") != "ok":
            raise SystemExit(f"whisper-server at {url} is not healthy")

    def score(self, audio: np.ndarray, text: str) -> float:
        idx = np.arange(0, len(audio), OUT_RATE / PROMPT_RATE)
        mono = np.interp(idx, np.arange(len(audio)), audio).astype(np.float32)
        r = requests.post(f"{self.url}/inference", files={"file": ("take.wav", wav_bytes(mono, PROMPT_RATE), "audio/wav")},
                          data={"language": "en", "response_format": "json", "temperature": "0"}, timeout=120)
        r.raise_for_status()
        heard = self.tr(r.json().get("text", ""))
        return self.wer(self.tr(text), heard) if heard else 1.0


def verified_take(port: int, text: str, reference_wav: bytes, verifier: Verifier) -> tuple[np.ndarray, float]:
    best, total = None, 0.0
    for seed in RETRY_SEEDS[:RETRY_TAKES]:
        audio, wall = synth_call(port, text, reference_wav, None, seed=seed)
        total += wall
        wer = verifier.score(audio, text)
        if best is None or wer < best[0]:
            best = (wer, audio)
        if wer <= FAITHFUL_WER:
            break
    return best[1], total


def synth_call(port: int, text: str, reference_wav: bytes, prompt_text: str | None, seed: int = 42) -> tuple[np.ndarray, float]:
    body = {"input": text, "reference_audio": base64.b64encode(reference_wav).decode(), "response_format": "wav", "seed": seed}
    if prompt_text:
        body["prompt_text"] = prompt_text
    t0 = time.perf_counter()
    r = requests.post(f"http://127.0.0.1:{port}/v1/audio/speech", json=body, timeout=600)
    r.raise_for_status()
    audio, rate = sf.read(io.BytesIO(r.content), dtype="float32")
    if rate != OUT_RATE:
        raise SystemExit(f"server returned {rate} Hz, expected {OUT_RATE}")
    return audio, time.perf_counter() - t0


def cmd_synth(a):
    prompts = read_jsonl(Path(a.prompts))
    english = {r["id"]: r["hyp"] for r in read_jsonl(Path(a.english))}
    out = Path(a.out) / a.arm
    out.mkdir(parents=True, exist_ok=True)
    reference = Path(a.reference).read_bytes() if a.reference else None
    if a.arm == "r" and reference is None:
        raise SystemExit("arm r needs --reference")
    turns = read_jsonl(Path(a.turns)) if a.turns else []
    if a.arm in ("pcs", "pcsv") and not turns:
        raise SystemExit(f"arm {a.arm} needs --turns (turns.py output)")
    verifier = (ServerVerifier(a.verify_url) if a.verify_url else Verifier()) if a.arm == "pcsv" else None
    rows = []
    for p in prompts:
        if not p["usable"] or not english.get(p["id"]):
            continue
        if a.limit and len(rows) >= a.limit:
            break
        text = english[p["id"]]
        if a.arm == "p":
            audio, wall = synth_call(a.port, text, Path(p["prompt_wav"]).read_bytes(), p["src"])
        elif a.arm == "pt":
            # The tree's knob for a failed English rate: the same Japanese prompt
            # audio, but its text given in English (the line's own translation),
            # so the continuation is steered into English by the text stream.
            audio, wall = synth_call(a.port, text, Path(p["prompt_wav"]).read_bytes(), text)
        elif a.arm == "pc":
            # Per-line REFERENCE, no continuation: the line's own slice fixes the
            # voice (timbre, energy) through the reference path, and the English
            # is generated fresh, so nothing continues Japanese audio.
            audio, wall = synth_call(a.port, text, Path(p["prompt_wav"]).read_bytes(), None)
        elif a.arm == "pcs":
            # Speaker-turn reference (D3a): the turn that covers most of the
            # cue names the speaker; the reference is that speaker's own turns
            # nearest the cue, up to REFERENCE_MAX_S, nothing else.
            ref = speaker_reference(a.stem, turns, p["start_ms"], p["end_ms"])
            if ref is None:
                continue
            audio, wall = synth_call(a.port, text, ref, None)
        elif a.arm == "pcsv":
            # Speaker-turn reference + content verification: generate, transcribe
            # back, keep the first take whose text holds (WER <= FAITHFUL_WER),
            # else the best of RETRY_TAKES. Different seeds per take.
            ref = speaker_reference(a.stem, turns, p["start_ms"], p["end_ms"])
            if ref is None:
                continue
            audio, wall = verified_take(a.port, text, ref, verifier)
        elif a.arm == "pw":
            # Windowed continuation: the scene BEFORE the line is the prompt, with
            # those cues' English as its text; the model continues the scene.
            start_s = p["start_ms"] / 1000 - WINDOW_BEFORE_S
            before = [q for q in prompts if q["start_ms"] >= start_s * 1000 and q["end_ms"] <= p["start_ms"] and english.get(q["id"])]
            prompt_text = " ".join(english[q["id"]] for q in before)
            if not prompt_text:
                continue
            audio, wall = synth_call(a.port, text, window_wav(a.stem, start_s, p["start_ms"] / 1000), prompt_text)
        elif a.arm == "pcw":
            # Windowed reference: the scene AROUND the line as the voice reference.
            mid = (p["start_ms"] + p["end_ms"]) / 2000
            audio, wall = synth_call(a.port, text, window_wav(a.stem, mid - WINDOW_AROUND_S / 2, mid + WINDOW_AROUND_S / 2), None)
        else:
            audio, wall = synth_call(a.port, text, reference, None)
        path = out / f"{p['id']:03d}.wav"
        sf.write(path, audio, OUT_RATE)
        rows.append({"id": p["id"], "text": text, "wall_s": round(wall, 3), "audio_s": round(len(audio) / OUT_RATE, 2),
                     "prompt_s": p["prompt_s"], "wav": str(path)})
        print(f"{a.arm} {p['id']:03d} {wall:.2f}s -> {len(audio) / OUT_RATE:.2f}s | {text[:60]}", flush=True)
    write_jsonl(Path(a.out) / f"{a.arm}.jsonl", rows)


def whisper_english(model, wav_path: str) -> tuple[str, str]:
    audio, rate = sf.read(wav_path, dtype="float32")
    if audio.ndim > 1:
        audio = audio.mean(axis=1)
    if rate != PROMPT_RATE:
        idx = np.arange(0, len(audio), rate / PROMPT_RATE)
        audio = np.interp(idx, np.arange(len(audio)), audio).astype(np.float32)
    lang = model.auto_detect_language(audio)[0][0]
    segs = model.transcribe(audio, language="en", translate=False, no_context=True, print_progress=False, print_realtime=False)
    return lang, " ".join(s.text.strip() for s in segs)


def cmd_score(a):
    import jiwer
    from funasr import AutoModel
    from pywhispercpp.model import Model
    from resemblyzer import VoiceEncoder, preprocess_wav

    root = Path(a.out)
    prompts = {r["id"]: r for r in read_jsonl(root / "prompts.jsonl")}
    whisper = Model(WHISPER_MODEL, n_threads=8, print_progress=False, print_realtime=False, redirect_whispercpp_logs_to=False)
    # emotion2vec on the GPU: the scorer's CPU time is whisper.cpp's alone.
    emotion = AutoModel(model=EMOTION_MODEL, disable_update=True, device="cuda")
    speaker = VoiceEncoder()
    tr = jiwer.Compose([jiwer.ToLowerCase(), jiwer.RemovePunctuation(), jiwer.RemoveMultipleSpaces(), jiwer.Strip()])

    def emotion_vec(path: str) -> np.ndarray:
        res = emotion.generate(path, granularity="utterance", extract_embedding=False)
        return np.asarray(res[0]["scores"], dtype=np.float64)

    def spk(path: str) -> np.ndarray:
        return speaker.embed_utterance(preprocess_wav(Path(path)))

    overlap_path = root / "prompts.overlap.jsonl"
    overlapped = {r["id"] for r in read_jsonl(overlap_path) if r["overlapped"]} if overlap_path.exists() else set()
    src_cache = {}
    for arm in a.arm:
        rows = read_jsonl(root / f"{arm}.jsonl")
        english, wers, emo_cos, emo_top, spk_cos, records = [], [], [], [], [], []
        for r in rows:
            p = prompts[r["id"]]
            lang, heard = whisper_english(whisper, r["wav"])
            english.append(lang == "en")
            wer = jiwer.wer(tr(r["text"]), tr(heard)) if tr(heard) else 1.0
            wers.append(wer)
            if r["id"] not in src_cache:
                src_cache[r["id"]] = (emotion_vec(p["prompt_wav"]), spk(p["prompt_wav"]))
            e_src, s_src = src_cache[r["id"]]
            e_dub, s_dub = emotion_vec(r["wav"]), spk(r["wav"])
            emo_cos.append(float(e_src @ e_dub / (np.linalg.norm(e_src) * np.linalg.norm(e_dub) + 1e-9)))
            emo_top.append(int(e_src.argmax() == e_dub.argmax()))
            spk_cos.append(float(s_src @ s_dub / (np.linalg.norm(s_src) * np.linalg.norm(s_dub) + 1e-9)))
            # Per-line record: what came back, so a garbled line can be found and heard.
            records.append({"id": r["id"], "text": r["text"], "heard": heard, "lang": lang, "wer": round(wer, 3),
                            "emotion_cosine": round(emo_cos[-1], 3), "speaker_cosine": round(spk_cos[-1], 3),
                            "runaway": r["audio_s"] >= RUNAWAY_S})
        write_jsonl(root / f"{arm}.scored.jsonl", records)
        summary = {
            "arm": arm, "n": len(rows), "english_rate": round(sum(english) / len(rows), 3),
            "content_faithful_rate": round(sum(w <= FAITHFUL_WER for w in wers) / len(rows), 3),
            "runaways": sum(r["runaway"] for r in records),
            "wer_median": round(statistics.median(wers), 3), "wer_mean": round(statistics.mean(wers), 3),
            "emotion_cosine_mean": round(statistics.mean(emo_cos), 3), "emotion_top_agreement": round(statistics.mean(emo_top), 3),
            "speaker_cosine_mean": round(statistics.mean(spk_cos), 3),
            "wall_median_s": round(statistics.median(r["wall_s"] for r in rows), 3),
            "rtf_median": round(statistics.median(r["wall_s"] / r["audio_s"] for r in rows), 3),
        }
        # The same numbers on cues whose slice holds ONE voice: what per-speaker
        # separation would give this arm (Michael heard the emotion of the
        # other speaker in an overlapped cue).
        clean = [i for i, r in enumerate(rows) if r["id"] not in overlapped]
        if overlapped and clean:
            summary["clean_subset"] = {
                "n": len(clean),
                "english_rate": round(sum(english[i] for i in clean) / len(clean), 3),
                "content_faithful_rate": round(sum(wers[i] <= FAITHFUL_WER for i in clean) / len(clean), 3),
                "emotion_cosine_mean": round(statistics.mean(emo_cos[i] for i in clean), 3),
                "emotion_top_agreement": round(statistics.mean(emo_top[i] for i in clean), 3),
                "speaker_cosine_mean": round(statistics.mean(spk_cos[i] for i in clean), 3),
            }
        print(json.dumps(summary))
        (root / f"{arm}.summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("prompts"); p.add_argument("cues"); p.add_argument("aligned"); p.add_argument("stem"); p.add_argument("out"); p.set_defaults(fn=cmd_prompts)
    s = sub.add_parser("synth"); s.add_argument("prompts"); s.add_argument("english"); s.add_argument("out"); s.add_argument("--arm", choices=["p", "r", "pt", "pc", "pw", "pcw", "pcs", "pcsv"], required=True)
    s.add_argument("--reference"); s.add_argument("--stem", help="16 kHz mono dialogue stem (windowed and turn arms)")
    s.add_argument("--turns", help="turns.jsonl from turns.py (arm pcs)")
    s.add_argument("--verify-url", help="whisper-server url for the retry verifier (default: CPU whisper in-process)")
    s.add_argument("--port", type=int, default=8090); s.add_argument("--limit", type=int, default=0); s.set_defaults(fn=cmd_synth)
    sc = sub.add_parser("score"); sc.add_argument("out"); sc.add_argument("--arm", action="append", required=True); sc.set_defaults(fn=cmd_score)
    ov = sub.add_parser("overlap"); ov.add_argument("prompts"); ov.set_defaults(fn=cmd_overlap)
    a = ap.parse_args()
    a.fn(a)


if __name__ == "__main__":
    main()
