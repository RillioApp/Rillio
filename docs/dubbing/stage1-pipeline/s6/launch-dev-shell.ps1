# S6: the dev shell with CDP on 9222 and the staged pack. Never pipe this
# script (the shell inherits the pipe and the caller hangs for its lifetime).
#   .\launch-dev-shell.ps1            debug build
#   .\launch-dev-shell.ps1 -Release   release build (the pipeline's real speed)
param([switch] $Release)
$env:RILLIO_DEVTOOLS_PORT = '9222'
$env:RILLIO_DUB_PACK_DIR = 'E:\packs\dubbing\1'
$profile_ = if ($Release) { 'release' } else { 'debug' }
$root = 'F:\Projects\Code\Rillio\apps\desktop\src-tauri'
$exe = "$root\target\$profile_\rillio-desktop.exe"
if ($Release -and -not (Test-Path "$root\target\release\libmpv-2.dll")) { Copy-Item "$root\target\debug\libmpv-2.dll" "$root\target\release\" }
$p = Start-Process -FilePath $exe -WorkingDirectory $root -PassThru -RedirectStandardOutput 'E:\datasets\s5\pipeline\shell.log' -RedirectStandardError 'E:\datasets\s5\pipeline\shell.err'
"pid=$($p.Id) ($profile_)"
