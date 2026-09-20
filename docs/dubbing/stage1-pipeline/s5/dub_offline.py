"""S5: the whole dub of F6 to a file, offline, on the parts the gates chose.

Per speaker turn (turns.py): the whisper text over the turn -> the 2B on
llama-server with the G3 prompt and a length budget from the time room ->
VoxCPM2 on llama-tts-server with the chosen reference mechanism and the
verified retry -> placed at the turn start, fitted (atempo <= 15%, else cut
with a fade) -> mixed over the residual stem at the original dialogue's level.

    python dub_offline.py <out_dir> [--from-s 0 --to-s 180] [--reference pcs|pc] [--no-verify]

Inputs (fixed paths from Stage 0/1): the BS-RoFormer stems of F6, the 16 kHz
dialogue stem, turns.jsonl, whisper-cli's JSON (CUDA greedy run), the 2B GGUF.
Outputs: dub.wav (48 kHz stereo), lines.jsonl, summary.json.
"""
import argparse
import json
import subprocess
import sys
import time
from pathlib import Path

import numpy as np
import requests
import soundfile as sf

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "g5"))
sys.path.insert(0, str(HERE.parent.parent / "stage0-gates" / "g3"))
from run import ServerVerifier, Verifier, read_jsonl, speaker_reference, synth_call, verified_take, wav_bytes  # noqa: E402
from score import translate  # noqa: E402

# The separator works at 44.1 kHz; the dub timeline is 48 kHz, so the stems
# are resampled once (ffmpeg -ar 48000) and read from here.
RESIDUAL = r"E:\datasets\g0\stem\residual48k.wav"
VOCALS = r"E:\datasets\g0\stem\vocals48k.wav"
STEM16K = r"E:\datasets\g5\stem16k.wav"
TURNS = r"E:\datasets\g5\turns.jsonl"
WHISPER_JSON = r"E:\datasets\g0\whisper-cuda\asr-cuda-greedy.json"
TRANSLATOR_GGUF = r"E:\models\gguf\Qwen3.5-2B-GGUF\Qwen3.5-2B-Q8_0.gguf"
LLAMA_SERVER = r"E:\tools\llama.cpp\cuda\llama-server.exe"
LLAMA_PORT = 8080
TTS_PORT = 8090
OUT_RATE = 48_000
CONTEXT_TURNS = 3
# VoxCPM2's own English rate, measured on the G5 line-reference arm (median).
EN_CHARS_PER_S = 13.7
MIN_BUDGET_CHARS = 12
# G4's branch: stretch up to this, cut beyond it.
MAX_STRETCH = 1.15
CUT_FADE_MS = 60
# The voice never sits more than this above the original dialogue's level.
MAX_GAIN = 4.0
MIN_TURN_TEXT_CHARS = 2


def find_llama_server() -> str:
    hits = list(Path(LLAMA_SERVER).parent.rglob("llama-server.exe"))
    if not hits:
        raise SystemExit(f"no llama-server.exe under {Path(LLAMA_SERVER).parent}")
    return str(hits[0])


def start_translator() -> subprocess.Popen:
    proc = subprocess.Popen([find_llama_server(), "-m", TRANSLATOR_GGUF, "-ngl", "99", "-c", "4096", "--port", str(LLAMA_PORT),
                             "--host", "127.0.0.1", "--parallel", "1"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(120):
        try:
            if requests.get(f"http://127.0.0.1:{LLAMA_PORT}/health", timeout=2).json().get("status") == "ok":
                return proc
        except requests.RequestException:
            pass
        time.sleep(1)
    proc.kill()
    raise SystemExit("llama-server never became healthy")


def whisper_segments(path: str) -> list:
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    return [{"start_ms": s["offsets"]["from"], "end_ms": s["offsets"]["to"], "text": s["text"].strip()}
            for s in data["transcription"] if s["text"].strip() and not s["text"].strip().startswith("♪")]


def text_for_turn(segments: list, start_ms: int, end_ms: int) -> str:
    # A segment belongs to the turn holding its midpoint.
    return " ".join(s["text"] for s in segments if start_ms <= (s["start_ms"] + s["end_ms"]) / 2 < end_ms)


def fit(audio: np.ndarray, room_s: float) -> tuple[np.ndarray, str]:
    seconds = len(audio) / OUT_RATE
    if seconds <= room_s:
        return audio, "fits"
    ratio = seconds / room_s
    if ratio <= MAX_STRETCH:
        out = subprocess.run(["ffmpeg", "-v", "error", "-f", "f32le", "-ar", str(OUT_RATE), "-ac", "1", "-i", "-",
                              "-af", f"atempo={ratio:.4f}", "-f", "f32le", "-"], input=audio.astype(np.float32).tobytes(),
                             check=True, capture_output=True).stdout
        return np.frombuffer(out, dtype=np.float32), f"stretched x{ratio:.2f}"
    n = int(room_s * OUT_RATE)
    fade = min(n, int(CUT_FADE_MS / 1000 * OUT_RATE))
    cut = audio[:n].copy()
    cut[n - fade:] *= np.linspace(1.0, 0.0, fade, dtype=np.float32)
    return cut, f"cut at {room_s:.2f}s (needed {seconds:.2f}s)"


def rms(x: np.ndarray) -> float:
    return float(np.sqrt(np.mean(x.astype(np.float64) ** 2)) + 1e-9)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("out", type=Path)
    ap.add_argument("--from-s", type=float, default=0.0); ap.add_argument("--to-s", type=float, default=None)
    ap.add_argument("--reference", choices=["pcs", "pc"], default="pcs")
    ap.add_argument("--no-verify", action="store_true")
    ap.add_argument("--verify-url", help="whisper-server url for the retry verifier (GPU); default CPU whisper in-process")
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)

    residual, rate = sf.read(RESIDUAL, dtype="float32")
    vocals, _ = sf.read(VOCALS, dtype="float32")
    if rate != OUT_RATE:
        raise SystemExit(f"residual is {rate} Hz")
    stem16k, _ = sf.read(STEM16K, dtype="float32")
    turns = read_jsonl(Path(TURNS))
    segments = whisper_segments(WHISPER_JSON)
    to_s = a.to_s if a.to_s is not None else len(residual) / OUT_RATE
    selected = [t for t in turns if t["start_ms"] >= a.from_s * 1000 and t["end_ms"] <= to_s * 1000]

    verifier = None if a.no_verify else (ServerVerifier(a.verify_url) if a.verify_url else Verifier())
    translator = start_translator()
    mix = residual.copy()
    lines, t_wall0 = [], time.perf_counter()
    try:
        history = []
        for i, t in enumerate(selected):
            src = text_for_turn(segments, t["start_ms"], t["end_ms"])
            if len(src) < MIN_TURN_TEXT_CHARS:
                continue
            next_start = selected[i + 1]["start_ms"] if i + 1 < len(selected) else t["end_ms"] + 5000
            room_s = (next_start - t["start_ms"]) / 1000
            budget = max(MIN_BUDGET_CHARS, int(room_s * EN_CHARS_PER_S))
            en, t_mt = translate(f"http://127.0.0.1:{LLAMA_PORT}", {"src": src, "context": history[-CONTEXT_TURNS:]}, "dub", 60.0,
                                 "English", "Japanese", budget_chars=budget)
            history.append(src)
            if not en:
                continue
            if a.reference == "pcs":
                ref = speaker_reference(STEM16K, turns, t["start_ms"], t["end_ms"])
            else:
                ref = wav_bytes(stem16k[t["start_ms"] * 16:t["end_ms"] * 16], 16_000)
            if ref is None:
                continue
            if verifier:
                audio, t_tts = verified_take(TTS_PORT, en, ref, verifier)
            else:
                audio, t_tts = synth_call(TTS_PORT, en, ref, None)
            audio, fitted = fit(audio, room_s)
            # Level: match the original dialogue over this turn.
            orig = vocals[t["start_ms"] * OUT_RATE // 1000:t["end_ms"] * OUT_RATE // 1000]
            gain = min(MAX_GAIN, rms(orig) / rms(audio)) if len(orig) else 1.0
            start = t["start_ms"] * OUT_RATE // 1000
            end = min(len(mix), start + len(audio))
            mix[start:end] += (audio[:end - start] * gain)[:, None]
            line = {"turn": i, "start_ms": t["start_ms"], "end_ms": t["end_ms"], "speaker": t["speaker"], "src": src, "en": en,
                    "audio_s": round(len(audio) / OUT_RATE, 2), "room_s": round(room_s, 2), "fit": fitted, "gain": round(gain, 2),
                    "t_mt": round(t_mt, 3), "t_tts": round(t_tts, 3)}
            lines.append(line)
            print(f"{t['start_ms'] / 1000:7.1f}s {fitted:<22} {en[:70]}", flush=True)
    finally:
        translator.kill()
    np.clip(mix, -1.0, 1.0, out=mix)
    sf.write(a.out / "dub.wav", mix, OUT_RATE)
    (a.out / "lines.jsonl").write_text("".join(json.dumps(l, ensure_ascii=False) + "\n" for l in lines), encoding="utf-8")
    wall = time.perf_counter() - t_wall0
    audio_span = to_s - a.from_s
    summary = {
        "reference": a.reference, "verify": not a.no_verify, "turns": len(selected), "lines": len(lines),
        "fits": sum(l["fit"] == "fits" for l in lines), "stretched": sum(l["fit"].startswith("stretched") for l in lines),
        "cut": sum(l["fit"].startswith("cut") for l in lines), "wall_s": round(wall, 1), "audio_s": round(audio_span, 1),
        "rtf": round(wall / audio_span, 3), "mt_s": round(sum(l["t_mt"] for l in lines), 1), "tts_s": round(sum(l["t_tts"] for l in lines), 1),
    }
    (a.out / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
