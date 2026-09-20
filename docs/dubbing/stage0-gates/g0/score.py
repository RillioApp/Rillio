"""G0 scoring: whisper text per official English cue -> the G3 prompt through
llama-server -> chrF++ against the official English line.

    python score.py cues   <en.srt> <cues.jsonl>
    python score.py align  <cues.jsonl> <asr.jsonl> <aligned.jsonl>   (source = ASR text overlapping the cue window)
    python score.py run    <aligned.jsonl> <out.jsonl> --model <label> [--server] [--copy] [--src-lang Japanese]
    python score.py fromsub <cues.jsonl> <other.srt> <aligned.jsonl>   (control: another official track as source)

The translation call is `g3/score.translate`, verbatim: one owner of the prompt.
"""
import argparse
import json
import re
import statistics
import sys
from pathlib import Path

import sacrebleu

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "g3"))
from score import translate  # noqa: E402

CUE_RE = re.compile(r"(\d+):(\d{2}):(\d{2})[,.](\d{3}) --> (\d+):(\d{2}):(\d{2})[,.](\d{3})")
TAG_RE = re.compile(r"<[^>]+>|\{\\[^}]*\}")
POSITIONED_RE = re.compile(r"\{\\an8\}")
CONTEXT_LINES = 3
WORD_ORDER = 2
SHORT_WORDS = 6


def ms(h, m, s, f) -> int:
    return int(h) * 3_600_000 + int(m) * 60_000 + int(s) * 1000 + int(f)


def read_srt(path: Path) -> list:
    rows, block = [], []
    for line in path.read_text(encoding="utf-8", errors="replace").split("\n") + [""]:
        if line.strip():
            block.append(line)
            continue
        if len(block) >= 3 and (m := CUE_RE.match(block[1])):
            raw = " ".join(block[2:])
            text = re.sub(r"\s+", " ", TAG_RE.sub("", raw)).strip()
            # A top-positioned cue with no lowercase letter is typeset on
            # screen (the episode title card), not spoken: no audio to score.
            typeset = POSITIONED_RE.search(raw) and not any(c.islower() for c in text)
            if text and not typeset:
                rows.append({"start_ms": ms(*m.groups()[:4]), "end_ms": ms(*m.groups()[4:]), "text": text})
        block = []
    return rows


def read_jsonl(path: Path) -> list:
    return [json.loads(l) for l in path.read_text(encoding="utf-8").split("\n") if l.strip()]


def write_jsonl(path: Path, rows: list) -> None:
    path.write_text("".join(json.dumps(r, ensure_ascii=False) + "\n" for r in rows), encoding="utf-8")


def cmd_cues(a):
    rows = read_srt(Path(a.srt))
    write_jsonl(Path(a.out), [{"id": i, **r} for i, r in enumerate(rows)])
    print(f"{len(rows)} cues -> {a.out}")


def overlap(a0, a1, b0, b1) -> int:
    return max(0, min(a1, b1) - max(a0, b0))


def cmd_align(a):
    cues, asr = read_jsonl(Path(a.cues)), read_jsonl(Path(a.asr))
    out = []
    for c in cues:
        # Every ASR segment that overlaps the cue window, in time order; a
        # segment straddling two cues is given to both (the translator sees
        # a little extra rather than a hole).
        src = " ".join(s["text"] for s in asr if overlap(c["start_ms"], c["end_ms"], s["start_ms"], s["end_ms"]) > 0)
        out.append({**c, "ref": c["text"], "src": src})
    for i, r in enumerate(out):
        r["context"] = [x["src"] for x in out[max(0, i - CONTEXT_LINES):i] if x["src"]]
    write_jsonl(Path(a.out), out)
    empty = sum(1 for r in out if not r["src"])
    print(f"{len(out)} cues aligned, {empty} with no ASR text -> {a.out}")


def cmd_fromsub(a):
    cues, other = read_jsonl(Path(a.cues)), read_srt(Path(a.srt))
    out = []
    for c in cues:
        src = " ".join(s["text"] for s in other if overlap(c["start_ms"], c["end_ms"], s["start_ms"], s["end_ms"]) > 0)
        out.append({**c, "ref": c["text"], "src": src})
    for i, r in enumerate(out):
        r["context"] = [x["src"] for x in out[max(0, i - CONTEXT_LINES):i] if x["src"]]
    write_jsonl(Path(a.out), out)
    print(f"{len(out)} cues from {a.srt}, {sum(1 for r in out if not r['src'])} empty -> {a.out}")


def cmd_run(a):
    rows = read_jsonl(Path(a.src))
    hyps, refs, lats = [], [], []
    with Path(a.out).open("w", encoding="utf-8") as f:
        for i, r in enumerate(rows):
            if a.copy or not r["src"]:
                hyp, lat = r["src"], 0.0
            else:
                hyp, lat = translate(a.server, r, a.model, 60.0, "English", a.src_lang)
            hyps.append(hyp)
            refs.append(r["ref"])
            lats.append(lat)
            f.write(json.dumps({**r, "hyp": hyp, "latency_s": round(lat, 3)}, ensure_ascii=False) + "\n")
            if (i + 1) % 50 == 0:
                print(f"  {i + 1}/{len(rows)}", file=sys.stderr)
    chrf = sacrebleu.corpus_chrf(hyps, [refs], word_order=WORD_ORDER).score
    short = [k for k, r in enumerate(rows) if len(r["ref"].split()) <= SHORT_WORDS]
    long_ = [k for k, r in enumerate(rows) if len(r["ref"].split()) > SHORT_WORDS]
    cls = lambda idx: round(sacrebleu.corpus_chrf([hyps[k] for k in idx], [[refs[k] for k in idx]], word_order=WORD_ORDER).score, 2) if idx else None
    translated = [l for l, r in zip(lats, rows) if r["src"]]
    summary = {
        "model": a.model, "pair": f"{a.src_lang}->English", "n": len(rows), "chrf++": round(chrf, 2),
        "chrf++_short": cls(short), "chrf++_long": cls(long_),
        "empty_source": sum(1 for r in rows if not r["src"]), "empty_outputs": sum(1 for h in hyps if not h),
        "latency_median_s": round(statistics.median(translated), 3) if translated else None,
        "len_ratio_hyp_over_ref": round(statistics.median(len(h) / max(1, len(r)) for h, r in zip(hyps, refs)), 3),
    }
    print(json.dumps(summary, ensure_ascii=False))
    Path(a.out).with_suffix(".summary.txt").write_text(json.dumps(summary, ensure_ascii=False), encoding="utf-8")


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("cues"); c.add_argument("srt"); c.add_argument("out"); c.set_defaults(fn=cmd_cues)
    al = sub.add_parser("align"); al.add_argument("cues"); al.add_argument("asr"); al.add_argument("out"); al.set_defaults(fn=cmd_align)
    fs = sub.add_parser("fromsub"); fs.add_argument("cues"); fs.add_argument("srt"); fs.add_argument("out"); fs.set_defaults(fn=cmd_fromsub)
    r = sub.add_parser("run"); r.add_argument("src"); r.add_argument("out"); r.add_argument("--model", required=True)
    r.add_argument("--server", default="http://127.0.0.1:8080"); r.add_argument("--copy", action="store_true"); r.add_argument("--src-lang", default="Japanese")
    r.set_defaults(fn=cmd_run)
    a = ap.parse_args()
    a.fn(a)


if __name__ == "__main__":
    main()
