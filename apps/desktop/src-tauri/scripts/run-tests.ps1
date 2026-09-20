# cargo test for the shell, with the one thing the test binaries lack: the
# app's manifest. The app exe carries Tauri's manifest, which activates
# comctl32 v6; a dependency imports TaskDialogIndirect (v6 only), so a test
# exe without that manifest dies at load with 0xC0000139 (entry point not
# found) before a single test runs (2026-09-18). This script builds the test
# binaries, embeds a Common-Controls 6.0 manifest into each with mt.exe
# (Windows SDK, on the VS BuildTools path), then runs the filter.
#
#   scripts\run-tests.ps1 [-Filter dub] [-Release]
param([string]$Filter = "", [switch]$Release)
$ErrorActionPreference = "Stop"
$here = Split-Path $PSScriptRoot -Parent
$profile = if ($Release) { "release" } else { "debug" }
$profileFlag = if ($Release) { "--release" } else { "" }
$vcvars = "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
$manifest = Join-Path $env:TEMP "rillio-test.manifest"
@'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
</assembly>
'@ | ForEach-Object { [System.IO.File]::WriteAllText($manifest, $_, (New-Object System.Text.UTF8Encoding($false))) }

Push-Location $here
try {
    cmd /c "cargo test $profileFlag --no-run $Filter"
    if ($LASTEXITCODE -ne 0) { throw "cargo test --no-run failed" }
    foreach ($exe in Get-ChildItem (Join-Path $here "target\$profile\deps") -Filter "rillio_desktop*.exe") {
        cmd /c "call `"$vcvars`" >nul && mt.exe -nologo -manifest `"$manifest`" -outputresource:`"$($exe.FullName);#1`""
        if ($LASTEXITCODE -ne 0) { throw "mt.exe failed on $($exe.Name)" }
    }
    cmd /c "cargo test $profileFlag $Filter"
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
