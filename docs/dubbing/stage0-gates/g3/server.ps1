# One owner of "serve a GGUF on llama-server for an arm": start with the
# arms' flags, wait for health, and stop. Dot-source this file.
#   . "$g3\server.ps1"; $srv = Start-LlamaServer -Model $gguf -Log $log; try { ... } finally { Stop-LlamaServer $srv }
$LlamaPort = 8080

function Start-LlamaServer([string] $Model, [string] $Log, [string] $Backend = 'cuda') {
    $exe = Get-ChildItem "E:\tools\llama.cpp\$Backend" -Recurse -Filter 'llama-server.exe' | Select-Object -First 1
    if (-not $exe) { throw "no llama-server.exe under E:\tools\llama.cpp\$Backend" }
    $p = Start-Process -FilePath $exe.FullName -ArgumentList '-m', $Model, '-ngl', '99', '-c', '4096', '--port', $LlamaPort, '--host', '127.0.0.1', '--parallel', '1' -WorkingDirectory $exe.DirectoryName -PassThru -WindowStyle Hidden -RedirectStandardOutput $Log -RedirectStandardError "$Log.err"
    foreach ($i in 1..120) {
        Start-Sleep 1
        try { if ((Invoke-RestMethod "http://127.0.0.1:$LlamaPort/health" -TimeoutSec 2).status -eq 'ok') { return $p } } catch {}
    }
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    throw "llama-server for $Model never became healthy (see $Log.err)"
}

function Stop-LlamaServer($Process) {
    Stop-Process -Id $Process.Id -Force -ErrorAction SilentlyContinue
}

# Offload evidence (R1): the server must report the layers on the GPU.
function Show-Offload([string] $Log, [string] $Label) {
    Get-Content "$Log.err" -ErrorAction SilentlyContinue | Select-String -Pattern 'offloaded|CUDA0|Vulkan0' | Select-Object -First 3 | ForEach-Object { "  [$Label] $($_.Line.Trim())" }
}
