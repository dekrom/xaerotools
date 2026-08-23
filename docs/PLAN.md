# XaeroTools — Xaero's World Map toolset for 2b2t

## Context

The user is a long-time 2b2t player with **300+ GB** of Xaero's World Map / XaeroPlus data (millions of small region `.zip` files + multi-GB XaeroPlus SQLite DBs). On 1.12.2 they used **Coordman** (static Leaflet viewer over pre-rendered JourneyMap PNGs) to browse map data in the browser; nothing equivalent exists for Xaero's format — research confirmed **no browser renderer for `.xaero` regions exists anywhere** (open niche). This plan creates "XaeroTools", an open-source toolset for the whole 2b2t community:

1. **Coordman-like browser viewer** for modern Xaero World Map data — optimized for huge archives, minimal extra disk footprint.
2. **Folder merger** for two map-data trees (tile-granular, data-safe) + **XaeroPlus SQLite highlight-DB merger**.
3. **Optional 2b2t Atlas integration** (POI overlay + historical WDL tile layers) and waypoint display.
4. Future (explicitly deferred by user): live location/map sharing server + Meteor addon — only an architectural seam in v1.

## Decisions locked with the user

- **Performance-first**; must handle 300+ GB, millions of small zips, big SQLite DBs.
- **Live sharing: DEFERRED** (seam only, no implementation).
- **Format scope: everything the game reads** — region majors 0..=7, minor ≤ 8, plus pre-versioned saves; writer emits 7/8. `minor` selects the pixel/tile framing, `major` selects block/biome identity: major 0 stores a numeric 1.12 block id+meta and a numeric biome id, majors 1+ use NBT blockstates, majors 4+ palette their biomes. Legacy ids resolve through `assets/legacy_block_ids.bin` (baked from the jar's `vanilla_states.dat`, mod renames applied) and a built-in biome table. This is not optional: a real long-lived 2b2t archive is ~half major-0 (measured 61.6% of one overworld folder). No JourneyMap import.
- Distribution must not read as malware (user rejected bare unsigned .exe as the only channel); user delegated stack choice asking for "the most effective option" → **Rust workspace** (core crate kept WASM-compilable for a future zero-install browser build) + **vanilla TS + Leaflet + Vite web UI** embedded in one portable binary.
- Security: localhost-only by default, opt-in LAN with password, no telemetry, Atlas calls opt-in, mergers never touch sources.
- New repo at `<workspace>/xaerotools/` (cwd keeps reference material: XaeroPlus source, coordman-main, sample data, mod jars).

---

## Ground truth (verified research digest)

Facts below were derived from XaeroPlus-26.2 source + jar bytecode and **verified byte-for-byte against real sample regions**; treat as spec.

### Region binary format
Container: `<rx>_<rz>.zip` (region = 512×512 blocks; `regionX = blockX >> 9`), single entry `region.xaero`, deflate. Written `.zip.temp` → atomic rename. Legacy bare `.xaero` = same stream uncompressed. Region regex `^(-?\d+)_(-?\d+)\.(zip|xaero)$`.

Stream (big-endian DataOutputStream): `u8 0xFF` marker + `i32 (major<<16)|minor`. Samples: 1.21.4 = **6.8**, 1.21.8 = **7.8** (current; loader rejects newer and backs the file up). Then repeat until EOF (no terminator): `u8 chunkMarker=(cx<<4)|cz` (8×8 MapTileChunks) → 4×4 MapTiles each: `i32 -1` if absent, else 16×16 pixels then trailers `u8 worldInterpretationVersion` (minor≥4), `i32 writtenCaveStart` (minor≥6), `u8 writtenCaveDepth` (minor≥7).

Per pixel `i32 params`: bit0 not-grass → blockstate follows (`i32` palette index, or inline NBT CompoundTag first time + bit21); bit1 overlays (`u8 count` + overlays); bits 8–11 light; bits 12–19 + 25–28 (shift 25 when minor≥4, else 24) = **signed 12-bit height**; bit20 biome (`i32` palette idx or `writeUTF` name + bit22); bit24 → separate `u8 topHeight` (quirk: shipped writer truncates topHeight to u8). Legacy read-only bits: 2–3, 4, 6, 23. Overlay `i32 overlayParams`: bit0 not-water → state (shares blockstate palette), bits 4–7 light, bits 11–14 opacity (minor≥8), bit10 NBT-follows; legacy bits 1,2,3,8–9. Per-region palettes (blockstates+overlays share one; biomes separate), index = first-appearance order → **pixel bytes cannot be spliced across regions; merge = decode → merge tiles → re-encode fresh palettes**. Blockstates/biomes stored as NBT/namespaced strings (no colors in file) → byte-compatible across MC versions; viewer must supply its own color tables. Truncation-tolerant (loader stops at EOF).

`cache_<globalVersion>/<rx>_<rz>.xwmc` (+ `cache/<level>/`, `.outdated`): regenerable derived render caches (entry `cache.xaero`, i16 major/minor 1.24 vs 2.24) — never merge, never copy, drop after merges. Cave layers `caves/<n>/` use the identical region format (populated in 1.21.8 samples, layers 1–6, incl. nested `caves/1/cache/3/`).

### Folder layout / naming
`<gameDir>/xaero/world-map/<worldId>/` with worldId = `Multiplayer_<ip>` | `Multiplayer_<serverListName>` | base-domain (`Multiplayer_2b2t.org`) per XaeroPlus `dataFolderResolutionMode` → **same server commonly has 2+ trees (prime merge target; present in samples)**. Dim folders: `null` overworld (or `DIM0` if XaeroPlus `nullOverworldDimensionFolder=false`), `DIM-1`, `DIM1`, else escaped `namespace$path` with `/`→`%` (e.g. `minecraft$worlds%2b2t%2b2t_1`). Multiworld `mw$default` / `mw$<intHash>`. Per-dim `dimension_config.txt` is authoritative: repeatable `MWName:<mwId>:<display>` + `caveModeType` + `dimensionTypeId`. Per-server `server_config.txt` (teleport formats). Minimap: `<gameDir>/xaero/minimap/<worldId>/dim%<n>/mw$<id>_1.txt`, records `waypoint:` + 13 colon-separated fields (names contain emoji/spaces and escaped colons → parse by splitting the fixed 12-field tail from the right); minimap `config.txt` maps `dimensionType:<levelId>:<dimTypeId>` (escaping there: only `:`→`$`).

### XaeroPlus SQLite
Location `<world-map>/<worldId>/XaeroPlus<Name>.db` (sibling of dim folders): NewChunks, NewChunksLiquidInverse, PaletteNewChunks(+Inverse), OldChunks, ModernChunks, Portals, OldBiomes, Breadcrumbs, LavaColumns, Drawing. Highlight schema **v2** (current): per-dimension table `"minecraft:overworld"(x,z,foundTime, PRIMARY KEY(x,z)) WITHOUT ROWID` + `metadata(id,version)`; x/z chunk coords, foundTime epoch ms. **v1** (what the samples have): rowid tables + `unique_xz_*` unique indexes. **v0**: tables `"0"/"-1"/"1"`, no metadata. Migration semantics in `V0ToV1Migration.java` / `V1ToV2Migration.java`. Drawing DB differs: `<dim>-highlights/-lines/-ellipses/-texts` tables, different metadata shape, samples at v0 (no ellipses), all sample drawing tables empty. Pragmas WAL + synchronous=NORMAL + busy_timeout 5000. Game inserts `INSERT OR IGNORE` batches of 25k. Custom-dimension tables appear (e.g. `"minecraft:worlds/2b2t/2b2t_1"`) → always enumerate `sqlite_master`. Row counts up to ~290k/table in samples. XaeroPlus writes **byte-identical region files** (only I/O-path opts) → one codec covers vanilla + XaeroPlus users. Its `AtlasWaypointImport.java` already consumes the Atlas API (precedent). Xaero itself ships only an in-game PNG exporter — no interchange tooling exists.

### Coordman (what to keep / fix)
Static Leaflet 1.7 viewer + one-time Python tile bake; `L.CRS.Simple` (map coords = block coords, Z negated), 512px tiles, zooms 0…−16 (±33.5M). Keep: per-dim base layers, **Overworld+Nether 1:8 overlay via `zoomOffset:3`**, world-border/highway guide polylines, waypoint groups with toggles + free-form popups, sidebar fly-to, live coord readout. Fix: no search, no clustering/culling, dimension field on markers ignored (bug), XSS-by-design user-data.js, full pre-render duplicating disk.

### 2b2t Atlas API (probed)
Real backend `https://api.blackportal.cloud` (frontend is Blazor WASM; `/api` page looks broken but isn't the API). `GET /api/locations` → 1242 locations (name, description, tags comma-string, dimension 0=OW/2=End, x/y/z, wiki, videoUrl, dateAddedUtc, warps, attachments, renders); `GET /api/locations/{rowid}`. **No auth, CORS `*`** (browser can fetch directly), Cloudflare cache 60 s. `renders[]` = Leaflet-ready XYZ pyramids of historical WDL renders: `tilesPath .../{z}/{y}/{x}.png` (y before x), 256px tiles, `coordinateScheme: atlas-sparse-v1` → at z: `tile_index = floor(world/512) + 250·2^(z−9)`; **native z9 tile = 512×512 blocks = exactly one Xaero region**. `blackportal.cloud/AtlasTiles/` is autoindexed with whole-map WDL datasets (Overworld/Nether/End; 7k/100k/256k…). 2b2t itself runs 1.21.4; players connect 1.21.4–1.21.8 via ViaVersion → both layouts must work (they do differ only by region major 6 vs 7).

### Sample data (test corpus, `<workspace>/sample data`)
408 MB / 3303 files: 1563 region zips, 1478 .xwmc, 37 DBs, 41 txt. **Built-in merge fixture**: `Multiplayer_2b2t` exists in both version trees with `mw$default` overlap — null 307 vs 90 regions (20 same-name conflicts), DIM-1 296 vs 794 (71), DIM1 0 vs 4. File mtimes preserved by the mod → valid recency signal. Firsthand-verified headers: `ff 00 07 00 08` (1.21.8), `ff 00 06 00 08` (1.21.4); DB schema v1 + metadata (0,1) confirmed via sqlite3.

---

## Architecture

```
                          ┌─────────────────────────────────────────────┐
                          │              xaerotools (binary)            │
                          │  clap CLI: serve/scan/merge/db-merge/...    │
                          └───────┬─────────────────────┬───────────────┘
                                  │                     │
         ┌────────────────────────▼──────┐   ┌──────────▼──────────────┐
         │      xaerotools-server        │   │   merge drivers (CLI)   │
         │  axum; embedded webui assets  │   │  plan/apply, journal,   │
         │  /tiles /hl /api/* endpoints  │   │  reflink copy, reports  │
         └──────┬──────────┬─────────┬───┘   └───┬───────────┬─────────┘
                │          │         │           │           │
┌───────────────▼──┐  ┌────▼─────────▼───────────▼──┐  ┌─────▼──────────┐
│   xaero-scan     │  │        xaero-core           │  │   xaero-db     │
│ roots/worlds/dim │  │ codec (v6/v7 read, v7.8     │  │ rusqlite; XP   │
│ region indexes,  │  │ write) · model · merge ·    │  │ highlight v0/  │
│ tile disk cache  │  │ render · waypoints ·        │  │ v1/v2 + drawing│
│ (native only)    │  │ dimconfig  [WASM-clean]     │  │ merge (native) │
└──────────────────┘  └──────────────▲──────────────┘  └────────────────┘
                                     │ include_bytes!
                      ┌──────────────┴──────────────┐
                      │  colortable.bin (baked)     │◄── tools/xaero-colorgen
                      │  blockstate→RGBA + biome    │    (dev-time: Mojang jar via
                      │  tints, versioned artifact  │     piston-meta or --jar)
                      └─────────────────────────────┘

webui/ (Vite + vanilla TS + Leaflet, CRS.Simple) ──built──► embedded in binary
Browser ── localhost HTTP ──► server; Atlas API fetched client-side (CORS *), opt-in
```

### Workspace layout

```
xaerotools/                          (Cargo workspace)
├── crates/
│   ├── xaero-core/     lib — WASM-compilable; no tokio/rusqlite/fs in core
│   │   src/{codec/{reader,writer,nbt,zipio}.rs, model.rs, merge.rs,
│   │        render/{mod,colortable,shade}.rs, waypoints.rs, dimconfig.rs, naming.rs}
│   ├── xaero-db/       lib — XaeroPlus SQLite read + merge (rusqlite bundled)
│   ├── xaero-scan/     lib — root discovery, region index, tile disk cache (LRU)
│   ├── xaerotools-server/  lib — axum app, embeds webui/dist + colortable
│   └── xaerotools-cli/     bin `xaerotools`
├── tools/xaero-colorgen/   dev bin — MC jar → colortable.bin (never shipped to users)
├── webui/                  Vite + TS + Leaflet 1.9, no framework, no CDN deps
│   src/{main.ts, layers/{xaeroTiles,netherOverlay,highlights,atlas,coverage}.ts,
│        ui/{sidebar,search,measure,permalink}.ts, api.ts, style.css}
├── assets/colortable.bin   committed baked artifact
├── fuzz/  testdata/  .github/workflows/{ci.yml,release.yml}
```

Deps: `flate2` + `zip` (deflate only) in core; `rusqlite` (bundled) only in xaero-db; `axum`/`tokio`/`tower-http`; `clap`, `indicatif`, `reflink-copy`, `dirs`, `open`; pure-Rust `png` + `image-webp`; `rayon`; `criterion`. License MIT (we implement from our own spec; no Xaero/Mojang code or assets redistributed). CI enforces `cargo check -p xaero-core --target wasm32-unknown-unknown` so the future zero-install WASM build never regresses.

## Component designs

### 1. Region codec (`xaero-core::codec`)
Semantic model with **passthrough-plus-fields** principle: every pixel keeps its raw `params: u32` (legacy bits preserved verbatim); typed fields (`state: Grass|Palette(u32)`, `height: i16` 12-bit signed, `top_height: Option<u8>`, `light: u8`, `biome: Option<u32>`, `overlays: SmallVec<Overlay>`) are decoded views. Encoder re-emits passthrough bits, recomputes only palette-index/NBT-first-time flags (bits 21/22, overlay bit 10) and palette ordering → **byte-identical re-encode of 7/8 inputs** becomes the regression oracle; merged regions get fresh palettes naturally. `Region { chunks: [Option<TileChunk>;64] }` → `TileChunk { tiles: [Option<Tile>;16] }` → `Tile { pixels: [Pixel;256], world_interpretation_version, written_cave_start, written_cave_depth }`. `Palettes { states: Vec<NbtCompound>, biomes: Vec<String> }` (Java MUTF-8). API: `read_region_container` (zip→inflate or bare `.xaero`) / `decode_region` / `encode_region` (always 7/8) / `write_region_container`. Minimal in-crate NBT reader/writer matching Java `NbtIo` uncompressed framing. Accept majors 6/7 minor ≤8; newer → clear "update XaeroTools" error (mirrors mod behavior). Any mid-structure EOF → partial region with `truncated=true`, never panic.

### 2. Color table + renderer
`tools/xaero-colorgen` (dev-time only): fetch sha1-verified client jar via piston-meta (`--mc-version`) or `--jar`; blockstates→models→top-face texture (priority `up`→`top`/`end`→`all`/first) → average RGBA; tint class from `tintindex` + known lists (Grass/Foliage/DryFoliage/Water/None); biome table from `data/minecraft/worldgen/biome/*.json` (temperature/downfall sampled against `colormap/{grass,foliage}.png` + explicit overrides); hand-maintained alias table for 1.18–1.21 renames (`grass`→`short_grass`, `grass_path`→`dirt_path`, …). Output `assets/colortable.bin` (`XCT1`: interned name table, per-block `{rgba, tint, flags}`, aliases, per-biome grass/foliage/water/dry colors, provenance header; ~40–80 KB, `include_bytes!`).

`render_region(&DecodedRegion, &ColorTable, &RenderOpts) -> 512×512 RGBA`: state color (grass default → grass texture color) → biome tint by tint class (fallback plains) → overlays composited bottom-up (water default; alpha from 4-bit opacity + depth darkening) → Xaero-style heightShade vs west/north neighbors (±15%, configurable; 1-px region-border seam accepted in v1) → light bits multiplied for cave layers/DIM-1 (`LightMode::Multiply`), off for surface. Unknown blocks: alias → suffix heuristics (`*_leaves`, `*_ore`, `*_log`…) → neutral gray (+ `?debug=missing` magenta mode with one-time logging). Property-insensitive by design. v1 renders: surface + water/overlays + shading + **cave layers as selectable Layer dropdown** (same format, near-zero marginal cost, one-ups every existing tool). Out: light-glow effects, exact Xaero dithering.

### 3. Viewer at 300 GB scale (`xaero-scan` + server)
Tile identity: **one Xaero region = one native z0 512px tile**; Leaflet `CRS.Simple`, zooms −16…+3 (`maxNativeZoom:0`), same convention as Coordman and structurally aligned with Atlas z9.

Endpoints: `/api/roots|worlds|maps` (scan), `/tiles/{world}/{dim}/{mw}/{layer}/{z}/{x}/{y}.webp`, `/coverage/...` (index-only presence/age tiles), `/hl/{world}/{db}/{dim}/{z}/{x}/{y}.webp` (XaeroPlus highlights), `/api/waypoints`, `/api/layers` (data-driven overlay registry = live-share seam), `/api/pyramid/build` + SSE progress, `/api/config`, `/api/browse` (disabled under `--lan`).

Index: startup scans directory names only; first selection of (world, dim, mw, layer) triggers one readdir pass with the region regex, skipping `cache*`/`.outdated`/`.temp`. Entry = packed `i64` key → `{mtime_min:u32, size_kb:u32}` ≈ 16 B → 500k-region dim ≈ 8 MB RAM; full 300 GB ≤ 64 MB but lazy-per-map keeps residency tiny. Persisted snapshot keyed by dir path+mtime for instant reopen; manual/dir-mtime refresh, no inotify in v1.

Tile serving: cache key = `hash(region_path, file_mtime, size, colortable_ver, render_opts)`; miss → rayon pool decode→render→encode **lossless WebP** (PNG via flag) → temp+rename into `<cache_dir>/tiles/` with rusqlite LRU metadata DB, default cap **2 GiB** (`--cache-cap`); in-flight de-dup; ~128-tile in-memory hot cache; memory bounded by pool size (tens of MB). Safe while Minecraft runs: mod's atomic renames honored; on transient zip error retry once → serve stale → transparent; DBs opened `SQLITE_OPEN_READONLY` + `busy_timeout` + `query_only` (WAL-safe, never checkpoint live DBs); degrade with UI notice if locked.

Zoom-out **without a 300 GB pre-render** (three tiers): (1) **coverage tiles** from the filename index alone — full explored footprint visible instantly on first launch, optional mtime age-heat; (2) z −1…−3 composed on demand by downscaling child tiles through the same LRU; (3) **persistent deep pyramid** (z ≤ −4): background job renders every region once, folds upward (32 px/region at z−4 doubles as the thumbnail store); ~1–2 GB total for a 300 GB archive, own quota (default 4 GiB), incrementally maintained via per-region mtime journal (changed region dirties only its ancestor chain); ~40–70 min one-time per huge dim at 25 regions/s/core × 8 cores, UI fully usable meanwhile (coverage fallback), progress via SSE.

Roots: config list; first-run auto-detect `.minecraft/xaero` per platform + Prism/MultiMC instance globs offered as checkboxes (never silently added); arbitrary folders via server-side browser; accepts `xaero/` root, bare `world-map/`, or single world folder. Dim/mw names + semantics from `dimension_config.txt` (MWName, dimensionTypeId → 1:8 behavior + light mode, caveModeType) and `server_config.txt` (teleport formats reused for "copy /tp" in popups).

### 4. Web UI v1
Vanilla TS + Leaflet 1.9 + Vite, dark default, fully bundled (offline-capable, zero CDN/tracking). In v1: root/world/dim/mw pickers + layer dropdown (Surface / Cave N) + overlay toggle tree + opacity slider; **OW+Nether 1:8 combined mode** (`zoomOffset:3` underlay, dual coord readout, Nether⇄OW converter); canvas grid overlay (region 512 always, chunk 16 at high zoom); coord readout + click-to-copy + "Go to X Z" + permalink hash `#/{world}/{dim}/{mw}/{layer}/{x}/{z}/{zoom}?overlays=`; waypoints from minimap files (grouped by dim + set, Xaero 16-color markers, popups with copyable /tp, search incl. emoji, fly-to, **dimension-correct rendering — fixes Coordman's bug** — plus explicit "project OW waypoints into Nether ÷8" toggle, zoom-based culling for StashFinder-scale dumps); **XaeroPlus highlight overlays** per DB from `sqlite_master` (NewChunks, OldChunks, Portals, LavaColumns, …) as tinted translucent chunk tiles via bbox queries, aggregated at deep zoom by integer-division GROUP BY, click → foundTime date; **Atlas POIs** client-side fetch (CORS `*`; nothing fetched until user enables the layer, choice persisted; tag filter chips, dimension-aware, wiki/video popups, in search); **Atlas historical WDL base layers** (experimental: curated list + paste-a-tilesPath; custom TileLayer implementing atlas-sparse-v1 with y-before-x); measurement polyline (per-segment + total, Nether-equivalent shown); 2b2t guide overlays (world border ±30M, axis + diagonal highways; editable). **Out of v1**: waypoint editing, Drawing rendering, live players (seam only), clustering, mobile, JM/legacy import, in-browser WASM, tile-export UI (CLI covers), TLS.

### 5. Folder merger
Unit = (worldId, dim, mw) with `caves/<n>` sub-units. Modes: `merge A B -o OUT` (default, sources untouched) and `merge A --into B` (practical at 300 GB; A read-only; `--backup DIR`). **Every mutating command is a dry-run report unless `--apply`.** Pipeline:
1. Normalize/alias: exact worldId match; base-domain heuristic proposes `Multiplayer_2b2t` ⇄ `Multiplayer_2b2t.org` as a confirmation table (`--alias X=Y` non-interactive, `--yes` accepts); dims: `null` ⇄ `DIM0` automatic; namespaced dims exact; mw by id.
2. Plan/diff: per unit classify only-A / only-B / conflict; report counts, bytes, mtime-winner distribution (human table + `--json`).
3. Copy phase: non-conflicting = raw copy preserving mtime, `reflink-copy` CoW when supported, `--hardlink` opt-in; **major-6 files copied as-is** (mod upgrades on next save; only newer-than-mod versions are refused).
4. Conflict merge: decode both; per tile (64×16): present-in-one → take; both → whole tile from the **newer-mtime source** (`--prefer mtime|a|b`; no in-file timestamps exist; XaeroPlus foundTime rejected as tiebreaker — it's first-seen, not last-updated); re-encode 7/8 fresh palettes; `.zip.tmp-xt` → fsync → atomic rename; output mtime = max(A,B).
5. Aux: `dimension_config.txt` MWName lines unioned (conflicting scalar keys → newer root + warning); `server_config.txt` newer wins with diff warning; minimap waypoints parsed (right-split tail fields), whole-record dedupe, union, `sets:` preserved; XaeroPlus DBs → DB merger by filename.
6. Exclusions: dirs `^cache(_\d+)?$|^caches$`, files `*.outdated`, `*.temp`, `*.zip.tmp*`; after `--into`, delete stale cache dirs of merged dims (would show outdated imagery in-game).
7. Journal/resume: `OUT/.xaerotools-merge.journal` (JSONL, params-hash header, per-unit completion lines); `--resume` validates hash and skips done units; atomic renames guarantee only whole files exist.
8. Post-verify: counts == |A ∪ B|; decode-validate all re-encoded regions + 1% sample of copies; no temp leftovers.

### 6. SQLite merger (`xaero-db`, `xaerotools db-merge`)
1. Snapshot sources safely (direct ro open; if locked/WAL-live → SQLite backup API / `VACUUM INTO` scratch). Destination: copy (`-o`) or direct `--into` with `--backup`.
2. Detect schema: metadata version 2 | 1 | absent→v0 (tables `"0"/"-1"/"1"`); Drawing DBs by filename + shape.
3. Normalize destination (and snapshots) to v2, re-implementing `V0ToV1Migration` / `V1ToV2Migration` semantics (rename to resource-key tables or `INSERT OR IGNORE`+drop when target exists; rebuild as `WITHOUT ROWID` PK).
4. Merge: `ATTACH`; enumerate `sqlite_master` (never assume dimension names — modded dims are real tables); batched `INSERT ... SELECT ... ON CONFLICT(x,z) DO UPDATE SET foundTime = MIN(foundTime, excluded.foundTime)` → **oldest-foundTime wins** (preserves first-seen history) — but only where the column really is a time. `XaeroPlusLavaColumns.db` stores a lava-column *height* there (measured range 0..123 over 43.7M rows; `LavaColumns.java` passes `maxHeight`), so it merges with MAX; `xaero_db::highlight_semantics(db_name)` decides per DB. Finish: `wal_checkpoint(TRUNCATE)`, `PRAGMA optimize`, `--vacuum` opt-in.
5. Drawing DB: best-effort whole-row-dedupe union, flagged experimental (all sample drawing tables are empty; small data).
6. Validation: dry-run prints per-table A, B, overlap, predicted; post-merge asserts `result == A + B − overlap` and min-foundTime spot checks.

### 7. CLI surface
```
xaerotools                            # double-click: auto-detect roots, serve, open browser
xaerotools serve [--root PATH]... [--port 45746] [--lan --password PW] [--open]
                 [--cache-dir DIR] [--cache-cap 2GiB] [--pyramid-cap 4GiB] [--tile-format webp|png]
xaerotools scan  [--root PATH]... [--json]
xaerotools index --root R [--world W] [--pyramid] [--threads N]
xaerotools render --root R --world W --dim D [--mw mw$default] [--layer surface|cave:N]
                  [--bbox x1,z1,x2,z2 | --all] [--zoom 0..-8] -o DIR [--stitch out.png]
xaerotools merge  A B -o OUT | A --into B  [--alias X=Y]... [--yes] [--prefer mtime|a|b]
                  [--server NAME]... [--dim D]... [--backup DIR] [--hardlink] [--resume] [--apply] [--json]
xaerotools db-merge SRC.db... --into DEST.db | -o OUT.db [--apply] [--vacuum] [--json]
xaerotools waypoints --root R [--world W] [--json|--csv]
xaerotools colortable info
```
Default port 45746; localhost-only unless `--lan` (then password login page, rate-limited, folder browsing disabled, printed recommendation of Tailscale/SSH for remote). Config `~/.config/xaerotools/config.toml` (Windows `%APPDATA%\xaerotools\`), cache in platform cache dir. `indicatif` progress with regions/s + ETA. No telemetry; only outbound traffic ever = opt-in Atlas (browser-side) and dev-only colorgen jar download.

### 8. Live-share seam (deferred — design only)
(1) All overlays go through a server-side `LayerProvider` registry populating `GET /api/layers`; a future live-share module is just another provider fed by an authenticated WS ingest (`/ingest/v1`, token field reserved in config schema). (2) UI overlay list is data-driven from `/api/layers` with a reserved `stream` kind. (3) `docs/adr/007-live-share-seam.md` records topology (Meteor addon → self-hosted server → viewers), auth model, and why it's out of v1. Nothing else built.

## Milestones (≈ 9–10 focused weeks)

| # | Scope | Exit criteria | Effort |
|---|---|---|---|
| M0 | Workspace scaffold, CI (fmt/clippy/test + wasm32 check), fixture wiring, colorgen spike | CI green; spike renders a hardcoded region with placeholder colors | 0.5 wk |
| M1 | Codec (NBT, zip IO, decode 6/7, encode 7/8), waypoints/dimconfig parsers, round-trip harness, fuzz/truncation tests | All 1563 sample regions pass round-trip incl. **byte-identical v7 re-encode**; fuzzer clean 1 CPU-day | 1.5–2 wk |
| M2 | Full colorgen + XCT1; renderer + shading; scan/index; axum: native tiles, shallow zoom-outs, coverage tiles, LRU cache; minimal UI (pickers, pan/zoom, coords) | Browse own 2b2t overworld from cold start < 5 s to first tiles; render goldens committed | 2 wk |
| M3 | Full UI §4: waypoints + dim fix + search, 1:8 mode + converter, grids, measure, permalinks, cave dropdown; **highlight overlays**; persistent pyramid + SSE background build; dark polish | Feature checklist on sample data + user's real archive; pyramid of one big dim correct | 2 wk |
| M4 | db-merge (v0/v1/v2 normalize, oldest-wins, validation) then folder merge (plan/apply, aliasing, tile-granular conflicts, aux files, journal/resume, backups) | Fixture invariants pass; 300 GB soak with resume-after-SIGKILL; merged tree loads in-game (manual checklist) | 2 wk |
| M5 | Atlas POIs + WDL layers; `--lan` auth; packaging/distribution: release workflow, attestations, checksums, Scoop/winget/AUR/Homebrew, docs + screenshots | v1.0.0 installable via Scoop + AUR + direct download | 1–1.5 wk |
| M6 | Live-share seam ADR only | ADR merged; `/api/layers` shape frozen | 0.5 wk overlap |

De-risking order: codec correctness gates everything; render fidelity is the highest-uncertainty visual outcome so it precedes feature breadth; mergers come last so the codec has maximal test soak before the only data-touching component ships.

## Verification

- **Codec**: round-trip all 1563 sample regions (assert corpus split 1.21.4→6.8, 1.21.8→7.8): decode strict (no trailing bytes), decode→encode→decode semantic equality (palette-resolved, not index-based, for major-6 inputs), **encode(decode(x)) == x byte-for-byte for every major-7 input** (drop to semantic-only if writer nondeterminism is discovered — R3); truncate-at-every-offset on 3 regions never panics; cargo-fuzz in CI; topHeight-u8 synthetic; MUTF-8 edge cases.
- **Render goldens**: ~10 curated regions (spawn, ocean, DIM-1, DIM1, cave layer, namespaced dim) exact-match PNGs with `--bless` regeneration; perceptual ΔE report on colortable regeneration.
- **Merge fixture**: 2b2t 1.21.4-vs-1.21.8 trees → output counts null=377, DIM-1=1019, DIM1=4; conflicts decode as 7/8 with palette invariants; non-conflicts byte-identical to source; mtime rules hold; SIGKILL mid-run + `--resume` converges to identical output; in-game loadability via automated header checks + documented manual checklist (no `.backup` files appear = no version rejection).
- **DB merge**: per-table `result == A + B − overlap` on fixture DBs; synthesized v0/v1 DBs exercise normalization to v2 golden schemas; min-foundTime spot checks.
- **Performance smoke** (criterion, fixed CI runner): ≥ 25 regions/s/core decode+render (stretch 50), ≥ 5× rayon scaling on 8 cores, full 1563-region corpus < 90 s on 8 cores, 1M-file synthetic index scan < 30 s warm, first-tile latency < 300 ms cache-miss.
- **Server**: in-process axum integration tests (tile = valid 512² WebP, waypoint JSON schema, highlight tile correctness vs known DB, coverage math, permalink params). Windows CI job runs codec+scan over a fixture with `mw$default`/emoji names.
- **End-to-end with the real app**: `xaerotools serve --root "sample data"` → browse both sample worlds, toggle highlights/waypoints/1:8 mode; then point at the user's real 300 GB archive for the scale soak.

## Distribution & trust (the "no scary .exe" answer)

1. Open source (MIT) from day one; README "why trust this" section.
2. GitHub Actions release builds with pinned toolchain + **build provenance attestations** (`gh attestation verify`-able) + SHA-256SUMS signed with minisign.
3. Portable single static binary per OS (Windows x64, Linux x64/aarch64, macOS universal) — no installer, no admin, no services, localhost-only default, zero telemetry.
4. Package managers as the recommended channel: **Scoop** (ideal for unsigned portable Windows tools), winget, AUR, Homebrew tap, `cargo install` — each an independent integrity layer.
5. VirusTotal links in release notes; proactive false-positive disputes.
6. Roadmapped endgame: the **WASM zero-install browser version** (core kept compilable from M0) — File System Access API, nothing to run at all.
7. Code signing (Windows EV / Apple notarization) noted as cost-gated future options.
8. colortable.bin ships only derived per-block average colors (no Mojang assets reconstructible); anyone can regenerate it with `xaero-colorgen` from their own jar.

## Risks

| # | Risk | L/I | Mitigation |
|---|---|---|---|
| R1 | Color fidelity vs in-game look | H/M | Texture-avg + biome tint + heightShade = "recognizably identical" goal; goldens; M3 tuning vs screenshots; adjustable shading |
| R2 | Major-6 nuance beyond minor-gates | M/H | 1563-region strict corpus is the M1 tripwire; fallback: bytecode-diff region classes across mod jars (on hand); 6 is read-only |
| R3 | Byte-identical re-encode blocked by writer nondeterminism | M/L | Drop to semantic oracle; investigate once |
| R4 | Namespaced dim folders break assumptions | L/M | Dims opaque end-to-end; samples include them; `naming.rs` centralizes `$`/`%` rules with tests |
| R5 | Index memory/startup at 2–4M files | M/M | Lazy per-map, 16 B entries, persisted snapshots, filename-only scan; 1M-file CI benchmark |
| R6 | Windows quirks (`$`/`%`/emoji paths, >260 chars, rename locks) | H/M | longPathAware manifest, direct API paths (no shell), UTF-8, retry-then-stale; Windows CI job |
| R7 | Reads while game writes | M/M | Atomic-rename semantics, retry + stale fallback; ro+busy_timeout DB opens, never checkpoint live DBs; mergers snapshot via backup API and warn if game running |
| R8 | Drawing DB merge has no natural key | M/L | Best-effort dedupe union, experimental flag, tiny data |
| R9 | Atlas API changes/downtime | M/L | Opt-in, client-side, schema-tolerant, degrades gracefully |
| R10 | AV false positives regardless | M/M | Trust stack above; WASM endgame |
| R11 | Mod bumps format past 7.8 | certain/M | Clear "update XaeroTools" guard; passthrough model minimizes adoption work |
| R12 | Merged 7/8 opened by pre-v7 mod → rejected+backed up | L/M | Documented requirement (current mod builds cover all target MC versions); merge report notes when v6 inputs were re-encoded |
| R13 | Colortable gaps for 1.18–1.20 names | M/L | Aliases + suffix heuristics + magenta debug; corpus test asserts ≥ 99.9% of sample palette entries resolve |

## Key reference files

- `<workspace>/XaeroPlus-26.2/common/src/main/java/xaeroplus/feature/highlights/ChunkHighlightDatabase.java` — v2 DDL, pragmas, batching
- `.../feature/highlights/db/V0ToV1Migration.java`, `V1ToV2Migration.java` — migration semantics to re-implement
- `.../util/DataFolderResolveUtil.java` — worldId derivation/aliasing; `.../mixin/client/MixinMapSaveLoad.java` — save-path hook
- `<workspace>/xaeroworldmap-fabric-26.1.2-1.44.2.jar` — `xaero/map/file/MapSaveLoad.class`, `xaero/map/region/MapBlock.class`, `Overlay.class` (bit-packing ground truth for R2 fallback)
- `<workspace>/coordman-main/coordman.js` — proven Leaflet CRS.Simple setup, zoomOffset-3 Nether overlay, guides
- `<workspace>/sample data/` — round-trip corpus, merge fixture, v1/v0 DBs, waypoint/dimconfig fixtures
