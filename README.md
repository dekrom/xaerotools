<div align="center">

# XaeroTools

[![CI](https://github.com/dekrom/xaerotools/actions/workflows/ci.yml/badge.svg)](https://github.com/dekrom/xaerotools/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/dekrom/xaerotools)](https://github.com/dekrom/xaerotools/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Browse, back up and share your Xaero's World Map — outside the game.**

**[Download for Windows / macOS / Linux](https://github.com/dekrom/xaerotools/releases/latest)** — unzip, double-click, done.

[Setup guide](#get-started) · [Share a map with your friends](#playing-together) · [Something didn't work?](#faq--something-didnt-work)

</div>

One small program on your PC. It reads the map folders your game already
writes and gives you:

- 🗺️ **A map viewer in your browser** — pan and zoom your whole archive like
  Google Maps, from block level out to the world border.
- 🔒 **Waypoint backup** — a waypoint deleted in game is never gone.
- 🧩 **Safe merging** — combine map folders and XaeroPlus databases without
  losing a tile or touching your originals.
- 📡 **A live shared map** — see your friends move and map in real time, on
  one map built from everyone's exploring. Terrain paints in as they travel,
  seconds before the game has saved it.

No accounts, no telemetry, nothing leaves your machine. The only online
feature (the 2b2t Atlas overlay) is off until you turn it on.

## Get started

Three steps, no installer, no account, nothing to configure.

### 1. Download

Pick the one file for your system from the
**[latest release](https://github.com/dekrom/xaerotools/releases/latest)**:

| Your system | File to download |
|---|---|
| **Windows** (any modern PC) | `xaerotools-windows-x86_64.zip` |
| **macOS** — Apple Silicon (M1–M4) | `xaerotools-macos-arm64.zip` |
| **macOS** — Intel | `xaerotools-macos-x86_64.zip` |
| **Linux** | `xaerotools-linux-x86_64.zip` |

Not sure which Mac you have? **Apple menu → About This Mac**. A chip that starts
with "Apple M" means Apple Silicon; anything saying Intel means Intel.

### 2. Unzip it

Right-click the downloaded `.zip` → **Extract All** (Windows) or double-click
it (macOS/Linux). You get a folder with three files: `xaerotools` (the
program), `START-HERE.txt` and `LICENSE`.

Unzip it somewhere you can find again — your Desktop is fine. **Do not run it
from inside the zip**; Windows will appear to work and then fail to save
anything.

### 3. Run it

Double-click **`xaerotools`**. A black console window opens — **that window is
the app**. Keep it open; closing it stops the map. Your browser then opens by
itself at `http://127.0.0.1:45746` with your map already loaded.

The first run on Windows and macOS needs one extra click, because the download
is not code-signed (a certificate costs hundreds per year). Every file is
built in public by GitHub Actions straight from this source, and
`SHA256SUMS.txt` on the release page lets you verify what you downloaded.

<details>
<summary><b>Windows — "Windows protected your PC"</b></summary>

A blue box appears. Click **More info**, then the **Run anyway** button that
appears underneath. Once, ever.

If SmartScreen is set to block instead of warn, or your antivirus quarantines
the file: right-click the `.zip` before extracting → **Properties** → tick
**Unblock** → **OK**, then extract again.

</details>

<details>
<summary><b>macOS — "Apple could not verify this app"</b></summary>

Easiest way: **System Settings → Privacy & Security**, scroll down — right
after you try to open it there is a line about `xaerotools` being blocked,
with an **Open Anyway** button. Click it, then confirm.

Or from Terminal (Cmd+Space, type `Terminal`, Enter). Type this, **with a
space at the end**:

```
xattr -d com.apple.quarantine 
```

then drag the unzipped `xaerotools` file into the Terminal window (it pastes
the path for you) and press Enter. Now double-click it.

</details>

<details>
<summary><b>Linux — it will not start</b></summary>

Mark it executable, then run it:

```
chmod +x xaerotools
./xaerotools
```

</details>

### What you should see

The console window prints the address it is serving on and how many maps it
found, then your browser opens. XaeroTools looks for map folders on its own —
the vanilla launcher plus CurseForge, Modrinth App, Prism Launcher, MultiMC,
ATLauncher and GDLauncher instances.

**Found nothing?** The page that opens has a folder picker — click **Browse**,
point it at your map folder, done. No terminal needed. You are looking for a
folder called `xaero` (or `XaeroWorldMap` inside it). Common locations:

| Launcher | Where the map folder lives |
|---|---|
| Vanilla launcher (Windows) | `%APPDATA%\.minecraft\xaero` |
| Vanilla launcher (macOS) | `~/Library/Application Support/minecraft/xaero` |
| Vanilla launcher (Linux) | `~/.minecraft/xaero` |
| CurseForge | `C:\Users\YOU\curseforge\minecraft\Instances\PACK\xaero` |
| Modrinth App | `%APPDATA%\ModrinthApp\profiles\PROFILE\xaero` |
| Prism / MultiMC | `…\instances\INSTANCE\.minecraft\xaero` |

You can add as many folders as you like — old backups, a friend's copy, an
external drive. They all show up in the same viewer, and XaeroTools only ever
*reads* them.

<details>
<summary><b>Starting it from a terminal instead (optional)</b></summary>

```
xaerotools serve --root "D:\backups\xaero" --root "C:\Users\you\.minecraft" --open
```

To open a terminal in the unzipped folder on Windows, type `cmd` into the
Explorer address bar and press Enter. In PowerShell you need the `.\` prefix
(`.\xaerotools`); in `cmd` plain `xaerotools` works.

</details>

## The map viewer

Your whole archive, rendered in the browser — including the old-format
regions from the 1.12 era that make up most of a long-lived 2b2t map.

- **Coverage view**: instantly see everything you have ever explored.
- **Instant deep zoom**: a persistent render pyramid means even a 100+ GB
  archive opens zoomed-out in seconds — every region is decoded at most once
  per version of it on disk, across restarts.
- **XaeroPlus overlays**: NewChunks, OldChunks, Portals and friends drawn on
  the map, toggleable per database — each with its own colour swatch and an
  opacity slider, plus one button to put the whole panel back to defaults.
  Your colours travel in the permalink; opacity stays on the screen you set
  it on.
- **Waypoints**: searchable (emoji included), with copyable teleport commands.
- **Nether ⇄ Overworld**: see your nether highways under the overworld at 1:8
  scale, with coordinate conversion.
- **2b2t Atlas** (optional): overlay 1200+ community-documented locations
  from [2b2tatlas.com](https://2b2tatlas.com), filter by tag, jump to wiki
  and video links. From a source checkout you can also mirror the Atlas map
  imagery once with `scripts/atlas-mirror.py` (needs Python 3) and see it
  under the parts you haven't explored, served entirely from your own disk.
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

The map is live on its own: while the game runs, new exploring appears in the
browser without reloading anything.

To build **one shared map from everyone's exploring**, one person runs
XaeroTools and everyone installs the
**[Companion addon](https://github.com/dekrom/xaerotools-companion)**. You get
live player markers, a terrain preview seconds ahead of the saved data, and
every player's freshly-mapped regions merged into a single map — plus a
private per-player backup on the host.

### Who needs what

| | Needs |
|---|---|
| **The host** — one person, the "server" | XaeroTools running on their PC, left open. A decent upload and a machine that stays on. |
| **Each player** — including the host | Fabric + Meteor Client + Xaero's World Map + the Companion addon |
| **Watching only** | Nothing. Just the map's address in a browser. |

The host does **not** need a dedicated server, a VPS or a domain. A normal PC
is fine. Any Companion release works with any XaeroTools release 0.2 or newer.

### Step 1 — the host starts the server

Pick the situation that matches your group.

<details open>
<summary><b>A. Just testing on your own PC</b></summary>

Run XaeroTools normally (double-click it). The address is
`http://127.0.0.1:45746` and the addon's defaults already point there. **No
token needed** — the addon and the server are on the same machine.

Skip straight to [Step 3](#step-3--each-player-installs-the-mods).

</details>

<details>
<summary><b>B. Friends in the same house (same Wi-Fi/router)</b></summary>

Open a terminal in the unzipped folder and start it in LAN mode. A password is
mandatory — without it everyone on the network could read your map:

```
xaerotools serve --lan --password pick-a-password
```

Windows will pop up a firewall prompt the first time. **Tick "Private
networks" and allow it** — if you miss this, nobody can connect.

Now find your PC's local address:

| | Command | Look for |
|---|---|---|
| Windows | `ipconfig` | `IPv4 Address` — e.g. `192.168.1.42` |
| macOS / Linux | `ip addr` (or `ifconfig`) | the `192.168.x.x` / `10.x.x.x` address |

Your group's address is then `http://192.168.1.42:45746` — substitute your own
number. Give players that address and the password.

</details>

<details>
<summary><b>C. Friends over the internet (recommended: Tailscale)</b></summary>

XaeroTools speaks plain HTTP on purpose, so do **not** port-forward it to the
open internet. Put everyone on the same private network instead —
[Tailscale](https://tailscale.com) is free for personal use and takes about
two minutes:

1. Install Tailscale on the host **and** on each player's PC, and sign in with
   the same account (or share the network with them from the Tailscale admin
   panel).
2. On the host, get its Tailscale address: `tailscale ip -4` — it looks like
   `100.101.102.103`.
3. Start the server the same way as LAN mode:

   ```
   xaerotools serve --lan --password pick-a-password
   ```

4. Players use `http://100.101.102.103:45746`.

This is encrypted end to end by Tailscale, works from anywhere, and needs no
router changes. A TLS reverse proxy in front of XaeroTools is the other valid
option if you already run one.

</details>

### Step 2 — the host makes one token per player

Each player gets their own token, tied to their account name. On the **host's**
machine, in a second terminal (the server can keep running — new tokens are
picked up immediately, no restart):

```
xaerotools tokens generate Notch
```

It prints the token **once**. Copy it and send it to that player privately —
a DM, not a public channel. Repeat per player. `xaerotools tokens list` shows
who has one; `xaerotools tokens revoke <name>` takes it back.

> **Note:** the map's **Share panel** can also mint tokens, but only when the
> server is running unprotected on your own machine. Starting it with
> `--lan --password` deliberately disables token, merge and map-root
> management in the web UI — otherwise anyone with the password could mint
> credentials. **When you are sharing, use the command above.**

The name must match the Minecraft account that will use it. Players on the
same PC using several alts can put one `NAME=TOKEN` line per account into the
addon's `account-tokens` list.

### Step 3 — each player installs the mods

In order. All four go in the same `mods` folder:

1. **[Fabric Loader](https://fabricmc.net/use/installer/)** for your Minecraft
   version — run the installer, pick your version, install.
2. **[Meteor Client](https://meteorclient.com/)** — download the jar for the
   same Minecraft version.
3. **[Xaero's World Map](https://modrinth.com/mod/xaeros-world-map)** — this is
   what actually draws the map the addon uploads.
   ([XaeroPlus](https://github.com/rfresh2/XaeroPlus) on top is optional and
   works great.)
4. **The Companion jar** — from the
   [latest Companion release](https://github.com/dekrom/xaerotools-companion/releases/latest),
   **exactly one**, matching your Minecraft version.

Where the `mods` folder is:

| Launcher | Path |
|---|---|
| Vanilla launcher (Windows) | `%APPDATA%\.minecraft\mods` |
| Vanilla launcher (macOS) | `~/Library/Application Support/minecraft/mods` |
| Vanilla launcher (Linux) | `~/.minecraft/mods` |
| CurseForge / Prism / MultiMC / Modrinth | that instance's own `mods` folder |

Paste the address into Windows Explorer or press Cmd+Shift+G in Finder. Start
the game once to confirm all four load.

### Step 4 — each player points the addon at the server

In game, open Meteor's GUI (**Right Shift** by default) and click the
**XaeroTools** tab in the top bar, next to Config. Under **Connection**:

| Setting | What to put |
|---|---|
| `server-url` | The host's address, e.g. `http://192.168.1.42:45746` |
| `token` | The token the host sent you. Leave empty if the server runs on *your* PC. |
| `player-name` | Leave empty — it uses your account name. Only set it if your token was made for a different spelling. |

Then flip **enabled** on (or type `.xt on` in chat). `.xt status` tells you
whether it is connected.

### Step 5 — the first upload

Each player runs this once, in chat:

```
.xt sync
```

That uploads the map you already have — the initial backup. It can take a
while on a big archive; `.xt status` shows the queue draining. Afterwards the
watcher keeps everything current by itself: explore, and regions upload
seconds after the game saves them.

### Watching the shared map

Anyone opens the host's address in a browser — `http://192.168.1.42:45746`,
or `127.0.0.1:45746` on the host itself. With `--lan` you are asked for the
password once. You will see live markers for everyone connected, a Players
panel with click-to-follow, and the merged map filling in as people explore.

### Chat commands

| Command | Does |
|---|---|
| `.xt on` / `.xt off` | Turn the live link on or off |
| `.xt status` | Connection, queue length, what has been sent |
| `.xt sync` | Upload your whole existing map once |
| `.xt sync <world>` | Same, but only one world |
| `.xt ping` | Send one position now — tests the connection |

### What actually gets shared

Your position, and the map regions your game saves. **Cave layers stay on your
PC** unless you turn `upload-caves` on, and the host can refuse them outright
with `--ingest-no-caves`.

If you run XaeroPlus, the chunks it finds are shared too — `highlight-sync` is
**on by default**. That is rows out of nine databases (new chunks by either
detection and their inverses, old/modern chunks, portals, old biomes and
breadcrumb trails), so the group map shows everyone's finds and not just your
own. Only modules you have enabled produce anything, the databases themselves
never leave your PC — only the rows — and none of it runs against a server on
your own machine, which reads those databases directly anyway. Turn it off in
the **XaeroTools** tab if you would rather keep your finds to yourself.

The addon only ever *reads* your Xaero folder — your own local map keeps
working exactly as before. Nothing is sent anywhere except the server address
you typed in.

### If it is not working

| Symptom | Fix |
|---|---|
| `.xt status` says not connected | Check `server-url` — it needs `http://` and the port, e.g. `http://192.168.1.42:45746`. |
| Connection refused / times out | The server is not running, or the firewall blocked it. On Windows, allow `xaerotools` on **Private networks**. |
| `401 unauthorized` | Wrong or revoked token, or it was minted for a different account name. Host: `xaerotools tokens list`, then re-issue. |
| Works on the host, not for friends | The server was started without `--lan`. Plain `serve` listens on `127.0.0.1` only. |
| Nobody can reach it over the internet | Use Tailscale (option C). Do not port-forward plain HTTP. |
| Marker moves, but no terrain appears | Xaero's World Map is not installed, or the game has not saved that area yet. Run `.xt sync` once. |
| Browser asks for a password you did not set | That is `--lan --password`; ask the host for it. |
| Share panel says tools are disabled | Expected under `--lan` — use the `tokens` CLI on the host (Step 2). |

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
- **Do I need to close Minecraft first?** No. Run both at once — that is the
  point. XaeroTools only reads the map files the game writes.
- **Will this change or break my in-game map?** No. Your game folders are
  opened read-only. Merges never touch the originals: they write a new folder
  and are a dry run until you add `--apply`.
- **How do I update to a new version?** Download the new zip and replace the
  old `xaerotools` file. Your settings, tokens and waypoint vault live
  elsewhere (`%APPDATA%\xaerotools` / `~/.local/share/xaerotools`) and are
  kept.
- **Can I run it on a home server / Raspberry Pi?** Yes — any always-on Linux
  box works, with `--lan --password`. It is a single binary with no services
  to install. The prebuilt Linux download is x86-64 only, so on an ARM board
  (a Pi included) build from source instead.

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
