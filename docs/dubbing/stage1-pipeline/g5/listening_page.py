"""Build the G5 listening page: per cue, the original line (dialogue stem
slice), the prompt-continuation dub (arm p) and the reference-only dub
(arm r), with the texts and whisper's language read of each dub, as one
self-contained HTML file (audio as MP3 data URIs).

    python listening_page.py <g5_dir> <out.html> [--n 24] [--seed 1] [--arm p --arm r ...]
"""
import argparse
import base64
import html
import json
import random
import subprocess
from pathlib import Path

import numpy as np
import soundfile as sf
from pywhispercpp.model import Model

WHISPER_MODEL = r"E:\models\whisper\ggml-small-q5_1.bin"
MP3_BITRATE = "64k"


def read_jsonl(p: Path) -> list:
    return [json.loads(l) for l in p.read_text(encoding="utf-8").split("\n") if l.strip()]


def mp3_data_uri(wav: Path) -> str:
    out = subprocess.run(["ffmpeg", "-v", "error", "-i", str(wav), "-c:a", "libmp3lame", "-b:a", MP3_BITRATE, "-f", "mp3", "-"],
                         check=True, capture_output=True).stdout
    return "data:audio/mpeg;base64," + base64.b64encode(out).decode()


def heard_language(model: Model, wav: Path) -> str:
    audio, rate = sf.read(str(wav), dtype="float32")
    if audio.ndim > 1:
        audio = audio.mean(axis=1)
    if rate != 16_000:
        idx = np.arange(0, len(audio), rate / 16_000)
        audio = np.interp(idx, np.arange(len(audio)), audio).astype(np.float32)
    return model.auto_detect_language(audio)[0][0]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("g5", type=Path); ap.add_argument("out", type=Path)
    ap.add_argument("--n", type=int, default=24); ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--arm", action="append", help="arm label(s) in order; default p r")
    a = ap.parse_args()
    arms = a.arm or ["p", "r"]
    prompts = {r["id"]: r for r in read_jsonl(a.g5 / "prompts.jsonl")}
    by_arm = {arm: {r["id"]: r for r in read_jsonl(a.g5 / f"{arm}.jsonl")} for arm in arms}
    # Per-line scores when the scorer has run: what whisper heard back and the WER.
    scored = {arm: {r["id"]: r for r in read_jsonl(a.g5 / f"{arm}.scored.jsonl")} if (a.g5 / f"{arm}.scored.jsonl").exists() else {} for arm in arms}
    overlap = {r["id"]: r for r in read_jsonl(a.g5 / "prompts.overlap.jsonl")} if (a.g5 / "prompts.overlap.jsonl").exists() else {}
    ids = sorted(set.intersection(*(set(v) for v in by_arm.values())))
    random.Random(a.seed).shuffle(ids)
    chosen = sorted(ids[:a.n])
    whisper = Model(WHISPER_MODEL, n_threads=8, print_progress=False, print_realtime=False, redirect_whispercpp_logs_to=False)

    rows = []
    for i, cue_id in enumerate(chosen, 1):
        pr = prompts[cue_id]
        ov = overlap.get(cue_id, {})
        row = {"n": i, "id": cue_id, "src": pr["src"], "en": by_arm[arms[0]][cue_id]["text"], "prompt_s": pr["prompt_s"],
               "orig": mp3_data_uri(Path(pr["prompt_wav"])), "arms": {},
               "overlap_score": ov.get("overlap_score"), "overlapped": ov.get("overlapped", False)}
        for arm in arms:
            d = by_arm[arm][cue_id]
            sc = scored[arm].get(cue_id)
            row["arms"][arm] = {"uri": mp3_data_uri(Path(d["wav"])), "s": d["audio_s"],
                                "lang": sc["lang"] if sc else heard_language(whisper, Path(d["wav"])),
                                "heard": sc["heard"] if sc else None, "wer": sc["wer"] if sc else None}
        rows.append(row)
        print(f"{i}/{len(chosen)} cue {cue_id}", flush=True)

    labels = {"p": "Prompt dub", "pt": "Prompt, EN text", "pc": "Line reference", "r": "Fixed reference"}

    def row_html(x: dict) -> str:
        def pill(kind: str, label: str, seconds: float, lang: str | None, heard: str | None = None, wer: float | None = None) -> str:
            tag = f'<span class="lang" data-ok="{str(lang == "en").lower()}">{lang}</span>' if lang else ""
            button = (f'<button class="play" data-kind="{kind}" data-row="{x["n"]}" aria-label="Play {label}">'
                      f'<span class="dot"></span><span class="label">{label}</span><span class="dur">{seconds:.1f}s</span>{tag}</button>')
            if heard is None:
                return button
            drift = "drift" if wer is not None and wer > 0.2 else ""
            return f'<div class="arm">{button}<p class="heard {drift}"><span class="mono dim">WER {wer:.2f}</span> {html.escape(heard) if heard else "(nothing heard)"}</p></div>'
        return f"""
<li class="cue" id="cue-{x['n']}" data-cue="{x['id']}" data-overlapped="{str(x['overlapped']).lower()}">
  <div class="num"><span class="mono">{x['n']:02d}</span><span class="mono dim">#{x['id']}</span>{'<span class="flag" title="speaker-embedding spread across the slice; low = more than one voice">voices ' + format(x['overlap_score'], '.2f') + '</span>' if x['overlap_score'] is not None else ''}</div>
  <div class="text">
    <p class="ja" lang="ja">{html.escape(x['src'])}</p>
    <p class="en">{html.escape(x['en'])}</p>
  </div>
  <div class="players">
    {pill('orig', 'Original', x['prompt_s'], None)}
    {''.join(pill(arm, labels.get(arm, arm), x['arms'][arm]['s'], x['arms'][arm]['lang'], x['arms'][arm]['heard'], x['arms'][arm]['wer']) for arm in arms)}
  </div>
  <div class="verdict">
    <div class="tri" data-field="emotion" data-row="{x['n']}"><span class="key">Emotion</span><button data-v="kept">kept</button><button data-v="partly">partly</button><button data-v="lost">lost</button></div>
    <div class="tri" data-field="voice" data-row="{x['n']}"><span class="key">Voice</span><button data-v="kept">kept</button><button data-v="partly">partly</button><button data-v="lost">lost</button></div>
  </div>
  <audio data-row="{x['n']}" data-kind="orig" preload="none" src="{x['orig']}"></audio>
  {"".join(f'<audio data-row="{x["n"]}" data-kind="{arm}" preload="none" src="{x["arms"][arm]["uri"]}"></audio>' for arm in arms)}
</li>"""

    page = f"""<title>Tomb Raider King Dub Bench</title>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Bricolage+Grotesque:opsz,wght@12..96,500;12..96,700&family=Source+Sans+3:ital,wght@0,400;0,600;1,400&family=JetBrains+Mono:wght@400;500&display=swap">
<style>
:root {{
  --bg: #f6f4ef; --surface: #ecebe6; --ink: #1d1c1a; --ink-2: #5b5852; --ink-3: #8a867e;
  --line: rgba(29,28,26,0.08); --accent: #ffa033; --accent-ink: #1d1c1a; --good: #4f8a5b; --bad: #b5533f;
  --font-display: "Bricolage Grotesque", "Avenir Next", "Segoe UI", sans-serif;
  --font-body: "Source Sans 3", "Segoe UI", system-ui, sans-serif;
  --font-mono: "JetBrains Mono", "Cascadia Mono", Consolas, monospace;
}}
@media (prefers-color-scheme: dark) {{ :root:not([data-theme="light"]) {{
  --bg: #17181b; --surface: #202227; --ink: #ece9e2; --ink-2: #aaa69d; --ink-3: #75726b; --line: rgba(236,233,226,0.08);
  --accent: #ffa033; --accent-ink: #17181b; --good: #7cbf8a; --bad: #e0806b; }} }}
:root[data-theme="dark"] {{
  --bg: #17181b; --surface: #202227; --ink: #ece9e2; --ink-2: #aaa69d; --ink-3: #75726b; --line: rgba(236,233,226,0.08);
  --accent: #ffa033; --accent-ink: #17181b; --good: #7cbf8a; --bad: #e0806b; }}
body {{ background: var(--bg); color: var(--ink); font-family: var(--font-body); font-size: 15px; line-height: 1.45; margin: 0; }}
main {{ max-width: 76rem; margin: 0 auto; padding: 2.5rem 1.5rem 5rem; }}
header {{ display: flex; flex-wrap: wrap; align-items: end; justify-content: space-between; gap: 1rem 2rem; margin-bottom: 1.5rem; }}
h1 {{ font-family: var(--font-display); font-weight: 700; font-size: 1.9rem; letter-spacing: -0.01em; margin: 0; text-wrap: balance; }}
.sub {{ color: var(--ink-2); margin: 0.35rem 0 0; max-width: 60ch; }}
.eyebrow {{ font-family: var(--font-mono); font-size: 0.7rem; letter-spacing: 0.08em; text-transform: uppercase; color: var(--ink-3); }}
.tally {{ display: flex; gap: 1.5rem; font-family: var(--font-mono); font-size: 0.8rem; color: var(--ink-2); font-variant-numeric: tabular-nums; align-items: center; }}
.tally b {{ color: var(--ink); font-weight: 500; }}
.copy {{ font: inherit; font-size: 0.8rem; border: 0; border-radius: 9999px; padding: 0.4rem 0.9rem; background: var(--surface); color: var(--ink); cursor: pointer; }}
.copy:hover {{ filter: brightness(1.1); }}
.copy:focus-visible, .play:focus-visible, .tri button:focus-visible {{ outline: 3px solid var(--accent); outline-offset: 2px; }}
ol {{ list-style: none; margin: 0; padding: 0; border-top: 1px solid var(--line); }}
.cue {{ display: grid; grid-template-columns: 5rem minmax(18rem, 1.4fr) auto auto; gap: 0.75rem 1.5rem; align-items: start; padding: 1rem 0; border-bottom: 1px solid var(--line); }}
.num {{ display: flex; flex-direction: column; gap: 0.15rem; padding-top: 0.2rem; }}
.mono {{ font-family: var(--font-mono); font-size: 0.8rem; font-variant-numeric: tabular-nums; }}
.dim {{ color: var(--ink-3); }}
.flag {{ font-family: var(--font-mono); font-size: 0.66rem; letter-spacing: 0.04em; color: var(--ink-3); margin-top: 0.3rem; }}
.cue[data-overlapped="true"] .flag {{ color: var(--bad); }}
.text p {{ margin: 0; }}
.ja {{ font-size: 1rem; }}
.en {{ color: var(--ink-2); margin-top: 0.15rem !important; }}
.players {{ display: flex; flex-direction: column; gap: 0.4rem; min-width: 14rem; max-width: 24rem; }}
.arm {{ display: flex; flex-direction: column; gap: 0.15rem; }}
.heard {{ margin: 0 0 0 0.7rem; font-size: 0.78rem; color: var(--ink-3); line-height: 1.3; }}
.heard.drift {{ color: var(--bad); }}
.play {{ display: grid; grid-template-columns: 0.6rem 1fr auto auto; align-items: center; gap: 0.6rem; width: 100%; font: inherit; font-size: 0.85rem; text-align: left;
  border: 0; border-radius: 9999px; padding: 0.35rem 0.8rem 0.35rem 0.7rem; background: var(--surface); color: var(--ink); cursor: pointer; }}
.play:hover {{ filter: brightness(1.1); }}
.play .dot {{ width: 0.55rem; height: 0.55rem; border-radius: 9999px; background: var(--ink-3); }}
.play[data-playing="true"] {{ background: var(--accent); color: var(--accent-ink); }}
.play[data-playing="true"] .dot {{ background: var(--accent-ink); }}
.play .dur {{ font-family: var(--font-mono); font-size: 0.72rem; color: inherit; opacity: 0.7; }}
.lang {{ font-family: var(--font-mono); font-size: 0.68rem; letter-spacing: 0.06em; text-transform: uppercase; border-radius: 9999px; padding: 0.1rem 0.45rem; background: var(--bg); color: var(--ink-2); }}
.lang[data-ok="false"] {{ color: var(--bad); }}
.play[data-playing="true"] .lang {{ background: rgba(0,0,0,0.12); color: var(--accent-ink); }}
.verdict {{ display: flex; flex-direction: column; gap: 0.4rem; }}
.tri {{ display: flex; align-items: center; gap: 0.25rem; }}
.tri .key {{ font-family: var(--font-mono); font-size: 0.7rem; letter-spacing: 0.06em; text-transform: uppercase; color: var(--ink-3); width: 4.2rem; }}
.tri button {{ font: inherit; font-size: 0.78rem; border: 0; border-radius: 9999px; padding: 0.25rem 0.65rem; background: var(--surface); color: var(--ink-2); cursor: pointer; }}
.tri button[aria-pressed="true"][data-v="kept"] {{ background: var(--good); color: #fff; }}
.tri button[aria-pressed="true"][data-v="partly"] {{ background: var(--accent); color: var(--accent-ink); }}
.tri button[aria-pressed="true"][data-v="lost"] {{ background: var(--bad); color: #fff; }}
.hint {{ color: var(--ink-3); font-size: 0.85rem; margin: 0.75rem 0 0; }}
kbd {{ font-family: var(--font-mono); font-size: 0.75rem; background: var(--surface); border-radius: 0.3rem; padding: 0.05rem 0.35rem; }}
@media (max-width: 52rem) {{ .cue {{ grid-template-columns: 4rem 1fr; }} .players, .verdict {{ grid-column: 2; }} }}
@media (prefers-reduced-motion: no-preference) {{ .play {{ transition: background 120ms ease; }} }}
</style>
<main>
<header>
  <div>
    <div class="eyebrow">G5 listening set, {len(rows)} of 269 cues, seed {a.seed}</div>
    <h1>Tomb Raider King Dub Bench</h1>
    <p class="sub">Each row is one line of episode 9. Original is the dialogue stem slice VoxCPM2 was prompted with. Prompt dub continues that slice in English; Prompt, EN text continues it with the English line as the prompt's text; Line reference uses the slice only as a voice reference; Fixed reference uses one 15 s reference for every line. The small tag is what whisper heard the dub's language as.</p>
  </div>
  <div class="tally"><span>Emotion kept <b id="t-emotion">0</b></span><span>Voice kept <b id="t-voice">0</b></span><span>Judged <b id="t-done">0</b>/{len(rows)}</span><button class="copy" id="copy">Copy verdicts</button></div>
</header>
<ol>{"".join(row_html(x) for x in rows)}</ol>
<p class="hint">One clip plays at a time. With a row's clip playing, <kbd>1</kbd> is the Original and <kbd>2</kbd> onward the arms in order; <kbd>space</kbd> stops. Verdicts stay in this browser.</p>
</main>
<script>
(() => {{
  const KEY = 'g5-verdicts-seed-{a.seed}';
  let verdicts = {{}};
  try {{ verdicts = JSON.parse(localStorage.getItem(KEY) || '{{}}'); }} catch (e) {{ verdicts = {{}}; }}
  const audios = [...document.querySelectorAll('audio')];
  const plays = [...document.querySelectorAll('.play')];
  let current = null;
  function stop() {{ if (current) {{ current.pause(); current.currentTime = 0; }} current = null; plays.forEach(b => b.removeAttribute('data-playing')); }}
  function play(row, kind) {{
    const a = audios.find(x => x.dataset.row === row && x.dataset.kind === kind);
    if (!a) return;
    const same = current === a && !a.paused;
    stop();
    if (same) return;
    current = a; a.play();
    plays.find(b => b.dataset.row === row && b.dataset.kind === kind)?.setAttribute('data-playing', 'true');
  }}
  audios.forEach(a => a.addEventListener('ended', () => {{ if (current === a) stop(); }}));
  plays.forEach(b => b.addEventListener('click', () => play(b.dataset.row, b.dataset.kind)));
  document.addEventListener('keydown', e => {{
    if (e.target.tagName === 'INPUT') return;
    const kinds = {{ '1': 'orig', {', '.join(f"'{i + 2}': '{arm}'" for i, arm in enumerate(arms))} }};
    if (e.key === ' ') {{ e.preventDefault(); stop(); return; }}
    if (kinds[e.key] && current) {{ e.preventDefault(); play(current.dataset.row, kinds[e.key]); }}
  }});
  function paint() {{
    let emotion = 0, voice = 0, done = 0;
    document.querySelectorAll('.cue').forEach(li => {{
      const v = verdicts[li.dataset.cue] || {{}};
      li.querySelectorAll('.tri').forEach(t => t.querySelectorAll('button').forEach(b => b.setAttribute('aria-pressed', String(v[t.dataset.field] === b.dataset.v))));
      if (v.emotion === 'kept') emotion++;
      if (v.voice === 'kept') voice++;
      if (v.emotion && v.voice) done++;
    }});
    document.getElementById('t-emotion').textContent = emotion;
    document.getElementById('t-voice').textContent = voice;
    document.getElementById('t-done').textContent = done;
  }}
  document.querySelectorAll('.tri button').forEach(b => b.addEventListener('click', () => {{
    const tri = b.closest('.tri'); const cue = b.closest('.cue').dataset.cue;
    verdicts[cue] = {{ ...(verdicts[cue] || {{}}), [tri.dataset.field]: b.dataset.v }};
    try {{ localStorage.setItem(KEY, JSON.stringify(verdicts)); }} catch (e) {{}}
    paint();
  }}));
  document.getElementById('copy').addEventListener('click', async () => {{
    const text = JSON.stringify(verdicts, null, 1);
    try {{ await navigator.clipboard.writeText(text); document.getElementById('copy').textContent = 'Copied'; setTimeout(() => document.getElementById('copy').textContent = 'Copy verdicts', 1500); }} catch (e) {{ prompt('Verdicts', text); }}
  }});
  paint();
}})();
</script>
"""
    a.out.write_text(page, encoding="utf-8")
    print(f"{len(rows)} rows -> {a.out} ({a.out.stat().st_size / 1e6:.1f} MB)")


if __name__ == "__main__":
    main()
