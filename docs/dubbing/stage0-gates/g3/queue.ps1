# The Stage-0 queue for the transfer-bound tail: waits for each file to
# finish (size >= expected AND stable for 60 s), then runs the arms that need
# it, one server on 8080 at a time and never a TTS timing while a server is up.
#   1. Qwen3-1.7B arms on JA/ZH/KO/RU->EN and EN->HE (+ DE->EN full set if unpacked)
#   2. Qwen3.5-4B (ceiling) arms on the same pairs
#   3. G1(a): VoxCPM2 PyTorch timing on F1 (idle GPU)
$ErrorActionPreference = 'Continue'
$g3 = 'F:\Projects\Code\Rillio\docs\dubbing\stage0-gates\g3'
$g1 = 'F:\Projects\Code\Rillio\docs\dubbing\stage0-gates\g1'
$py = 'E:\venvs\dubbing\Scripts\python.exe'
$pairs = @(@('en-ja','Japanese','English'), @('en-zh_CN','Chinese','English'), @('en-ko','Korean','English'), @('en-ru','Russian','English'), @('en-he','English','Hebrew'))

function Wait-File([string] $Path, [long] $MinBytes) {
    $last = -1
    while ($true) {
        if (Test-Path $Path) {
            $size = (Get-Item $Path).Length
            if ($size -ge $MinBytes -and $size -eq $last) { return }
            $last = $size
        }
        Start-Sleep 60
    }
}

# Arms across all pairs run through run_pairs.ps1 (one owner of the pair table
# and the skip-if-banked rule).
function Run-Pairs([string] $Model, [string] $Label) {
    powershell -ExecutionPolicy Bypass -File "$g3\run_pairs.ps1" -Model $Model -Label $Label
}

function Ensure-Pair([string] $Dir, [string] $SrcCode, [string] $Src, [string] $Tgt) {
    # Slice + floor control once per pair.
    if (-not (Get-ChildItem "E:\datasets\opus\$Dir" -Filter 'OpenSubtitles.*' -ErrorAction SilentlyContinue)) { "corpus $Dir not unpacked, skipping"; return $false }
    $slice = "E:\datasets\opus\$Dir\f3-slice.jsonl"
    $out = "E:\datasets\opus\$Dir\g3"
    New-Item -ItemType Directory -Force $out | Out-Null
    if (-not (Test-Path $slice)) { & $py "$g3\make_slice.py" "E:\datasets\opus\$Dir" $SrcCode en $slice }
    if (-not (Test-Path "$out\copy-source.jsonl")) { & $py "$g3\score.py" $slice --model copy-source --copy --lang $Tgt --src-lang $Src --out "$out\copy-source.jsonl" 2>$null }
    return $true
}

# Ceiling first: G3's verdict needs it; the 1.7B is a fourth candidate.
"[queue] waiting for Qwen3.5-4B (ceiling)"
Wait-File 'E:\models\gguf\Qwen3.5-4B-GGUF\Qwen3.5-4B-Q8_0.gguf' 4400000000
foreach ($p in $pairs) { Ensure-Pair $p[0] ($p[0] -replace '^en-', '') $p[1] $p[2] | Out-Null }
Ensure-Pair 'de-en' 'de' 'German' 'English' | Out-Null
Ensure-Pair 'en-fr' 'fr' 'French' 'English' | Out-Null
Run-Pairs 'E:\models\gguf\Qwen3.5-0.8B-GGUF\Qwen3.5-0.8B-Q8_0.gguf' 'qwen3.5-0.8b-q8-cuda'
Run-Pairs 'E:\models\gguf\Qwen3.5-2B-GGUF\Qwen3.5-2B-Q8_0.gguf' 'qwen3.5-2b-q8-cuda'
Run-Pairs 'E:\models\gguf\Qwen3.5-4B-GGUF\Qwen3.5-4B-Q8_0.gguf' 'qwen3.5-4b-q8-cuda-ceiling'

"[queue] waiting for VoxCPM2 model.safetensors"
Wait-File 'E:\models\VoxCPM2\model.safetensors' 4550000000
"=== G1(a): VoxCPM2 PyTorch timing on F1 (idle GPU) ==="
& $py "$g1\time_voxcpm.py" 'E:\datasets\opus\en-he\f3-slice.jsonl' 'E:\datasets\g1\pytorch' 2>&1 | Select-Object -Last 25

# The 1.7B is a fourth candidate; its transfer is only started once the two
# files above are in, so it never competes with them for the line.
"[queue] fetching Qwen3-1.7B"
$d = 'E:\models\gguf\Qwen3-1.7B-GGUF'
Set-Location $d
curl.exe -L -sS --retry 30 --retry-delay 5 --retry-all-errors -C - -o Qwen3-1.7B-Q8_0.gguf 'https://huggingface.co/Qwen/Qwen3-1.7B-GGUF/resolve/main/Qwen3-1.7B-Q8_0.gguf'
Wait-File "$d\Qwen3-1.7B-Q8_0.gguf" 1800000000
Run-Pairs "$d\Qwen3-1.7B-Q8_0.gguf" 'qwen3-1.7b-q8-cuda'
"[queue] done"
