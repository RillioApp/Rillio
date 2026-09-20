"""S2 speed: normalize a captured ORT stderr log (ORT writes UTF-16 lines on Windows, Python UTF-8) and grep it.

Prints the node-placement summary (which nodes the CUDA EP left on CPU), the
BFC arena extension lines (how much device memory a run reserves), and any
error lines.

Run: E:\\venvs\\dubbing\\Scripts\\python.exe speed_log.py <log> [--pattern REGEX]
"""
import argparse
import re
from pathlib import Path


def normalize(path: Path) -> list[str]:
    raw = path.read_bytes()
    lines = []
    for chunk in raw.split(b"\n"):
        if b"\x00" in chunk:
            text = chunk.replace(b"\r", b"").decode("utf-16-le", errors="replace")
        else:
            text = chunk.decode("utf-8", errors="replace")
        lines.append(text.rstrip("\r\x00 "))
    return lines


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("log", type=Path)
    ap.add_argument("--pattern", default=r"placed on|Number of nodes|CPUExecutionProvider\]|Fallback|fallback|Extending BFCArena|Extended allocation|Error|error|Available memory")
    ap.add_argument("--max", type=int, default=60)
    ap.add_argument("--context", type=int, default=0, help="lines after each match to print (for the placement node lists)")
    args = ap.parse_args()
    lines = normalize(args.log)
    pat = re.compile(args.pattern)
    shown = 0
    extend_bytes = 0
    for i, line in enumerate(lines):
        if "Extending BFCArena for Cuda" in line:
            m = re.search(r"rounded_bytes:(\d+)", line)
            if m:
                extend_bytes += int(m.group(1))
        if pat.search(line) and shown < args.max:
            print(line[:300])
            for extra in lines[i + 1 : i + 1 + args.context]:
                print("    " + extra[:200])
            shown += 1
    print(f"[speed_log] {len(lines)} lines, {shown} shown, arena extensions total {extend_bytes / 1024**3:.2f} GiB")


if __name__ == "__main__":
    main()
