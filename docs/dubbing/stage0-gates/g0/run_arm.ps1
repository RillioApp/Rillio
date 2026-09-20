# One G0 translation arm: serve <model.gguf>, translate every aligned cue with
# the G3 prompt (g0/score.py run), stop the server. Never alongside a G1 timing.
#   run_arm.ps1 -Model <gguf> -Label <x> -Aligned <aligned.jsonl> [-SrcLang Japanese]
param(
    [Parameter(Mandatory)] [string] $Model,
    [Parameter(Mandatory)] [string] $Label,
    [Parameter(Mandatory)] [string] $Aligned,
    [string] $SrcLang = 'Japanese',
    [string] $OutDir = 'E:\datasets\g0\arms'
)
$ErrorActionPreference = 'Stop'
$py = 'E:\venvs\dubbing\Scripts\python.exe'
$gates = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
. (Join-Path $gates 'g3\server.ps1')
New-Item -ItemType Directory -Force $OutDir | Out-Null
$log = Join-Path $OutDir "$Label.server.log"
$p = Start-LlamaServer -Model $Model -Log $log
try {
    $summary = Join-Path $OutDir "$Label.stdout.txt"
    $progress = Join-Path $OutDir "$Label.progress.txt"
    $args = @((Join-Path $gates 'g0\score.py'), 'run', $Aligned, (Join-Path $OutDir "$Label.jsonl"), '--model', $Label, '--src-lang', $SrcLang)
    $proc = Start-Process -FilePath $py -ArgumentList $args -NoNewWindow -Wait -PassThru -RedirectStandardOutput $summary -RedirectStandardError $progress
    if ($proc.ExitCode -ne 0) { throw "score.py run failed for $Label (exit $($proc.ExitCode)); see $progress" }
    Get-Content $summary
} finally {
    Stop-LlamaServer $p
}
Show-Offload $log $Label
