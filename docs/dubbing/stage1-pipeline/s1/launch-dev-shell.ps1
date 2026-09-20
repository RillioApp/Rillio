$env:RILLIO_DEVTOOLS_PORT = '9222'
$p = Start-Process -FilePath 'F:\Projects\Code\Rillio\apps\desktop\src-tauri\target\debug\rillio-desktop.exe' -WorkingDirectory 'F:\Projects\Code\Rillio\apps\desktop\src-tauri' -PassThru -RedirectStandardOutput 'E:\datasets\g0\shell.log' -RedirectStandardError 'E:\datasets\g0\shell.err'
"pid=$($p.Id)"
