# Which processes sit in a job object: the shell and the pack sidecars.
# Uses IsProcessInJob(process, NULL): "is in ANY job".
Add-Type -Namespace Win32 -Name Job -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
public static extern bool IsProcessInJob(System.IntPtr process, System.IntPtr job, out bool result);
'@
foreach ($p in Get-CimInstance Win32_Process | Where-Object { $_.Name -match 'rillio-desktop|llama-server|llama-tts-server|whisper-server' }) {
    $h = (Get-Process -Id $p.ProcessId).Handle
    $in = $false
    $ok = [Win32.Job]::IsProcessInJob($h, [IntPtr]::Zero, [ref]$in)
    "{0,6} {1,-22} inJob={2} ok={3} parent={4}" -f $p.ProcessId, $p.Name, $in, $ok, $p.ParentProcessId
}
