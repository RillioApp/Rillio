"""Merge the translator and TTS sidecar logs of one Rillio dub session into one
timeline (seconds since the translator started), to see where a window's time
goes. The sidecars stamp lines as m.ss.mmm.uuu since their own start."""
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
TTS_OFFSET = float(sys.argv[2])  # tts process start minus translator process start, seconds
STAMP = re.compile(r"^(\d+)\.(\d+)\.(\d+)\.(\d+) ")


def stamp(line):
    m = STAMP.match(line)
    return int(m[1]) * 60 + int(m[2]) + int(m[3]) / 1e3 if m else None


events = []
for line in (root / "translator.stderr.log").read_text("utf-8", "replace").splitlines():
    t = stamp(line)
    if t is None:
        continue
    if "launch_slot_" in line:
        events.append((t, "translate start"))
    elif "total time" in line:
        events.append((t, "translate done  " + line.split("total time =")[1].strip()))
last = None
for line in (root / "tts.stderr.log").read_text("utf-8", "replace").splitlines():
    t = stamp(line)
    if t is not None and "handle_audio_speech" in line:
        last = t + TTS_OFFSET
        events.append((last, "  tts start"))
    elif line.startswith("clone timing") and last is not None:
        parts = [float(x) for x in re.findall(r"([\d.]+) s", line)]
        events.append((last + sum(parts), f"  tts done ({sum(parts):.2f} s)"))
events.sort()
prev = None
for t, what in events:
    gap = "" if prev is None else f"+{t - prev:6.2f}"
    print(f"{t:8.2f} {gap:>8}  {what}")
    prev = t
