@echo off
rem The dev RELEASE shell on Michael's REAL profile with the staged dub pack.
rem Started through explorer.exe so it runs outside Claude's MSIX container
rem (a shell launched from inside it would use a redirected copy of the
rem profile). Close the installed Rillio first: never both at once.
rem A staged copy (target\michael), so a rebuild never has to wait for this
rem exe to close. Output goes to E:\datasets\s5\pipeline\michael-shell.log.
set RILLIO_DUB_PACK_DIR=E:\packs\dubbing\1
set RILLIO_SHARED_PROFILE=1
set RILLIO_DEVTOOLS_PORT=9222
cd /d F:\Projects\Code\Rillio\apps\desktop\src-tauri
start "" /b "F:\Projects\Code\Rillio\apps\desktop\src-tauri\target\michael\rillio-desktop.exe" > E:\datasets\s5\pipeline\michael-shell.log 2>&1
