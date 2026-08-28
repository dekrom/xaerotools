# XaeroTools — building and testing from source

Everything you need to build, run and keep developing XaeroTools from a clone
of this repository. If you only want to *use* it, take the zip for your system
from the [latest release](https://github.com/dekrom/xaerotools/releases/latest)
instead — no build, no toolchain.

## One-liner setup (builds from source)

**Linux / macOS**, from the repository root:

```
./setup.sh
```

**Windows** (PowerShell), from the repository root:

```
powershell -ExecutionPolicy Bypass -File setup.ps1
```

That's the whole setup: it installs the Rust toolchain if missing (user-local,
no admin), builds the release binary, and self-checks the format codec against
the sample corpus if one sits next to the repo. **Node.js is not needed** — the
web UI ships prebuilt and gets embedded into the binary. Add `--serve` to
`setup.sh` to launch the viewer right after building. On Linux `./install.sh`
also puts `xaerotools` on your PATH with an app-menu launcher.

## Run it

```
./target/release/xaerotools                 # finds your maps, opens the viewer in your browser
./target/release/xaerotools help            # usage: serve, merge, db-merge, waypoints, tokens, render, stats, doctor
```

Map folders are detected across the vanilla launcher and CurseForge,
Modrinth App, Prism Launcher (flatpak included), MultiMC, ATLauncher and
GDLauncher instances. With nothing found it still starts — the page that
opens in the browser lets you pick a folder, and the viewer's World panel
adds more roots later.

## Testing against your real data

```
./target/release/xaerotools serve --root "C:\Users\you\.minecraft" --open
```

First interesting things to try on a 300 GB archive: cold-start time to first
tiles, zooming out to your whole footprint, XaeroPlus overlay toggles
(OldChunks/Portals), and `xaerotools waypoints sync` to take the first full
vault backup of every account's waypoints.

## Developing

- `cargo test --workspace` — the full suite. The corpus-backed tests (the
  byte-identical codec round-trip and friends) are `#[ignore]`d, so this run
  reports them as **skipped**, never as passed. A green run therefore makes no
  claim about the corpus either way.
- To actually run them, point `XAERO_CORPUS` at a copy of the 2b2t sample
  corpus, which is not part of this repo, and ask for the ignored tests:

  ```
  XAERO_CORPUS=/path/to/sample-data cargo test --workspace -- --ignored \
    --skip optional_corpus_decodes_to_exact_eof
  ```

  Without `XAERO_CORPUS` they stop with an error rather than passing quietly.
  The `--skip` excludes one sweep over a **private** legacy archive that is not
  part of the public corpus; drop it if you have set `XAERO_LEGACY_CORPUS`.
- If you change the web UI: `cd webui && npm install && npm run build`, then
  `cargo clean -p xaerotools-server && cargo build` (the UI is embedded at
  compile time).
- The verified byte-level format spec and full project plan: `docs/PLAN.md`.
- Live-share (positions + map streaming between accounts) design:
  `docs/adr/007-live-share-seam.md`; the client contract it turned into is
  `docs/INGEST.md`.
