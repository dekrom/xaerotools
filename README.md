<div align="center">

# XaeroTools

[![CI](https://github.com/dekrom/xaerotools/actions/workflows/ci.yml/badge.svg)](https://github.com/dekrom/xaerotools/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/dekrom/xaerotools)](https://github.com/dekrom/xaerotools/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Browse, back up and share your Xaero's World Map — outside the game.**

**[Download for Windows / macOS / Linux](https://github.com/dekrom/xaerotools/releases/latest)** — unzip, double-click, done.

<img src="assets/viewer.webp" alt="A 2b2t archive in the XaeroTools viewer — spawn and its highway web, with overlays, waypoints, live share and merge tools open" width="900">

*A real 2b2t archive in the viewer: spawn's highway web, XaeroPlus overlays,
waypoint vault, live share and the merge tools.*

</div>

One small program on your PC. It reads the map folders your game already
writes and gives you:

- 🗺️ **A map viewer in your browser** — pan and zoom your whole archive like
  Google Maps, from block level out to the world border.
- 🔒 **Waypoint backup** — a waypoint deleted in game is never gone.
- 🧩 **Safe merging** — combine map folders and XaeroPlus databases without
  losing a tile or touching your originals.
- 📡 **A live shared map** — see your friends move and map in real time, on
  one map built from everyone's exploring.

No accounts, no telemetry, nothing leaves your machine. The only online
feature (the 2b2t Atlas overlay) is off until you turn it on.

## The live map in action

<div align="center">
<img src="assets/demo.gif" alt="Live share: a player runs a nether highway and the shared map paints their surroundings in real time" width="760">

*Following a player down a nether highway — terrain streams onto the shared
map as they travel, before the game has even saved it.
([full-quality video](assets/demo.mp4))*

</div>

## Get started

1. **Download** the [latest release](https://github.com/dekrom/xaerotools/releases/latest)
   for your system:

   | System | File |
   |---|---|
   | Windows | `xaerotools-windows-x86_64.zip` |
   | Linux | `xaerotools-linux-x86_64.zip` |
   | macOS (Apple Silicon) | `xaerotools-macos-arm64.zip` |
   | macOS (Intel) | `xaerotools-macos-x86_64.zip` |

2. **Unzip and run** `xaerotools` — a single portable file, no installer, no
   admin rights. A console window opens: that window is the app — keep it
   open; closing it stops the map.

   > **Windows**: the first run shows a blue "Windows protected your PC"
   > screen. The app is unsigned, not unsafe — every download is built in
   > public by GitHub Actions and checksummed in `SHA256SUMS.txt`. Click
   > **More info**, then **Run anyway**. Only needed once.

   > **macOS**: the first run is blocked ("Apple could not verify…"). Unblock
   > it once: open Terminal (Cmd+Space, type `Terminal`), paste
   > `xattr -d com.apple.quarantine` followed by a space, drag the unzipped
   > `xaerotools` file into the window, press Enter. Then double-click it.
   > (Alternative: System Settings → Privacy & Security → **Open Anyway**.)

3. **The browser opens by itself with your map.** It finds maps from the
   vanilla launcher and from CurseForge, Modrinth App, Prism Launcher,
   MultiMC, ATLauncher and GDLauncher instances automatically. If nothing was
   found, the page that opens lets you point at your folder — no terminal
   needed.

Backups or maps somewhere else? Add them in the viewer — **World panel →
Map roots → Browse** — or point at them from a terminal:

```
.\xaerotools serve --root "D:\backups\xaero" --root "C:\Users\you\.minecraft" --open
```

(To open a terminal in the unzipped folder on Windows: type `cmd` into the
Explorer address bar and press Enter. PowerShell needs the `.\` prefix; plain
`xaerotools` also works in cmd.)

## The map viewer

Your whole archive, rendered in the browser — including the old-format
regions from the 1.12 era that make up most of a long-lived 2b2t map.

- **Coverage view**: instantly see everything you have ever explored.
- **Instant deep zoom**: a persistent render pyramid means even a 100+ GB
  archive opens zoomed-out in seconds — every region is decoded once, ever.
- **XaeroPlus overlays**: NewChunks, OldChunks, Portals and friends drawn on
  the map, toggleable per database.
- **Waypoints**: searchable (emoji included), with copyable teleport commands.
- **Nether ⇄ Overworld**: see your nether highways under the overworld at 1:8
  scale, with coordinate conversion.
- **2b2t Atlas** (optional): overlay 1200+ community-documented locations
  from [2b2tatlas.com](https://2b2tatlas.com), filter by tag, jump to wiki
  and video links. You can also mirror the Atlas map imagery once with
  `scripts/atlas-mirror.py` and see it under the parts you haven't explored,
  served entirely from your own disk.
- Region grid, world border and highway guides, a measure tool, permalinks.

## Never lose a waypoint again

Every time XaeroTools starts, it backs up all waypoints from every account
and instance into its own vault. The same waypoint seen from three accounts
becomes one entry. If a waypoint disappears from the game — deleted, corrupted,
wrong file overwritten — it stays in the vault, marked *archived*, and can be
exported straight back into game-ready files:

```
xaerotools waypoints sync                                    # back up right now
xaerotools waypoints list --world Multiplayer_2b2t
xaerotools waypoints export --world Multiplayer_2b2t -o restore/ --include-archived
```

Copy the exported files into any account's `.minecraft/xaero/minimap/` (game
closed) and the waypoints are back in game.

## Merging map folders

Combine an old backup with your current map, or your map with a friend's —
in the viewer's **Tools** tab, or from a terminal. Every merge is a **dry run
first** — it prints exactly what it would do and changes nothing until you
add `--apply` (**Apply** in the Tools tab):

```
.\xaerotools merge "D:\old-backup" "C:\...\xaero" -o "D:\merged"          # preview
.\xaerotools merge "D:\old-backup" "C:\...\xaero" -o "D:\merged" --apply  # do it
```

- Where both sides mapped the same region, it merges **tile by tile** —
  newest wins, nothing is clobbered.
- `Multiplayer_2b2t` vs `Multiplayer_2b2t.org` style splits are detected and
  paired up for you.
- XaeroPlus databases merge too, keeping your oldest "first seen" history.
- Your source folders are **never modified**, and the output must be an
  empty folder.

`.\xaerotools db-merge A.db B.db -o Merged.db --apply` merges databases on
their own.

## Playing together

The map is live while the game runs: new exploring shows up in the browser by
itself, no reloads.

With the **[companion Meteor addon](https://github.com/dekrom/xaerotools-companion)**,
your group can run one shared map:

- Everyone appears as a **live marker** with a player list, click-to-follow
  and optional trails.
- A **live preview** sketches the terrain each player is currently seeing
  onto the shared map, seconds ahead of the real data.
- Freshly mapped regions upload automatically. The server keeps a private
  per-player backup **and** merges everyone's exploring into one shared map.
  Cave layers stay local unless a client opts in — and the server can refuse
  them outright with `--ingest-no-caves`.
- `.xt sync` uploads a whole existing map once — instant off-site backup.

Any Companion release works with any XaeroTools release 0.2 or newer.

Each player gets their own access token — generate it in the map's **Share
panel**, or with `xaerotools tokens generate <name>`. To share on your LAN:

```
xaerotools serve --lan --password mysecret
```

The first `--lan` run makes Windows Firewall ask to allow network access —
tick **Private networks**. For friends outside your LAN, use a VPN like
Tailscale — the server speaks plain HTTP on purpose.

## Every map version since 1.12

A long-lived archive is not one format: on a real 2b2t archive measured here,
62% of the main overworld folder was still in the 1.12-era format, next to
current-version data in the same folder. XaeroTools reads **every save
version the game itself can read** and writes the current format, which loads
straight back into the game. Vanilla Xaero's and XaeroPlus maps both work,
including mixed archives.

## Why you can trust it

- Open source (MIT), small dependency tree, no telemetry.
- One portable binary — no installer, no services, nothing running in the
  background.
- Listens on `127.0.0.1` only, unless you explicitly share with
  `--lan --password`.
- Merges are dry-run by default, atomic, and self-checked — and tested
  against a 1,563-region corpus of real 2b2t data: re-encoding current-format
  regions is **bit-for-bit identical**.
- A broken region file never takes the viewer down — it leaves a hole in the
  map and gets listed in diagnostics.

<details>
<summary><b>Building from source</b></summary>

```
# Rust 1.85+ (Node 20+ only if you change the web UI)
cargo build --release
./target/release/xaerotools
```

Or use the one-shot scripts: `./setup.sh` (Linux/macOS) or
`powershell -ExecutionPolicy Bypass -File setup.ps1` (Windows) — they install
the Rust toolchain if missing and build everything. On Linux, `./install.sh`
puts `xaerotools` on your PATH with an app-menu launcher.

The block/biome color table (`assets/colortable.bin`) ships pre-generated.
Regenerate it from any official client jar with:

```
cargo run -p xaero-colorgen -- --mc-version 1.21.8 --out assets/colortable.bin
```

It contains only derived per-block average colors — no Mojang assets are
redistributed. The full format documentation lives in `docs/PLAN.md`, and the
live-share client contract in `docs/INGEST.md`.

</details>

## FAQ / something didn't work

- **Windows says it "protected your PC".** Normal for an unsigned app —
  click **More info**, then **Run anyway**. See [Get started](#get-started).
- **macOS says it can't verify the app.** A one-time unblock — see
  [Get started](#get-started).
- **My antivirus flags it.** A false positive that small unsigned programs
  get.
  Every download is built in public by GitHub Actions from this source, and
  `SHA256SUMS.txt` on the release page lets you verify your file.
- **It says "port 45746 is busy — using 45747 instead".** That's fine — use
  the address the window prints.
- **No maps were found.** The page that opens lets you add your map folder
  right in the browser. Where to look: CurseForge keeps instances under
  `C:\Users\YOU\curseforge\minecraft\Instances\PACK\xaero`, the Modrinth App
  under `%APPDATA%\ModrinthApp\profiles\PROFILE\xaero`.
- **Where does XaeroTools keep its own data?** `%APPDATA%\xaerotools` on
  Windows, `~/.local/share/xaerotools` elsewhere. Your game folders are only
  ever read.
- **Does anything go online?** No — everything stays on your machine. The
  two opt-ins are the 2b2t Atlas overlay and `--lan` sharing.
- **I closed the black window and the map stopped.** By design — that window
  is the app. Double-click `xaerotools` to start it again.

## Roadmap

- Zero-install browser version (WASM) — open your folder in a tab, run
  nothing.
- JourneyMap 1.12.2 archive import.

## Credits & prior art

- [Xaero96](https://chocolateminecraft.com/) — Xaero's World Map / Minimap.
- [rfresh2/XaeroPlus](https://github.com/rfresh2/XaeroPlus) — the databases
  this tool understands, and the best living reference of the save pipeline.
- [Gjum's xaero-format notes](https://github.com/Gjum/voxelmap-cache/blob/master/xaero-format.md)
  and [DanDucky/XaerosMapFormat](https://github.com/DanDucky/XaerosMapFormat) —
  community format documentation.
- [rebane2001's Coordman](https://github.com/rebane2001) — the 1.12.2-era
  JourneyMap browser viewer this project is a spiritual successor to.
- [2b2t Atlas](https://2b2tatlas.com) — the community location database.

## License

MIT.
