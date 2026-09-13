# Rillio

Local-first streaming app: a hard fork of Stremio (web UI + core) with its own
Rust streaming server and a Tauri v2 desktop shell. Windows-only for now.

## Layout

- `apps/web` - the React web app (webpack, CommonJS-heavy legacy + newer TS/Tailwind v4)
- `apps/desktop/src-tauri` - the Tauri shell (frameless window, embedded mpv player, updater)
- `crates/core` - forked stremio-core (Rust; profile/library/addons logic)
- `crates/core-web` - wasm bridge exposing the core to the web app (artifacts are gitignored build outputs)
- `crates/streaming-server` - the Rust torrent/streaming engine (librqbit), runs in-process in the shell
- `landing` - rillio.app landing page (static, deployed to Vercel)

## Build and run

```
pnpm build:wasm                 # rebuild the core wasm (needed after crates/core changes)
pnpm --filter rillio build      # build the web app into apps/web/build
cargo run                       # in apps/desktop/src-tauri: build + launch the desktop shell
```

`generate_context!` **bakes `apps/web/build` into the binary** at compile time, so
**rebuild the web app before `cargo run`** to see web changes. Corollary that
bites: rebuilding the web app and then relaunching the EXE changes nothing, it
still serves the bundle baked into that binary. Always web build, then cargo
build, then launch (touch `src/lib.rs` first if cargo does not notice an
asset-only change). This is a SECOND staleness trap, separate from the WebView2
cache below. To tell them apart, `fetch(<stylesheet href>, {cache:'no-store'})`
from the page and grep the text for your change: absent means the BINARY is
stale, present but not applying means the WebView2 cache.

`libmpv-2.dll` is not present next to the debug binary, so playback is disabled
under `cargo run` (harmless for UI work; the release bundle ships the dll).

The shell links whisper.cpp (generated subtitles, `src/transcribe.rs`) through
`whisper-rs`, which compiles it with cmake and runs bindgen at build time, so a
Windows build needs cmake plus a libclang: set `LIBCLANG_PATH` to a directory
holding `libclang.dll` (CI uses the runner's `C:\Program Files\LLVM\bin`; a
dev box can use the PyPI `libclang` wheel's `site-packages\clang\native`). The
crate's bundled bindings are Linux-only, so `WHISPER_DONT_GENERATE_BINDINGS`
does not work on MSVC.

**Debug builds use their own profile** (`com.rillio.desktop.dev` - fresh/empty
data, own dirs under `%LOCALAPPDATA%`), so a dev shell can never contend with
the installed app's WebView2 profile (root cause of the 2026-08 dead-storage
incident: a boot while the previous browser still held the Local Storage LOCK
got a silent empty in-memory session). `RILLIO_SHARED_PROFILE=1` opts a dev
build into the REAL profile when a bug needs real data - run it strictly
instead of the installed app, never alongside it. The cache-clear ritual below
targets the installed profile; for a default dev build substitute
`com.rillio.desktop.dev` in the paths.

To inspect the running shell (its real DOM/console, the only way to settle bugs
that do not reproduce in a browser), launch with `RILLIO_DEVTOOLS_PORT=9222` and
drive CDP over `ws://127.0.0.1:9222`. Off unless the env var is set.

### Dev-loop gotcha: stale WebView2 cache (bites constantly)

Webpack prefixes asset URLs with the **git commit hash**, so rebuilding at the
same commit produces identical URLs and WebView2's service worker + HTTP cache
serve the OLD bundle - edits silently do not appear. Before each relaunch:

```powershell
Get-Process rillio-desktop -ErrorAction SilentlyContinue | Stop-Process -Force
# Wait for RILLIO'S webview children to exit on their own (they die with the
# host). NEVER `Stop-Process -Force` all msedgewebview2 like this snippet used
# to: that kills every WebView2 app on the machine AND can catch Chromium's
# storage service mid-write - "Local Storage" in this profile is the ONLY copy
# of the user's profile/library/settings (incident 2026-07-13, data wiped).
Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" |
  Where-Object { $_.CommandLine -match 'com\.rillio\.desktop' } |
  ForEach-Object { try { Wait-Process -Id $_.ProcessId -Timeout 15 -ErrorAction Stop } catch {} }
$d = "$env:LOCALAPPDATA\com.rillio.desktop\EBWebView\Default"
foreach ($t in @('Service Worker','Cache','Code Cache')) { Remove-Item (Join-Path $d $t) -Recurse -Force -ErrorAction SilentlyContinue }
```

Only ever delete those three cache dirs - everything else under `Default`
(especially `Local Storage`) is user data.

Production is unaffected (each release is a new commit, and the shell also
clears these caches when its version changes).

## Releases

Tag `v*` on main -> `.github/workflows/release.yml` builds, signs and publishes
a DRAFT GitHub release. Then, every release (until `bundle.targets` is switched
to nsis-only): download `latest.json`, point the `windows-x86_64` entry at the
NSIS `-setup.exe` (mirror url+signature from `windows-x86_64-nsis`), re-upload
with `--clobber`, publish with `gh release edit v<X> --draft=false --latest`,
and verify `/releases/latest/download/latest.json` + `RillioSetup.exe`.
Bump `version` in `apps/desktop/src-tauri/tauri.conf.json` before tagging.

The landing deploys from `landing/` with the Vercel CLI: `vercel build --prod`
then `vercel deploy --prebuilt --prod` (the remote build is flaky), then
`vercel alias set <deployment-url> rillio.app` (aliasing is NOT automatic).

## Conventions

- No em dashes anywhere (UI copy, commits, docs). Use commas, parentheses, or hyphens.
- Colors/spacing via semantic tokens only (`--color-*` in `apps/web/src/styles/tailwind.css`).
  Tailwind v4 rules there are load-bearing: utilities imported UNLAYERED and
  `@theme static` (see the comments in that file); keyframes live outside `@theme`.
- Flat, borderless UI: `divide-y` lists over nested cards, `rounded-full` pills,
  one accent color (#FFA033), hover via `brightness-110` or a bg tint.
- Root-cause fixes only; no silent fallbacks - fail loud.
- Windows dev: prefer PowerShell for shell commands (bash tooling misbehaves on
  the F: drive); write multi-line commit messages to a file and use `git commit -F`
  (embedded double quotes break `-m` arg passing from PowerShell).
