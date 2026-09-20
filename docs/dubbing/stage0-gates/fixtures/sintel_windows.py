"""F4 / F5 from Sintel: pick windows by the English subtitle track and cut them.

F4 = the 60 s window with the most dialogue (cue coverage), audio downmixed
to stereo 44.1 kHz, plus its cues as JSONL (video-time ms) for G4 and for
the by-ear clone check.
F5 background = the longest cue-free stretch >= 60 s (score + effects, no
speech) as the ground-truth "everything that is not the dialogue" bed.

Usage: sintel_windows.py <Sintel.mkv> <out_dir>
"""
import json
import re
import subprocess
import sys
from pathlib import Path

WINDOW_S = 60.0
CUE_RE = re.compile(r"(\d+):(\d{2}):(\d{2})[,.](\d{3}) --> (\d+):(\d{2}):(\d{2})[,.](\d{3})")


def ms(h, m, s, f) -> int:
    return int(h) * 3_600_000 + int(m) * 60_000 + int(s) * 1000 + int(f)


def read_cues(mkv: Path, out_dir: Path) -> list[dict]:
    srt = out_dir / "sintel.eng.srt"
    subprocess.run(["ffmpeg", "-y", "-v", "error", "-i", str(mkv), "-map", "0:s:m:language:eng", str(srt)], check=True)
    cues, block = [], []
    for line in srt.read_text(encoding="utf-8", errors="replace").splitlines() + [""]:
        if line.strip():
            block.append(line)
            continue
        if len(block) >= 3 and (m := CUE_RE.match(block[1])):
            text = " ".join(block[2:]).strip()
            if text:
                cues.append({"start_ms": ms(*m.groups()[:4]), "end_ms": ms(*m.groups()[4:]), "text": text})
        block = []
    return cues


def densest_window(cues: list[dict]) -> float:
    best, best_start = -1.0, 0.0
    for cue in cues:
        start = cue["start_ms"] / 1000
        end = start + WINDOW_S
        covered = sum(
            max(0.0, min(c["end_ms"] / 1000, end) - max(c["start_ms"] / 1000, start)) for c in cues
        )
        if covered > best:
            best, best_start = covered, start
    return best_start, best


def longest_silence(cues: list[dict], total_s: float) -> tuple[float, float]:
    edges = sorted((c["start_ms"] / 1000, c["end_ms"] / 1000) for c in cues)
    gaps, prev_end = [], 0.0
    for s, e in edges:
        if s - prev_end > 0:
            gaps.append((prev_end, s))
        prev_end = max(prev_end, e)
    gaps.append((prev_end, total_s))
    return max(gaps, key=lambda g: g[1] - g[0])


def cut(mkv: Path, start: float, length: float, out: Path) -> None:
    subprocess.run(["ffmpeg", "-y", "-v", "error", "-ss", f"{start:.3f}", "-t", f"{length:.3f}", "-i", str(mkv),
                    "-vn", "-ac", "2", "-ar", "44100", "-c:a", "pcm_s16le", str(out)], check=True)


def main(mkv: Path, out_dir: Path) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    cues = read_cues(mkv, out_dir)
    total = float(subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", str(mkv)],
                                 capture_output=True, text=True, check=True).stdout.strip().replace(",", "."))
    f4_start, covered = densest_window(cues)
    cut(mkv, f4_start, WINDOW_S, out_dir / "f4_dialogue.wav")
    window = [dict(c, start_ms=c["start_ms"] - int(f4_start * 1000), end_ms=c["end_ms"] - int(f4_start * 1000))
              for c in cues if c["end_ms"] / 1000 > f4_start and c["start_ms"] / 1000 < f4_start + WINDOW_S]
    (out_dir / "f4_cues.jsonl").write_text("\n".join(json.dumps(c, ensure_ascii=False) for c in window) + "\n", encoding="utf-8")
    gap_start, gap_end = longest_silence(cues, total)
    # Step 1 s in from the edges: a cue boundary is where a mouth opens.
    bg_start, bg_len = gap_start + 1.0, min(WINDOW_S, gap_end - gap_start - 2.0)
    cut(mkv, bg_start, bg_len, out_dir / "f5_background.wav")
    print(json.dumps({
        "cues_total": len(cues), "film_s": round(total, 1),
        "f4": {"start_s": round(f4_start, 3), "dialogue_s_in_window": round(covered, 1), "cues": len(window)},
        "f5_background": {"start_s": round(bg_start, 3), "length_s": round(bg_len, 1), "gap": [round(gap_start, 1), round(gap_end, 1)]},
    }))


if __name__ == "__main__":
    main(Path(sys.argv[1]), Path(sys.argv[2]))
