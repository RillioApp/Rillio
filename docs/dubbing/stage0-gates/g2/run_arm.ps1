# One G2 arm on the F5 mix: run a separator, sum its non-vocal stems into a
# residual, score SI-SDR against the truth, and report RTF from the
# separator's OWN "Separation duration" (wall-clock includes process start,
# first-run model fetches and stem writes, none of which is the model).
# Run with an IDLE GPU for the timing to mean anything.
param(
    [Parameter(Mandatory)] [string] $Model,     # e.g. htdemucs.yaml, model_bs_roformer_ep_368_sdr_12.9628.ckpt
    [Parameter(Mandatory)] [string] $Label,
    [string] $Truth = 'E:\datasets\g2\f5',
    [string] $ModelDir = 'E:\models\separator',
    # Extra audio-separator options as ONE space-separated string, e.g.
    # '--mdxc_overlap=2 --mdxc_batch_size=4' (construction knobs, R6). A
    # string[] does not survive `powershell -File` (it arrives comma-joined).
    [string] $Extra = ''
)
$ErrorActionPreference = 'Stop'
$py = 'E:\venvs\dubbing\Scripts\python.exe'
$sep = 'E:\venvs\dubbing\Scripts\audio-separator.exe'
$score = 'F:\Projects\Code\Rillio\docs\dubbing\stage0-gates\g2\score.py'
$out = Join-Path $Truth $Label
$log = "$out.log"
$sepArgs = @((Join-Path $Truth 'mix.wav'), '-m', $Model, '--model_file_dir', $ModelDir, '--output_dir', $out, '--output_format', 'WAV') + @($Extra -split '\s+' | Where-Object { $_ })
$proc = Start-Process -FilePath $sep -ArgumentList $sepArgs -NoNewWindow -Wait -PassThru -RedirectStandardOutput $log -RedirectStandardError "$log.err"
if ($proc.ExitCode -ne 0) { throw "separator failed for $Label (exit $($proc.ExitCode)); see $log.err" }
$dur = Get-Content $log, "$log.err" | Select-String -Pattern 'Separation duration: (\d+):(\d+):(\d+)' | Select-Object -First 1
if (-not $dur) { throw "no 'Separation duration' line for $Label" }
$sec = [int]$dur.Matches[0].Groups[1].Value * 3600 + [int]$dur.Matches[0].Groups[2].Value * 60 + [int]$dur.Matches[0].Groups[3].Value
$vocals = Get-ChildItem $out -Filter '*Vocals*' | Select-Object -First 1
$others = Get-ChildItem $out -File | Where-Object { $_.Name -notmatch 'Vocals|residual_sum' }
& $py -c "import soundfile as sf, sys; xs=[sf.read(p, dtype='float32', always_2d=True)[0] for p in sys.argv[1:]]; n=min(len(x) for x in xs); sf.write(sys.argv[0] if False else r'$out\residual_sum.wav', sum(x[:n] for x in xs), 44100)" @($others.FullName)
& $py $score score --truth $Truth --label $Label --vocals $vocals.FullName --residual "$out\residual_sum.wav" --seconds $sec
