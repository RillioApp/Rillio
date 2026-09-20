"""G2 scorer: SI-SDR of separated stems against ground truth.

Two uses:
  mix    build F5: speech (F2) mixed over a dialogue-free background at 0 dB
         loudness match; writes mix.wav + the two truth stems.
  score  SI-SDR (dB) of a separator's vocals stem vs the true speech and of
         its residual/instrumental stem vs the true background, plus the
         CONTROL: the unprocessed mix scored against each truth (what "doing
         nothing" earns). Every arm's claim is excess over that control.

SI-SDR is scale-invariant, so a stem returned at a different gain is not
penalised; it is time-aligned by construction (separators preserve length).
"""
import argparse
import json
from pathlib import Path

import numpy as np
import soundfile as sf

SR = 44_100


def load_mono(path: Path, sr: int = SR) -> np.ndarray:
    audio, rate = sf.read(path, dtype="float32", always_2d=True)
    audio = audio.mean(axis=1)
    if rate != sr:
        import librosa
        audio = librosa.resample(audio, orig_sr=rate, target_sr=sr)
    return audio


def si_sdr(estimate: np.ndarray, reference: np.ndarray) -> float:
    n = min(len(estimate), len(reference))
    e, r = estimate[:n].astype(np.float64), reference[:n].astype(np.float64)
    r = r - r.mean()
    e = e - e.mean()
    scale = np.dot(e, r) / (np.dot(r, r) + 1e-12)
    target = scale * r
    noise = e - target
    return float(10 * np.log10((np.dot(target, target) + 1e-12) / (np.dot(noise, noise) + 1e-12)))


def rms(x: np.ndarray) -> float:
    return float(np.sqrt(np.mean(x.astype(np.float64) ** 2) + 1e-12))


def cmd_mix(args: argparse.Namespace) -> None:
    speech = load_mono(args.speech)
    background = load_mono(args.background)
    n = min(len(speech), len(background))
    speech, background = speech[:n], background[:n]
    # Level the background to the speech's loudness (0 dB SNR), a hard but
    # realistic film mix; --snr shifts it.
    background = background * (rms(speech) / rms(background)) * 10 ** (-args.snr / 20)
    mix = speech + background
    peak = np.max(np.abs(mix))
    if peak > 0.99:
        speech, background, mix = (x * 0.99 / peak for x in (speech, background, mix))
    args.out.mkdir(parents=True, exist_ok=True)
    sf.write(args.out / "mix.wav", mix, SR)
    sf.write(args.out / "truth_speech.wav", speech, SR)
    sf.write(args.out / "truth_background.wav", background, SR)
    print(json.dumps({"seconds": round(n / SR, 1), "snr_db": args.snr, "out": str(args.out)}))


def cmd_score(args: argparse.Namespace) -> None:
    truth_speech = load_mono(args.truth / "truth_speech.wav")
    truth_background = load_mono(args.truth / "truth_background.wav")
    mix = load_mono(args.truth / "mix.wav")
    control = {"speech": round(si_sdr(mix, truth_speech), 2), "background": round(si_sdr(mix, truth_background), 2)}
    result = {"arm": args.label, "control_mix": control}
    if args.vocals:
        v = si_sdr(load_mono(args.vocals), truth_speech)
        result["vocals_si_sdr"] = round(v, 2)
        result["vocals_excess"] = round(v - control["speech"], 2)
    if args.residual:
        b = si_sdr(load_mono(args.residual), truth_background)
        result["residual_si_sdr"] = round(b, 2)
        result["residual_excess"] = round(b - control["background"], 2)
    if args.seconds:
        result["rtf"] = round(args.seconds / (len(mix) / SR), 3)
    print(json.dumps(result))


def main() -> None:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    m = sub.add_parser("mix")
    m.add_argument("--speech", type=Path, required=True)
    m.add_argument("--background", type=Path, required=True)
    m.add_argument("--snr", type=float, default=0.0)
    m.add_argument("--out", type=Path, required=True)
    m.set_defaults(fn=cmd_mix)
    s = sub.add_parser("score")
    s.add_argument("--truth", type=Path, required=True, help="dir holding mix.wav + truth_*.wav")
    s.add_argument("--label", required=True)
    s.add_argument("--vocals", type=Path)
    s.add_argument("--residual", type=Path)
    s.add_argument("--seconds", type=float, help="separator wall-clock, for RTF")
    s.set_defaults(fn=cmd_score)
    args = ap.parse_args()
    args.fn(args)


if __name__ == "__main__":
    main()
