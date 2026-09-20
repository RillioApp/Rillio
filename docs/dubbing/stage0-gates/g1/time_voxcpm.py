"""G1 arm (a): VoxCPM2 through the `voxcpm` PyTorch package on this GPU.

F1 = the first 10 lines of the F3 slice (English source, Hebrew reference),
so every TTS arm speaks the identical sentences and RTFs pair (R11). Each
sentence is generated 3 times after one untimed warm-up; the banked number is
the median RTF = generation seconds / produced audio seconds, per language,
plus peak VRAM. The wavs are kept for the by-ear judgement and for G4.

Usage: python time_voxcpm.py <slice.jsonl> <out_dir> [--ref clone.wav --ref-text "..."]
"""
import argparse
import json
import statistics
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import gates_env  # noqa: E402

gates_env.point_cc_at_triton_tcc()

import soundfile as sf  # noqa: E402
import torch  # noqa: E402
from voxcpm import VoxCPM  # noqa: E402

MODEL_ID = "openbmb/VoxCPM2"
LOCAL_DIR = Path("E:/models/VoxCPM2")
LINES = 10
RUNS = 3


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("slice", type=Path)
    ap.add_argument("out", type=Path)
    ap.add_argument("--ref", type=Path, help="reference wav for cloning")
    ap.add_argument("--ref-text", help="exact transcript of --ref (ultimate cloning)")
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)

    rows = [json.loads(l) for l in args.slice.read_text(encoding="utf-8").splitlines()[:LINES]]
    model = VoxCPM.from_pretrained(str(LOCAL_DIR) if LOCAL_DIR.exists() else MODEL_ID)
    sr = getattr(model, "sample_rate", None) or 48_000

    def speak(text: str):
        # --ref alone is text-free cloning (the CLI's -r); with --ref-text it
        # is prompt continuation, which voxcpm requires to be paired.
        kwargs = {}
        if args.ref and args.ref_text:
            kwargs["prompt_wav_path"] = str(args.ref)
            kwargs["prompt_text"] = args.ref_text
        elif args.ref:
            kwargs["reference_wav_path"] = str(args.ref)
        return model.generate(text=text, **kwargs)

    speak(rows[0]["src"])  # warm-up, untimed
    torch.cuda.reset_peak_memory_stats()
    results = {}
    for lang, key in (("en", "src"), ("he", "ref")):
        rtfs = []
        for i, row in enumerate(rows):
            per_run = []
            for run in range(RUNS):
                torch.cuda.synchronize()
                t0 = time.perf_counter()
                wav = speak(row[key])
                torch.cuda.synchronize()
                gen = time.perf_counter() - t0
                audio_s = len(wav) / sr
                per_run.append(gen / audio_s)
                if run == 0:
                    sf.write(args.out / f"{lang}_{i:02d}.wav", wav, sr)
            rtfs.append(statistics.median(per_run))
            print(f"{lang} {i:02d} rtf={rtfs[-1]:.3f} audio={audio_s:.2f}s", flush=True)
        results[lang] = {"rtf_median": round(statistics.median(rtfs), 3), "rtf_max": round(max(rtfs), 3)}
    results["peak_vram_gb"] = round(torch.cuda.max_memory_allocated() / 2**30, 2)
    results["sample_rate"] = sr
    results["arm"] = "voxcpm-pytorch" + ("-clone" if args.ref else "")
    print(json.dumps(results))
    (args.out / "summary.json").write_text(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
