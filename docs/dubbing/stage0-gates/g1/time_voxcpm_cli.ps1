# G1 arm (b): the ggml VoxCPM2 CLI (llama.cpp-omni, CUDA build) on the SAME F1
# lines as arm (a), 3 runs each after a warm-up. The CLI reports its own
# elapsed / audio seconds (RTF) per synthesis, so model load is not counted
# (it reloads per invocation). Banked number: median RTF per language.
# Requires the GGUFs from convert_voxcpm2_to_gguf.py (BaseLM + Acoustic).
param(
    [string] $Slice = 'E:\datasets\opus\en-he\f3-slice.jsonl',
    [string] $Out = 'E:\datasets\g1\ggml-cuda',
    [string] $BaseLM = 'E:\models\VoxCPM2-GGUF\VoxCPM2-BaseLM-F16.gguf',
    [string] $Acoustic = 'E:\models\VoxCPM2-GGUF\VoxCPM2-Acoustic-F16.gguf',
    [string] $Ref = '',
    [int] $Lines = 10,
    [int] $Runs = 3,
    # The ggml build under test: CUDA (arm b) or Vulkan (arm c), same harness.
    [string] $Cli = 'E:\src\llama.cpp-omni\build-cuda\bin\voxcpm2-cli.exe'
)
$ErrorActionPreference = 'Stop'
$cli = $Cli
New-Item -ItemType Directory -Force $Out | Out-Null
$rows = Get-Content $Slice -Encoding UTF8 -TotalCount $Lines | ForEach-Object { $_ | ConvertFrom-Json }

# Start-Process joins an ArgumentList array with bare spaces, so a multi-word
# line splits into positionals: the command line is built as ONE quoted string.
function Quote([string] $s) { '"' + ($s -replace '"', '\"') + '"' }

function Synth([string] $Text, [string] $Wav) {
    $parts = @('-t', (Quote $Text), '-o', (Quote $Wav))
    if ($Ref) { $parts += @('-r', (Quote $Ref)) }
    $parts += @((Quote $BaseLM), (Quote $Acoustic))
    $log = "$Wav.log"
    $p = Start-Process -FilePath $cli -ArgumentList ($parts -join ' ') -NoNewWindow -Wait -PassThru -RedirectStandardOutput $log -RedirectStandardError "$log.err"
    if ($p.ExitCode -ne 0) { throw "voxcpm2-cli failed on '$Text' (exit $($p.ExitCode)); see $log.err" }
    # The CLI prints e.g. "... 1.24s elapsed for 4.80s audio (RTF 0.26)"; take
    # its RTF, fall back to elapsed/audio if only those are present.
    $text = (Get-Content $log, "$log.err" -ErrorAction SilentlyContinue) -join "`n"
    if ($text -match 'RTF\s*[:=]?\s*([0-9.]+)') { return [double]$Matches[1] }
    if ($text -match '([0-9.]+)\s*s elapsed for ([0-9.]+)\s*s audio') { return [double]$Matches[1] / [double]$Matches[2] }
    throw "no RTF/elapsed line in $log (CLI output format changed?)"
}

Synth $rows[0].src (Join-Path $Out 'warmup.wav') | Out-Null
# The arm label follows the build under test (…\build-cuda\… or …\build-vulkan\…).
$backend = if ($cli -match 'build-(\w+)') { $Matches[1] } else { 'unknown' }
$summary = @{ arm = "voxcpm2-ggml-$backend" + $(if ($Ref) { '-clone' } else { '' }) }
foreach ($lang in @(@('en', 'src'), @('he', 'ref'))) {
    $rtfs = @()
    for ($i = 0; $i -lt $rows.Count; $i++) {
        $per = @()
        for ($r = 0; $r -lt $Runs; $r++) {
            $wav = Join-Path $Out ("{0}_{1:d2}_run{2}.wav" -f $lang[0], $i, $r)
            $per += Synth $rows[$i].($lang[1]) $wav
        }
        $sorted = $per | Sort-Object
        $rtfs += $sorted[[int](($sorted.Count - 1) / 2)]
        "{0} {1:d2} rtf={2:n3}" -f $lang[0], $i, $rtfs[-1]
    }
    $s = $rtfs | Sort-Object
    $summary[$lang[0]] = @{ rtf_median = [math]::Round($s[[int](($s.Count - 1) / 2)], 3); rtf_max = [math]::Round($s[-1], 3) }
}
$summary | ConvertTo-Json -Compress
$summary | ConvertTo-Json | Set-Content (Join-Path $Out 'summary.json') -Encoding UTF8
