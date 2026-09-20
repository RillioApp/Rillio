"""G3 verdict: every banked arm as a fraction of the pair's 4B ceiling, with the
pre-registered branch applied mechanically (README, G3 "Decision tree").

    python verdict.py E:\\datasets\\opus            # markdown table on stdout

Rows are read from <root>/<pair>/g3/<label>.summary.txt; the copy-source
control is re-scored from its jsonl. A pair without a ceiling summary prints
PENDING rather than a raw claim (claims are fractions of the ceiling, never
raw).
"""
import json
import sys
from pathlib import Path

import sacrebleu

CEILING = "qwen3.5-4b-q8-cuda-ceiling"
CONTROL = "copy-source"
PASS_FRACTION = 0.85
FINE_TUNE_FRACTION = 0.70
MAX_MEDIAN_LATENCY_S = 0.5
WORD_ORDER = 2


def control_score(path: Path) -> float:
    rows = [json.loads(l) for l in path.read_text(encoding="utf-8").split("\n") if l.strip()]
    return sacrebleu.corpus_chrf([r["hyp"] for r in rows], [[r["ref"] for r in rows]], word_order=WORD_ORDER).score


def branch(fraction: float, latency: float) -> str:
    if latency > MAX_MEDIAN_LATENCY_S:
        return "FAIL latency"
    if fraction >= PASS_FRACTION:
        return "PASS: use as-is, fine-tune later"
    if fraction >= FINE_TUNE_FRACTION:
        return "Stage 2 fine-tune REQUIRED before shipping"
    return "FAIL: the 2B/4B carries the feature"


def main() -> None:
    root = Path(sys.argv[1])
    print("| pair | arm | chrF++ (short / long) | median s | of ceiling | branch |")
    print("|---|---|---|---|---|---|")
    for g3 in sorted(root.glob("*/g3")):
        summaries = {}
        for f in g3.glob("*.summary.txt"):
            text = f.read_text(encoding="utf-8").strip()
            if not text:
                continue  # an arm still running: run_arm.ps1 opens the file at start
            s = json.loads(text)
            summaries[s["model"]] = s
        if not summaries:
            continue
        pair = next(iter(summaries.values()))["pair"]
        ceiling = summaries.get(CEILING)
        control = g3 / f"{CONTROL}.jsonl"
        if control.exists():
            floor = control_score(control)
            frac = f"{floor / ceiling['chrf++']:.0%}" if ceiling else "PENDING"
            print(f"| {pair} | {CONTROL} (floor) | {floor:.2f} | 0 | {frac} | control |")
        for label, s in sorted(summaries.items(), key=lambda kv: kv[1]["chrf++"]):
            cls = f"{s['chrf++_short']} / {s['chrf++_long']}"
            if label == CEILING:
                print(f"| {pair} | {label} | {s['chrf++']} ({cls}) | {s['latency_median_s']} | 100% | ceiling |")
            elif ceiling:
                fraction = s["chrf++"] / ceiling["chrf++"]
                print(f"| {pair} | {label} | {s['chrf++']} ({cls}) | {s['latency_median_s']} | {fraction:.0%} | {branch(fraction, s['latency_median_s'])} |")
            else:
                print(f"| {pair} | {label} | {s['chrf++']} ({cls}) | {s['latency_median_s']} | PENDING | ceiling not banked |")


if __name__ == "__main__":
    main()
