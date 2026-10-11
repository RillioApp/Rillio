# Fetch libmpv-2.dll and inject it into the Tauri bundle resources (Windows CI).
#
# Playback needs libmpv-2.dll next to the exe (loaded at runtime via libloading;
# see mpv.rs default_dll_path). It is gitignored, so CI stages a pinned shared
# build here rather than committing the DLL or the config change - that way a
# normal local `cargo build` (no DLL on disk) still works.
#
# The DLL comes from our own mirror, RillioApp/rillio-deps, not from
# shinchiro/mpv-winbuild-cmake directly: upstream keeps only its ~30 newest
# nightlies, so a pin there expires after about a month (the 20260610 build
# vanished and broke the v0.1.47 release). Each mirrored DLL is extracted
# unmodified from an upstream mpv-dev archive; the rillio-deps README records
# which one.
#
# NOTE: these builds are GPL; shipping the DLL carries the usual GPL source
# obligations.
#
# Bumping mpv: extract libmpv-2.dll from the new upstream mpv-dev-x86_64 archive,
# publish it as a new rillio-deps release (tag libmpv-<upstream tag>), note its
# source archive in the README, then update the two pinned values together.
#
# Used by .github/workflows/ci.yml and release.yml.

$ErrorActionPreference = 'Stop'

$releaseTag = 'libmpv-20260610'
$sha256 = '5c876d79e070529128331591b48f87846fb30557f19c11280df9c6ee9b6dbafa'

$repoRoot = Split-Path $PSScriptRoot -Parent
$tauriDir = Join-Path $repoRoot 'apps/desktop/src-tauri'
$dll = Join-Path $tauriDir 'libmpv-2.dll'

Write-Host "downloading libmpv-2.dll ($releaseTag)"
Invoke-WebRequest "https://github.com/RillioApp/rillio-deps/releases/download/$releaseTag/libmpv-2.dll" -OutFile $dll
$actual = (Get-FileHash $dll -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $sha256) {
    throw "SHA256 mismatch for libmpv-2.dll ($releaseTag): expected $sha256, got $actual"
}

# Inject as a bundle resource. Written with UTF8Encoding($false): PowerShell 5.1's
# Set-Content -Encoding utf8 writes a BOM, and a BOM in tauri.conf.json / Cargo.toml
# has already killed one release (tauri-action chokes on U+FEFF).
$cfgPath = Join-Path $tauriDir 'tauri.conf.json'
$cfg = Get-Content $cfgPath -Raw | ConvertFrom-Json
$cfg.bundle | Add-Member -NotePropertyName resources -NotePropertyValue @('libmpv-2.dll') -Force
[IO.File]::WriteAllText($cfgPath, ($cfg | ConvertTo-Json -Depth 30), (New-Object System.Text.UTF8Encoding($false)))
Write-Host 'injected libmpv-2.dll into bundle.resources'
