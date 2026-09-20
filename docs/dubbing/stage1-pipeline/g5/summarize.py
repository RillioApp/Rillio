"""Add the clean-subset block (cues whose prompt slice holds one voice) to every
arm's summary from its per-line records, without re-running any model.

    python summarize.py <g5_dir> [--arm p --arm r ...]   (default: every <arm>.scored.jsonl present)
"""
import argparse
import json
import statistics
from pathlib import Path

from run import FAITHFUL_WER, read_jsonl


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("g5", type=Path); ap.add_argument("--arm", action="append")
    a = ap.parse_args()
    overlapped = {r["id"] for r in read_jsonl(a.g5 / "prompts.overlap.jsonl") if r["overlapped"]}
    arms = a.arm or sorted(p.name[:-len(".scored.jsonl")] for p in a.g5.glob("*.scored.jsonl"))
    print("| arm | n | english | faithful | runaways | emotion cos | top agree | speaker cos | clean n | clean english | clean faithful | clean emotion | clean speaker |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for arm in arms:
        records = read_jsonl(a.g5 / f"{arm}.scored.jsonl")
        summary_path = a.g5 / f"{arm}.summary.json"
        summary = json.loads(summary_path.read_text(encoding="utf-8"))
        clean = [r for r in records if r["id"] not in overlapped]
        block = {
            "n": len(clean),
            "english_rate": round(sum(r["lang"] == "en" for r in clean) / len(clean), 3),
            "content_faithful_rate": round(sum(r["wer"] <= FAITHFUL_WER for r in clean) / len(clean), 3),
            "emotion_cosine_mean": round(statistics.mean(r["emotion_cosine"] for r in clean), 3),
            "speaker_cosine_mean": round(statistics.mean(r["speaker_cosine"] for r in clean), 3),
        }
        summary["clean_subset"] = block
        summary.setdefault("content_faithful_rate", round(sum(r["wer"] <= FAITHFUL_WER for r in records) / len(records), 3))
        summary.setdefault("runaways", sum(r["runaway"] for r in records))
        summary_path.write_text(json.dumps(summary, indent=2), encoding="utf-8")
        s = summary
        print(f"| {arm} | {s['n']} | {s['english_rate']} | {s['content_faithful_rate']} | {s['runaways']} | {s['emotion_cosine_mean']} | {s['emotion_top_agreement']} | {s['speaker_cosine_mean']} "
              f"| {block['n']} | {block['english_rate']} | {block['content_faithful_rate']} | {block['emotion_cosine_mean']} | {block['speaker_cosine_mean']} |")


if __name__ == "__main__":
    main()
