"""G3 harness: translate F3 through a llama-server (OpenAI-compatible) and score.

One model per invocation; the server is started by the caller with the model
under test, so this file never knows a path. Every line is sent with its 3
context lines and a length hint; latency is wall-clock per request. Output:
one JSONL of hypotheses (for the per-class breakdown and for G4) and a
summary line with chrF++ (sacrebleu, word order 2), median/p90 latency, and
the length ratio, in the shape the ledger banks.

Control arms (R5) run through the same code path: --copy scores the source
text itself against the reference (the floor); the ceiling is simply the 4B
model through this same harness.
"""
import argparse
import json
import statistics
import sys
import time
from pathlib import Path

import requests
import sacrebleu

SYSTEM = (
    "You translate film subtitle lines from {src} to {lang}. Output ONLY the "
    "{lang} translation of the LAST line, nothing else: no quotes, no notes. Keep "
    "it as short and natural as spoken dialogue; the translation must be readable "
    "in the same time as the original, so it should not be longer than about "
    "{budget} characters."
)


def translate(server: str, row: dict, model: str, timeout: float, lang: str, src: str, budget_chars: int | None = None) -> tuple[str, float]:
    # CJK sources are ~2x denser than their English rendering; the budget is
    # in TARGET characters, so scale it by the source's script. A caller that
    # knows the time room (the dub worker, G4's branch) passes its own budget.
    dense = any("぀" <= ch <= "鿿" or "가" <= ch <= "힯" for ch in row["src"])
    budget = budget_chars if budget_chars is not None else max(12, int(len(row["src"]) * (2.4 if dense else 1.1)))
    messages = [
        {"role": "system", "content": SYSTEM.format(budget=budget, lang=lang, src=src)},
        {"role": "user", "content": "Previous lines:\n" + "\n".join(row["context"]) + "\n\nLine to translate:\n" + row["src"]},
    ]
    t0 = time.perf_counter()
    r = requests.post(
        f"{server}/v1/chat/completions",
        json={"model": model, "messages": messages, "temperature": 0, "max_tokens": 96,
              "chat_template_kwargs": {"enable_thinking": False}},
        timeout=timeout,
    )
    r.raise_for_status()
    text = r.json()["choices"][0]["message"]["content"].strip()
    # A model that still thinks aloud despite the flag is scored on its answer only.
    if "</think>" in text:
        text = text.split("</think>", 1)[1].strip()
    return text.splitlines()[0].strip() if text else "", time.perf_counter() - t0


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("slice", type=Path)
    ap.add_argument("--server", default="http://127.0.0.1:8080")
    ap.add_argument("--model", required=True, help="label for the ledger")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--copy", action="store_true", help="control: score the source as the hypothesis")
    ap.add_argument("--lang", default="English", help="target language name for the prompt")
    ap.add_argument("--src-lang", default="Japanese", help="source language name for the prompt")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--timeout", type=float, default=60.0)
    args = ap.parse_args()

    rows = [json.loads(l) for l in args.slice.read_text(encoding="utf-8").splitlines() if l.strip()]
    if args.limit:
        rows = rows[: args.limit]
    hyps, refs, lats = [], [], []
    with args.out.open("w", encoding="utf-8") as f:
        for i, row in enumerate(rows):
            if args.copy:
                hyp, lat = row["src"], 0.0
            else:
                hyp, lat = translate(args.server, row, args.model, args.timeout, args.lang, args.src_lang)
            hyps.append(hyp)
            refs.append(row["ref"])
            lats.append(lat)
            f.write(json.dumps({**row, "hyp": hyp, "latency_s": round(lat, 3)}, ensure_ascii=False) + "\n")
            if (i + 1) % 50 == 0:
                print(f"  {i + 1}/{len(rows)}", file=sys.stderr)

    chrf = sacrebleu.corpus_chrf(hyps, [refs], word_order=2)
    # Classes by REFERENCE length: the source may be a script without spaces.
    short = [k for k, r in enumerate(rows) if len(r["ref"].split()) <= 6]
    long_ = [k for k, r in enumerate(rows) if len(r["ref"].split()) > 6]
    by_class = {
        "short": sacrebleu.corpus_chrf([hyps[k] for k in short], [[refs[k] for k in short]], word_order=2).score if short else None,
        "long": sacrebleu.corpus_chrf([hyps[k] for k in long_], [[refs[k] for k in long_]], word_order=2).score if long_ else None,
    }
    ratio = statistics.median(len(h) / max(1, len(r)) for h, r in zip(hyps, refs))
    empty = sum(1 for h in hyps if not h)
    summary = {
        "model": args.model, "pair": f"{args.src_lang}->{args.lang}", "n": len(rows), "chrf++": round(chrf.score, 2),
        "chrf++_short": None if by_class["short"] is None else round(by_class["short"], 2),
        "chrf++_long": None if by_class["long"] is None else round(by_class["long"], 2),
        "latency_median_s": round(statistics.median(lats), 3),
        "latency_p90_s": round(sorted(lats)[int(0.9 * (len(lats) - 1))], 3),
        "len_ratio_hyp_over_ref": round(ratio, 3), "empty_outputs": empty,
    }
    print(json.dumps(summary, ensure_ascii=False))


if __name__ == "__main__":
    main()
