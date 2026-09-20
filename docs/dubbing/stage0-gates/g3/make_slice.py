"""F3: a seed-fixed 500-line slice from an OPUS OpenSubtitles v2024 Moses pair.

Held out from any later fine-tune by construction: the sampled line ids are
written into the slice. Lines are filtered to subtitle-shaped text (2..20
"units": words for space-delimited languages, characters/2 for CJK, no SDH
brackets, no music notes) so the gate scores dialogue, not credits. Context =
the 3 preceding corpus lines in the SOURCE language, which is what the player
would have at hand.

Usage: make_slice.py <pair_dir> <src_code> <tgt_code> <out.jsonl>
  e.g. make_slice.py E:/datasets/opus/en-ja ja en E:/datasets/opus/en-ja/f3-slice.jsonl
"""
import json
import random
import re
import sys
from pathlib import Path

SEED = 1
N = 500
MIN_UNITS, MAX_UNITS = 2, 20
CONTEXT = 3
SDH = re.compile(r"^[\[(♪].*[\])♪]$|^-?\s*[A-Z ]{3,}:")
CJK = re.compile(r"[\u3040-\u30ff\u3400-\u9fff\uac00-\ud7af]")


def units(text: str) -> int:
    return len(text) // 2 if CJK.search(text) else len(text.split())


def main(src_dir: Path, src_code: str, tgt_code: str, out: Path) -> None:
    src_path = next(src_dir.glob(f"*.{src_code}"))
    tgt_path = next(src_dir.glob(f"*.{tgt_code}"))
    # Moses files are aligned on "\n" ONLY. str.splitlines() also breaks on
    #  , \x0c, \x85 etc., which occur INSIDE lines on one side and not
    # the other (129 such lines in en-he), and the pair reads as misaligned.
    src = src_path.read_text(encoding="utf-8", errors="replace").split("\n")
    tgt = tgt_path.read_text(encoding="utf-8", errors="replace").split("\n")
    if src and src[-1] == "":
        src.pop()
    if tgt and tgt[-1] == "":
        tgt.pop()
    if len(src) != len(tgt):
        sys.exit(f"misaligned corpus: {len(src)} {src_code} vs {len(tgt)} {tgt_code} lines")
    eligible = [
        i for i in range(CONTEXT, len(src))
        if MIN_UNITS <= units(src[i]) <= MAX_UNITS
        and tgt[i].strip() and not SDH.match(src[i].strip()) and not SDH.match(tgt[i].strip())
    ]
    rng = random.Random(SEED)
    picks = sorted(rng.sample(eligible, N))
    rows = [
        {"id": i, "src": src[i].strip(), "ref": tgt[i].strip(),
         "context": [src[j].strip() for j in range(i - CONTEXT, i)]}
        for i in picks
    ]
    out.write_text("\n".join(json.dumps(r, ensure_ascii=False) for r in rows) + "\n", encoding="utf-8")
    print(f"{len(rows)} lines ({src_code}->{tgt_code}) from {len(src)} ({len(eligible)} eligible) -> {out}")


if __name__ == "__main__":
    main(Path(sys.argv[1]), sys.argv[2], sys.argv[3], Path(sys.argv[4]))
