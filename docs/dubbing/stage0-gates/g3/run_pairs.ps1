# One G3 arm across every pair: `run_arm.ps1` per pair, skipping pairs whose
# summary is already banked, so a re-run after an interruption costs nothing.
#   run_pairs.ps1 -Model <gguf> -Label <ledger label>
# Never start this while a TTS timing (G1) is running: the llama-server
# shares the GPU and the timing is only valid idle.
param(
    [Parameter(Mandatory)] [string] $Model,
    [Parameter(Mandatory)] [string] $Label
)
$ErrorActionPreference = 'Continue'
$g3 = Split-Path -Parent $MyInvocation.MyCommand.Path
$pairs = @(
    @('en-ja', 'Japanese', 'English'), @('en-zh_CN', 'Chinese', 'English'), @('en-ko', 'Korean', 'English'),
    @('en-ru', 'Russian', 'English'), @('en-he', 'English', 'Hebrew'),
    @('de-en', 'German', 'English'), @('en-fr', 'French', 'English')
)
foreach ($p in $pairs) {
    $dir, $src, $tgt = $p
    $slice = "E:\datasets\opus\$dir\f3-slice.jsonl"
    $out = "E:\datasets\opus\$dir\g3"
    $summary = "$out\$Label.summary.txt"
    if ((Test-Path $summary) -and (Get-Item $summary).Length -gt 0) { "skip $dir $Label (done)"; continue }
    if (-not (Test-Path $slice)) { "skip $dir (no slice)"; continue }
    "=== $src->$tgt arm: $Label ==="
    powershell -ExecutionPolicy Bypass -File "$g3\run_arm.ps1" -Model $Model -Label $Label -Slice $slice -OutDir $out -Lang $tgt -SrcLang $src
}
"[run_pairs] done: $Label"
