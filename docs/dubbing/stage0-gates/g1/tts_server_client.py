"""Warm per-call timing through llama-tts-server (the resident VoxCPM2 process):
init once, then N clone requests with the same reference. The server's stderr
carries the per-stage split ("clone timing: ..."), this prints wall time per
request and writes the wavs.

    python tts_server_client.py <base_lm.gguf> <acoustic.gguf> <reference.wav> <out_dir> [--n 3] [--port 8090]
"""
import argparse
import base64
import json
import time
from pathlib import Path

import requests

LINES = [
    "Remember we're both in the soup, if anything happens.",
    "You are crazy.",
    "Skipper, his body is plastered all over the communications room.",
]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("base_lm"); ap.add_argument("acoustic"); ap.add_argument("reference"); ap.add_argument("out")
    ap.add_argument("--n", type=int, default=3); ap.add_argument("--port", type=int, default=8090)
    a = ap.parse_args()
    base = f"http://127.0.0.1:{a.port}"
    out = Path(a.out); out.mkdir(parents=True, exist_ok=True)
    for _ in range(120):
        try:
            if requests.get(f"{base}/health", timeout=2).json().get("status") == "ok":
                break
        except requests.RequestException:
            time.sleep(1)
    else:
        raise SystemExit("server never became healthy")
    t0 = time.perf_counter()
    r = requests.post(f"{base}/v1/voxcpm2/init", json={"base_lm": a.base_lm, "acoustic": a.acoustic}, timeout=600)
    r.raise_for_status()
    print(f"init {time.perf_counter() - t0:.1f} s, sample_rate {r.json()['sample_rate']}")
    ref_b64 = base64.b64encode(Path(a.reference).read_bytes()).decode()
    for i in range(a.n):
        text = LINES[i % len(LINES)]
        t0 = time.perf_counter()
        r = requests.post(f"{base}/v1/audio/speech", json={"input": text, "reference_audio": ref_b64, "response_format": "wav"}, timeout=600)
        r.raise_for_status()
        wall = time.perf_counter() - t0
        wav = out / f"warm_{i:02d}.wav"
        wav.write_bytes(r.content)
        audio_s = (len(r.content) - 44) / 2 / 48_000
        print(json.dumps({"call": i, "wall_s": round(wall, 3), "audio_s": round(audio_s, 2), "rtf": round(wall / audio_s, 3), "text": text}))


if __name__ == "__main__":
    main()
