# Incident 2026-09-15: the REAL profile's Local Storage (the only copy of
# profile/library/settings) was found empty (fresh leveldb created 2026-09-14
# 19:12:38 by the first boot after the wipe; the installed app booted fine at
# 02:54 that day). Restore from the shell's own pre-update snapshot taken
# 2026-09-13 23:04 (0.1.43 -> 0.1.44), which holds profile, library_recent,
# addons, streams, search_history, rillio.syncLog, rillio.displayName.
#
# Runs OUTSIDE Claude's container by using the UNC path (\\localhost\C$), so
# it acts on the real folder. Rillio must be CLOSED (Chromium holds the
# leveldb LOCK). Nothing is deleted: the empty database is moved aside.
param(
    [string] $Snapshot = '\\localhost\C$\Users\Michael\AppData\Local\com.rillio.desktop\storage-backup\0.1.43-1789329914\Local Storage\leveldb',
    [string] $Live = '\\localhost\C$\Users\Michael\AppData\Local\com.rillio.desktop\EBWebView\Default\Local Storage\leveldb',
    [string] $Aside = "E:\backups\rillio-localstorage-empty-moved-$(Get-Date -Format 'yyyyMMdd-HHmmss')"
)
$ErrorActionPreference = 'Stop'
if (Get-Process rillio-desktop -ErrorAction SilentlyContinue) { throw 'Rillio is running; close it first' }
if (-not (Test-Path (Join-Path $Snapshot 'CURRENT'))) { throw "snapshot has no CURRENT: $Snapshot" }
$snapBytes = (Get-ChildItem $Snapshot -File | Measure-Object Length -Sum).Sum
$liveBytes = if (Test-Path $Live) { (Get-ChildItem $Live -File | Measure-Object Length -Sum).Sum } else { 0 }
"snapshot: $snapBytes bytes; live: $liveBytes bytes"
if ($liveBytes -gt $snapBytes) { throw 'the live database is LARGER than the snapshot; refusing to replace it blindly' }
if (Test-Path $Live) {
    Move-Item $Live $Aside
    "moved the empty live database aside to $Aside"
}
New-Item -ItemType Directory -Force $Live | Out-Null
Copy-Item (Join-Path $Snapshot '*') $Live -Force
Remove-Item (Join-Path $Live 'LOCK') -Force -ErrorAction SilentlyContinue
$restored = (Get-ChildItem $Live -File | Measure-Object Length -Sum).Sum
if ($restored -ne ($snapBytes - ((Get-Item (Join-Path $Snapshot 'LOCK') -ErrorAction SilentlyContinue).Length))) { throw "restored $restored bytes, expected the snapshot's size" }
"restored $restored bytes into $Live. Launch the installed Rillio now and check boot-journal.log for 'guard verdict=ok ... userdata=true'."
