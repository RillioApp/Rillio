@echo off
rem Run the shell's unit tests OUTSIDE Claude's MSIX container (via explorer.exe):
rem a loader failure that only happens inside the container shows up here as a pass.
cd /d F:\Projects\Code\Rillio\apps\desktop\src-tauri
"F:\Projects\Code\Rillio\apps\desktop\src-tauri\target\release\deps\rillio_desktop_lib-0d7ffe1e2637fb6c.exe" dub:: > E:\datasets\s5\pipeline\tests-outside.txt 2>&1
echo exit %ERRORLEVEL% >> E:\datasets\s5\pipeline\tests-outside.txt
