# XaeroTools — dev & testing bundle

> This document is staged at the root of the portable dev bundle by
> `scripts/make-bundle.sh`. The `sample data/` corpus and `bin/` prebuilt
> binary it describes are bundle-only — not part of this repository (the
> test suite skips corpus tests when `sample data` is absent).

Everything you need to run, test and keep developing XaeroTools on any machine.

## What's inside

| Path | What it is |
|---|---|
| `xaerotools/` | Full source repo **with git history** — your working copy |
| `sample data/` | The 2b2t test corpus (1,563 regions, DBs, waypoints) the test suite runs against — keep it next to `xaerotools/` |
| `bin/xaerotools-linux-x86_64` | Prebuilt Linux binary — runs immediately, no build needed |

## One-liner setup (builds from source)

**Linux / macOS** — after unzipping:

```
cd xaerotools && ./setup.sh
```

**Windows** (PowerShell):

```
cd xaerotools; powershell -ExecutionPolicy Bypass -File setup.ps1
```

That's the whole setup: it installs the Rust toolchain if missing (user-local,
no admin), builds the release binary, and self-checks the format codec against
the sample corpus. **Node.js is not needed** — the web UI ships prebuilt and
gets embedded into the binary. Add `--serve` to `setup.sh` to launch the
viewer right after building.

## Run it

```
./target/release/xaerotools                 # auto-detects .minecraft, starts the viewer
./target/release/xaerotools help            # all commands (merge, db-merge, waypoints vault…)
```

On Linux you can skip the build entirely: `bin/xaerotools-linux-x86_64` is
ready to run.

## Testing against your real data

```
./target/release/xaerotools serve --root "C:\Users\you\.minecraft" --open
```

First interesting things to try on a 300 GB archive: cold-start time to first
tiles, deep-zoom coverage view of your full footprint, XaeroPlus overlay
toggles (OldChunks/Portals), and `xaerotools waypoints sync` to take the first
full vault backup of every account's waypoints.

## Developing

- `cargo test --workspace` — the full suite (the codec round-trip needs
  `sample data/` next to the repo, or set `XAERO_CORPUS=/path/to/sample-data`).
- If you change the web UI: `cd webui && npm install && npm run build`, then
  `cargo clean -p xaerotools-server && cargo build` (the UI is embedded at
  compile time).
- The verified byte-level format spec and full project plan: `docs/PLAN.md`.
- Live-share (positions + map streaming between accounts) design: `docs/adr/007-live-share-seam.md`.
