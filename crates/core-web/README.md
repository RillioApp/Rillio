# rillio-core-web

Wasm bridge exposing `rillio-core` (`crates/core`) to the Rillio web app
(`apps/web`). Part of the Rillio monorepo (https://github.com/RillioApp/rillio),
a hard fork of stremio-core-web.

## Build

From the repo root:

```bash
pnpm build:wasm
```

Rebuild after any change to `crates/core` or this crate; the web app links
against the output.

## Artifacts

The build emits `rillio_core_web*` files (wasm, JS glue, typings) into this
directory. They are gitignored build outputs, consumed by `apps/web` through
the pnpm workspace.

This package is internal. It is not published to npm or crates.io.
