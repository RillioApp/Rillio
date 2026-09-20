"""G4: does a dubbed line land in its cue window?

Faithful to the product path: Sintel's OFFICIAL German subtitle track carries
the same cue timings as the English one, so each German line is a source cue
with a known window. We translate DE->EN with the G3 winner (through the same
llama-server + prompt as G3), synthesize the English with G1's winning arm,
and measure ratio = audio seconds / cue seconds per line.

Pass line (pre-registered): >= 80% of lines within [0.7, 1.15].

Usage:
  fit.py cues   <Sintel.mkv> <out_dir>                 -> cues.jsonl (German text, ms windows)
  fit.py translate <cues.jsonl> <out.jsonl> [--server]  -> adds "en"
  fit.py synth <translated.jsonl> <wav_dir> --cli|--pytorch [--ref clone.wav] -> adds "audio_s"
  fit.py speech <cues.jsonl> <Sintel.mkv> <speech.jsonl>  -> original dialogue seconds per window (VAD, center channel)
  fit.py score <synth.jsonl> [--speech speech.jsonl]
"""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

CUE_RE = re.compile(r"(\d+):(\d{2}):(\d{2})[,.](\d{3}) --> (\d+):(\d{2}):(\d{2})[,.](\d{3})")
LOW, HIGH, PASS = 0.7, 1.15, 0.8
CLI = r"E:\src\llama.cpp-omni\build-cuda\bin\voxcpm2-cli.exe"
GGUF_BASE = r"E:\models\VoxCPM2-GGUF\VoxCPM2-BaseLM-F16.gguf"
GGUF_ACOUSTIC = r"E:\models\VoxCPM2-GGUF\VoxCPM2-Acoustic-F16.gguf"


def ms(h, m, s, f) -> int:
    return int(h) * 3_600_000 + int(m) * 60_000 + int(s) * 1000 + int(f)


LANGS = {"ger": "German", "fre": "French", "rus": "Russian"}


def cmd_cues(a):
    # Sintel has only 26 lines per track; several official tracks share the
    # same windows, so each extra language adds 26 (cue, source) pairs.
    out = Path(a.out_dir)
    out.mkdir(parents=True, exist_ok=True)
    rows = []
    for tag in a.langs.split(","):
        srt = out / f"sintel.{tag}.srt"
        subprocess.run(["ffmpeg", "-y", "-v", "error", "-i", a.mkv, "-map", f"0:s:m:language:{tag}", str(srt)], check=True)
        block = []
        for line in srt.read_text(encoding="utf-8", errors="replace").splitlines() + [""]:
            if line.strip():
                block.append(line)
                continue
            if len(block) >= 3 and (m := CUE_RE.match(block[1])):
                text = " ".join(block[2:]).strip()
                if text:
                    start, end = ms(*m.groups()[:4]), ms(*m.groups()[4:])
                    rows.append({"start_ms": start, "end_ms": end, "cue_s": round((end - start) / 1000, 3),
                                 "src_lang": LANGS[tag], "src": text})
            block = []
    (out / "cues.jsonl").write_text("\n".join(json.dumps(r, ensure_ascii=False) for r in rows) + "\n", encoding="utf-8")
    print(f"{len(rows)} cues ({a.langs}) -> {out / 'cues.jsonl'}")


def cmd_translate(a):
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "g3"))
    from score import translate  # the G3 prompt, verbatim: one owner
    rows = [json.loads(l) for l in Path(a.cues).read_text(encoding="utf-8").splitlines() if l.strip()]
    with Path(a.out).open("w", encoding="utf-8") as f:
        for i, r in enumerate(rows):
            ctx = [x["src"] for x in rows[max(0, i - 3):i] if x["src_lang"] == r["src_lang"]]
            hyp, lat = translate(a.server, {"src": r["src"], "context": ctx}, "g4", 60.0, "English", r["src_lang"])
            f.write(json.dumps({**r, "en": hyp, "latency_s": round(lat, 3)}, ensure_ascii=False) + "\n")
    print(f"translated {len(rows)} -> {a.out}")


def audio_seconds(wav: Path) -> float:
    import soundfile as sf
    info = sf.info(str(wav))
    return info.frames / info.samplerate


VAD_RATE = 16_000
VAD_FRAME_MS = 20
VAD_AGGRESSIVENESS = 3
VAD_GAP_FILL_FRAMES = 6  # 120 ms: pauses between words stay inside the speech span
RUNAWAY_RATIO = 3.0  # audio this far past the cue is a generation that never stopped


def speech_seconds(pcm: bytes) -> float:
    import webrtcvad
    vad = webrtcvad.Vad(VAD_AGGRESSIVENESS)
    n = VAD_RATE * VAD_FRAME_MS // 1000 * 2
    voiced = [vad.is_speech(pcm[i:i + n], VAD_RATE) for i in range(0, len(pcm) - n + 1, n)]
    # Short gaps BETWEEN voiced frames count as speech; leading and trailing
    # silence never does.
    last = None
    for i, v in enumerate(voiced):
        if not v:
            continue
        if last is not None and i - last - 1 <= VAD_GAP_FILL_FRAMES:
            voiced[last + 1:i] = [True] * (i - last - 1)
        last = i
    return sum(voiced) * VAD_FRAME_MS / 1000


def cmd_speech(a):
    # The original dialogue's own duration inside each cue window, from the
    # 5.1 center channel (dialogue lives there, the score mostly does not).
    rows = [json.loads(l) for l in Path(a.cues).read_text(encoding="utf-8").splitlines() if l.strip()]
    windows = sorted({(r["start_ms"], r["end_ms"]) for r in rows})
    out = []
    for start, end in windows:
        pcm = subprocess.run(
            ["ffmpeg", "-v", "error", "-ss", f"{start / 1000:.3f}", "-t", f"{(end - start) / 1000:.3f}", "-i", a.mkv,
             "-map", "0:a:0", "-af", "pan=mono|c0=FC", "-ar", str(VAD_RATE), "-f", "s16le", "-"],
            check=True, capture_output=True).stdout
        out.append({"start_ms": start, "end_ms": end, "speech_s": round(speech_seconds(pcm), 3)})
    Path(a.out).write_text("\n".join(json.dumps(r) for r in out) + "\n", encoding="utf-8")
    print(f"{len(out)} windows -> {a.out}")


def cmd_synth(a):
    rows = [json.loads(l) for l in Path(a.src).read_text(encoding="utf-8").splitlines() if l.strip()]
    wav_dir = Path(a.wav_dir)
    wav_dir.mkdir(parents=True, exist_ok=True)
    model = None
    if a.pytorch:
        sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
        import gates_env

        gates_env.point_cc_at_triton_tcc()
        from voxcpm import VoxCPM
        model = VoxCPM.from_pretrained(r"E:\models\VoxCPM2")
    out = Path(a.src).with_suffix(".synth.jsonl")
    with out.open("w", encoding="utf-8") as f:
        for i, r in enumerate(rows):
            wav = wav_dir / f"{i:03d}.wav"
            if a.pytorch:
                import soundfile as sf
                # Text-free cloning, the same contract as the CLI's -r.
                kwargs = {"reference_wav_path": a.ref} if a.ref else {}
                audio = model.generate(text=r["en"], **kwargs)
                sf.write(wav, audio, getattr(model, "sample_rate", 48_000))
            else:
                args = [CLI, "-t", r["en"], "-o", str(wav)] + (["-r", a.ref] if a.ref else []) + [GGUF_BASE, GGUF_ACOUSTIC]
                subprocess.run(args, check=True, capture_output=True)
            r["audio_s"] = round(audio_seconds(wav), 3)
            r["ratio"] = round(r["audio_s"] / r["cue_s"], 3)
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    print(f"synthesized {len(rows)} -> {out}")


def ratio_stats(ratios: list) -> dict:
    inside = sum(LOW <= x <= HIGH for x in ratios)
    s = sorted(ratios)
    return {
        "n": len(ratios), "within_window": inside, "fraction": round(inside / len(ratios), 3),
        "pass": inside / len(ratios) >= PASS,
        "ratio_median": s[len(s) // 2], "ratio_p10": s[int(0.1 * (len(s) - 1))], "ratio_p90": s[int(0.9 * (len(s) - 1))],
        "too_long": sum(x > HIGH for x in ratios), "too_short": sum(x < LOW for x in ratios),
    }


def cmd_score(a):
    rows = [json.loads(l) for l in Path(a.src).read_text(encoding="utf-8").splitlines() if l.strip()]
    if not rows:
        raise SystemExit(f"{a.src} holds no synthesized rows: synth failed upstream")
    # Pre-registered instrument: audio / cue window. Runaways (audio far past
    # the cue) are a generation defect, counted on their own.
    result = {"vs_cue": ratio_stats([r["ratio"] for r in rows]),
              "runaway": sum(r["ratio"] > RUNAWAY_RATIO for r in rows)}
    # What actually breaks a dub: audio still playing when the next cue starts.
    starts = sorted({r["start_ms"] for r in rows})
    next_start = {s: starts[i + 1] for i, s in enumerate(starts[:-1])}
    with_next = [r for r in rows if r["start_ms"] in next_start]
    collisions = sum(r["start_ms"] + r["audio_s"] * 1000 > next_start[r["start_ms"]] for r in with_next)
    result["next_cue"] = {"n": len(with_next), "collisions": collisions, "rate": round(collisions / len(with_next), 3)}
    if a.speech:
        # Post-hoc second denominator: the original dialogue's own duration,
        # so a short dub inside a padded window is not scored as a miss.
        speech = {(s["start_ms"], s["end_ms"]): s["speech_s"]
                  for s in (json.loads(l) for l in Path(a.speech).read_text(encoding="utf-8").splitlines() if l.strip())}
        paired = [(r, speech[(r["start_ms"], r["end_ms"])]) for r in rows]
        unvoiced = [r["en"] for r, s in paired if s == 0]
        if unvoiced:
            raise SystemExit(f"VAD found no speech in {len(unvoiced)} cue windows: {unvoiced[:3]}")
        result["vs_original_speech"] = ratio_stats([round(r["audio_s"] / s, 3) for r, s in paired])
    print(json.dumps(result))


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("cues"); c.add_argument("mkv"); c.add_argument("out_dir"); c.add_argument("--langs", default="ger,fre"); c.set_defaults(fn=cmd_cues)
    t = sub.add_parser("translate"); t.add_argument("cues"); t.add_argument("out"); t.add_argument("--server", default="http://127.0.0.1:8080"); t.set_defaults(fn=cmd_translate)
    s = sub.add_parser("synth"); s.add_argument("src"); s.add_argument("wav_dir"); s.add_argument("--pytorch", action="store_true"); s.add_argument("--cli", action="store_true"); s.add_argument("--ref"); s.set_defaults(fn=cmd_synth)
    v = sub.add_parser("speech"); v.add_argument("cues"); v.add_argument("mkv"); v.add_argument("out"); v.set_defaults(fn=cmd_speech)
    r = sub.add_parser("score"); r.add_argument("src"); r.add_argument("--speech", help="speech.jsonl from `speech`"); r.set_defaults(fn=cmd_score)
    a = ap.parse_args()
    a.fn(a)


if __name__ == "__main__":
    main()
