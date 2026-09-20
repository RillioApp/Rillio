# One G3 arm: serve <model.gguf> on llama-server (CUDA or Vulkan build), wait
# for health, score the slice, stop the server. Same path for every arm.
param(
    [Parameter(Mandatory)] [string] $Model,
    [Parameter(Mandatory)] [string] $Label,
    [string] $Backend = 'cuda',
    [string] $Slice = 'E:\datasets\opus\en-ja\f3-slice.jsonl',
    [string] $OutDir = 'E:\datasets\opus\en-ja\g3',
    [string] $Lang = 'English',
    [string] $SrcLang = 'Japanese',
    [int] $Limit = 0
)
$ErrorActionPreference = 'Stop'
$py = 'E:\venvs\dubbing\Scripts\python.exe'
$g3 = Split-Path -Parent $MyInvocation.MyCommand.Path
$score = Join-Path $g3 'score.py'
. (Join-Path $g3 'server.ps1')
New-Item -ItemType Directory -Force $OutDir | Out-Null
$log = Join-Path $OutDir "$Label.server.log"
$p = Start-LlamaServer -Model $Model -Log $log -Backend $Backend
try {
    $args = @($Slice, '--model', $Label, '--lang', $Lang, '--src-lang', $SrcLang, '--out', (Join-Path $OutDir "$Label.jsonl"))
    if ($Limit -gt 0) { $args += @('--limit', $Limit) }
    # Python's progress goes to stderr; under Stop, merging it into the
    # pipeline makes PowerShell treat "50/500" as a terminating error and the
    # arm dies at the first progress line (it did). Keep the streams apart.
    $summary = Join-Path $OutDir "$Label.summary.txt"
    $progress = Join-Path $OutDir "$Label.progress.txt"
    $proc = Start-Process -FilePath $py -ArgumentList (@($score) + $args) -NoNewWindow -Wait -PassThru -RedirectStandardOutput $summary -RedirectStandardError $progress
    if ($proc.ExitCode -ne 0) { throw "score.py failed for $Label (exit $($proc.ExitCode)); see $progress" }
    Get-Content $summary
} finally {
    Stop-LlamaServer $p
}
Show-Offload $log $Label
