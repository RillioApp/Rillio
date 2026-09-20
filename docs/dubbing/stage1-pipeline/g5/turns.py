"""Speaker turns from the dialogue stem (decision D3a): activity by WebRTC VAD,
identity by speaker embeddings clustered per scene.

    python turns.py <stem_16k_mono.wav> <turns.jsonl> [--vad 3]

Output rows: {start_ms, end_ms, speaker} with speaker ids stable within a
scene (a gap longer than SCENE_GAP_S starts a new scene and a new cluster).
Embedding: Resemblyzer GE2E on each run; clustering: agglomerative with a
cosine threshold, the same encoder and threshold family as the overlap
heuristic, so the two instruments agree on what "one voice" means.
"""
import argparse
import json
from pathlib import Path

import numpy as np
import soundfile as sf
import webrtcvad
from resemblyzer import VoiceEncoder

RATE = 16_000
FRAME_MS = 20
# Unvoiced gaps up to this long stay inside a run (pauses between words).
GAP_FILL_MS = 300
# Runs shorter than this are not turns (VAD chatter, a breath).
MIN_TURN_MS = 400
# A run longer than this is split at its longest inner pause (two turns in one run).
MAX_TURN_MS = 12_000
# Silence longer than this separates scenes: speaker ids do not carry across.
SCENE_GAP_S = 20.0
# Two runs closer than this in embedding space are the same speaker.
SAME_SPEAKER_COSINE = 0.75


def vad_runs(samples: np.ndarray, aggressiveness: int) -> list[tuple[int, int]]:
    vad = webrtcvad.Vad(aggressiveness)
    frame = RATE * FRAME_MS // 1000
    pcm = samples.tobytes()
    n = len(samples) // frame
    voiced = [vad.is_speech(pcm[i * frame * 2:(i + 1) * frame * 2], RATE) for i in range(n)]
    fill = GAP_FILL_MS // FRAME_MS
    runs, start, gap = [], None, 0
    for i, v in enumerate(voiced + [False]):
        if v:
            if start is None:
                start = i
            gap = 0
        elif start is not None:
            gap += 1
            if gap > fill:
                runs.append((start * FRAME_MS, (i - gap + 1) * FRAME_MS))
                start, gap = None, 0
    return [(s, e) for s, e in runs if e - s >= MIN_TURN_MS]


def split_long(runs: list[tuple[int, int]], samples: np.ndarray) -> list[tuple[int, int]]:
    out = []
    for s, e in runs:
        while e - s > MAX_TURN_MS:
            # Cut at the quietest 100 ms inside the middle half of the run.
            seg = samples[s * RATE // 1000:e * RATE // 1000].astype(np.float32)
            hop = RATE // 10
            lo, hi = len(seg) // 4, 3 * len(seg) // 4
            energies = [(np.abs(seg[i:i + hop]).mean(), i) for i in range(lo, hi - hop, hop)]
            cut = s + min(energies)[1] * 1000 // RATE
            out.append((s, cut))
            s = cut
        out.append((s, e))
    return out


# Within-run speaker change: sub-window embeddings on this hop; a run is cut
# where the two sides differ by more than the same-speaker line.
CHANGE_WINDOW_MS = 1000
CHANGE_HOP_MS = 250
MIN_PIECE_MS = 600


def split_by_speaker_change(runs: list[tuple[int, int]], audio: np.ndarray, encoder: VoiceEncoder) -> list[tuple[int, int]]:
    """Fast exchanges leave no VAD gap; find the point where the voice changes."""
    out = []
    todo = list(runs)
    while todo:
        s, e = todo.pop(0)
        if e - s < 2 * MIN_PIECE_MS + CHANGE_WINDOW_MS:
            out.append((s, e))
            continue
        starts = list(range(s, e - CHANGE_WINDOW_MS + 1, CHANGE_HOP_MS))
        embeds = np.stack([encoder.embed_utterance(audio[t * RATE // 1000:(t + CHANGE_WINDOW_MS) * RATE // 1000]) for t in starts])
        embeds /= np.linalg.norm(embeds, axis=1, keepdims=True) + 1e-9
        best_cut, best_sim = None, SAME_SPEAKER_COSINE
        for k in range(1, len(starts)):
            cut = starts[k]
            if cut - s < MIN_PIECE_MS or e - cut < MIN_PIECE_MS:
                continue
            left, right = embeds[:k].mean(axis=0), embeds[k:].mean(axis=0)
            sim = float(left @ right / (np.linalg.norm(left) * np.linalg.norm(right) + 1e-9))
            if sim < best_sim:
                best_cut, best_sim = cut, sim
        if best_cut is None:
            out.append((s, e))
        else:
            todo[:0] = [(s, best_cut), (best_cut, e)]
    return sorted(out)


def cluster(embeds: np.ndarray) -> list[int]:
    """Greedy agglomerative clustering by cosine to cluster centroids."""
    labels, centroids = [], []
    for e in embeds:
        e = e / (np.linalg.norm(e) + 1e-9)
        best, best_sim = None, SAME_SPEAKER_COSINE
        for k, c in enumerate(centroids):
            sim = float(e @ c / (np.linalg.norm(c) + 1e-9))
            if sim > best_sim:
                best, best_sim = k, sim
        if best is None:
            centroids.append(e.copy())
            labels.append(len(centroids) - 1)
        else:
            centroids[best] = centroids[best] + e
            labels.append(best)
    return labels


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("stem", type=Path); ap.add_argument("out", type=Path)
    ap.add_argument("--vad", type=int, default=3)
    a = ap.parse_args()
    samples, rate = sf.read(str(a.stem), dtype="int16")
    if rate != RATE or samples.ndim != 1:
        raise SystemExit(f"{a.stem}: need {RATE} Hz mono, got {rate} Hz {samples.ndim}-d")
    encoder = VoiceEncoder()
    audio = samples.astype(np.float32) / 32768.0
    runs = split_by_speaker_change(split_long(vad_runs(samples, a.vad), samples), audio, encoder)
    embeds = np.stack([encoder.embed_utterance(audio[s * RATE // 1000:e * RATE // 1000]) for s, e in runs])

    # Cluster per scene so ids never bleed across a long silence.
    turns, scene_start, scene = [], 0, 0
    for i in range(len(runs) + 1):
        boundary = i == len(runs) or (i > 0 and runs[i][0] - runs[i - 1][1] > SCENE_GAP_S * 1000)
        if boundary:
            labels = cluster(embeds[scene_start:i])
            for (s, e), lab in zip(runs[scene_start:i], labels):
                turns.append({"start_ms": s, "end_ms": e, "speaker": f"s{scene}-{lab}"})
            scene_start, scene = i, scene + 1
    a.out.write_text("".join(json.dumps(t) + "\n" for t in turns), encoding="utf-8")
    speakers = {t["speaker"] for t in turns}
    print(f"{len(turns)} turns, {len(speakers)} speaker ids over {scene} scenes -> {a.out}")


if __name__ == "__main__":
    main()
