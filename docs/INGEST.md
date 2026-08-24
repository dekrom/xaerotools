# Ingest — the live-mode client contract

How a client (a Meteor/Fabric addon, a script, anything that can POST)
feeds live player positions and freshly-mapped region data into a running
XaeroTools server. This is the live-share seam designed in
`adr/007-live-share-seam.md`, both halves implemented; everything below is
derived from the code and is the contract to build against.

## Overview

`xaerotools serve` is live mode — there is no extra flag. One binary, one port
(default `45746` on `127.0.0.1`; if busy the server tries up to 20 candidate
ports from the base and prints the one it chose, so make the base URL
configurable). Clients
POST their position about once a second; the viewer shows a live marker per
player (with heading) and a Players panel, updating in place over the
`/ws/live` WebSocket. Positions live in memory only — nothing is persisted,
and a restart starts with an empty roster.

## Getting a token

**A client on the server's own machine needs no token.** Connections from
loopback (`127.0.0.1` / `::1`) may omit the `Authorization` header entirely
and just declare their player name — positions already carry it in the body,
and region uploads send it in an `X-XT-Player` header. Tokens gate *remote*
connections; a local process could read the config file the tokens live in
anyway, so demanding one of it adds setup without adding security. Two edges
to know: a token that *is* presented is always validated, even from loopback
(a revoked or mistyped token fails 401 instead of silently falling back), and
browser pages get no exemption (cross-origin requests are rejected, and both
routes need content types no hostile page can send without a CORS preflight).

Remote clients authenticate with per-player bearer tokens, generated on the
server box:

```bash
xaerotools tokens generate Account1     # prints the token — shown once
xaerotools tokens list                  # player, first 8 chars, age
xaerotools tokens revoke Account1
```

The viewer's **Share** panel does the same in the browser (`/api/tokens`:
GET lists player/prefix/age, POST `{player}` returns the token once, DELETE
`{player}` revokes). Like the roots and merge tools it is local-only —
disabled under `--lan`, so remote viewers can never mint credentials.

- One token per player: `generate` for an existing player **replaces** the old
  token, which stops working immediately. `revoke` removes it.
- Tokens are 64 lowercase hex chars (32 OS-random bytes).
- They live in the server config file:
  `~/.local/share/xaerotools/config.json` (platform equivalent — the same
  directory as the default vault; `--config PATH` overrides it on both `serve`
  and `tokens`). The file is forced to mode `0600` on every save and load.
- A **running server picks up generate/revoke within ~1 s** (it re-stats the
  config on ingest attempts, throttled to once per second). No restart needed.

Store the token in the client's own config file, never in shell arguments.

## POST /ingest/v1/position

### Request

```
POST /ingest/v1/position
Authorization: Bearer <token>
Content-Type: application/json
```

The token goes **only** in the `Authorization` header — never in query
parameters, and the server never logs it. Loopback clients may drop the
header; the body's `player` then names them directly (same safe-character
rule as region `world`, else 400). Body:

```json
{"player": "Account1", "dim": "minecraft:overworld",
 "x": 123.5, "y": 64.0, "z": -420.5, "yaw": 180.0}
```

| field    | type   | meaning |
|----------|--------|---------|
| `player` | string | account name; must equal the player the token was generated for, else 403 |
| `dim`    | string | dimension id — see below |
| `x` `y` `z` | number (f64) | player position in **block coordinates**, as doubles |
| `yaw`    | number (f32) | facing in **degrees, Minecraft convention**: 0 = south (+Z), 90 = west (−X); any finite value is accepted (no 0–360 wrapping required) |

All six fields are required; unknown extra fields are ignored. There is no
client timestamp — the server stamps arrival time.

Working example:

```bash
TOKEN=$(cat token.txt)   # keep tokens in a 0600 file, not in the command line
curl -sS -o /dev/null -w '%{http_code}\n' \
  -X POST http://127.0.0.1:45746/ingest/v1/position \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"player":"Account1","dim":"overworld","x":123.5,"y":64.0,"z":-420.5,"yaw":180.0}'
```

### `dim` values and normalization

Input is trimmed, then:

| sent | broadcast as |
|------|--------------|
| `overworld`, `minecraft:overworld` | `minecraft:overworld` |
| `nether`, `the_nether`, `minecraft:the_nether` | `minecraft:the_nether` |
| `end`, `the_end`, `minecraft:the_end` | `minecraft:the_end` |
| any other `namespace:path` | passed through unchanged |

Generic ids must have both parts non-empty, each ≤ 128 chars, using only
`a-z 0-9 _ - . /`. Anything else (no colon, uppercase, empty) → 400.

### Coordinate validity

Rejected with 400 unless all of: `x`, `y`, `z`, `yaw` finite (no NaN/Inf),
`|x| ≤ 40,000,000`, `|z| ≤ 40,000,000`, `-1024 ≤ y ≤ 4096`.

### Responses

| status | meaning |
|--------|---------|
| 204 No Content | accepted; broadcast to all `/ws/live` viewers |
| 401 | no token from a **remote** peer (`missing bearer token`), or a presented token not in the config (`unknown token`) — tokenless loopback requests pass |
| 403 | valid token, but body `player` doesn't match the token's player; or a tokenless request with a cross-origin `Origin` header |
| 400 | `unrecognized dim`, `coordinates out of range`, or a tokenless `player` name that fails the safe-character rule |
| 429 | per-player rate limit exceeded — slow down and keep going |

Unknown-token failures incur a server-side linear backoff **before** the 401
is sent: 100 ms × (consecutive failures + 1), capped at ~2.1 s. The counter is
server-global and resets on the next valid token, so don't hot-loop retries on
401 — fix the token. (A missing header 401 returns immediately.)

Malformed requests are rejected by the framework before auth runs: missing or
wrong `Content-Type` → 415, invalid JSON syntax → 400, valid JSON with a
missing field or wrong type → 422.

Anything under `/ingest` other than a POST to one of the four exact routes in
this document (`position`, `region`, `preview`, `highlights`) is 404.

### Rate limit

Token bucket per validated player: **5 requests/s sustained, burst 10**. A
denied request consumes nothing — the bucket refills continuously. **Post at
~1 Hz per account**; that is what the viewer is tuned for, and 4+ accounts at
1/s each pass with plenty of headroom (the limit is per player, not global).

## Security model

- **Ingest tokens and the viewer session are separate capabilities.** Ingest
  requests bypass the viewer's cookie session entirely (exact route only); a
  leaked viewer password cannot inject positions, and a leaked ingest token
  cannot view the map — it only authorizes posting that one player's position.
- **Loopback is trusted, the network is not.** The tokenless exemption keys
  off the TCP peer address (never a header, which anything can forge): only
  `127.0.0.1`/`::1` qualifies, so under `--lan` every other machine still
  needs a token. Cross-origin browser requests are rejected even from
  loopback, so a hostile web page can't ride the exemption.
- **Plain HTTP.** There is no in-process TLS; the token crosses the wire in
  cleartext. Default bind is `127.0.0.1`. `--lan` (which requires
  `--password`, protecting the *viewer*) binds `0.0.0.0`. Per ADR 007, beyond
  localhost or a trusted LAN you **must** put this behind a VPN (Tailscale) or
  a TLS reverse proxy. Never expose the raw port to the internet.
- Token comparison is constant-time-ish and failed lookups back off, but the
  real protections are the transport and keeping tokens out of argv, URLs and
  logs. Follow pos-sim's `--accounts-file` pattern: tokens read from a
  `0600` file, never passed as command-line arguments (argv is visible in
  `/proc/<pid>/cmdline` and shell history).

## The viewer side: `/ws/live` (informational)

You don't need this to send positions, but if you're building an alternative
viewer: `GET /ws/live` upgrades to a WebSocket that fans out every live event
as a JSON text frame. Browser clients must be same-origin (the `Origin` header
is checked against `Host`); non-browser clients that send no `Origin` pass.
Under `--lan` the upgrade requires the viewer session cookie. Messages sent
*to* the socket are ignored. `v` is a monotonic sequence number. A socket
that falls behind the broadcast channel is not dropped: the server skips the
missed events and sends a `resync` frame instead — refresh your tile layers
in place when you see one.

`hello` — sent once on connect, the current roster (each entry is a full
`pos` object):

```json
{"type":"hello","players":[{"type":"pos","player":"Account1","dim":"minecraft:overworld",
 "x":123.5,"y":64.0,"z":-420.5,"yaw":180.0,"t":1750000000000}],"v":42}
```

`pos` — one player moved; `t` is server receive time, unix ms:

```json
{"type":"pos","player":"Account1","dim":"minecraft:overworld",
 "x":124.1,"y":64.0,"z":-419.9,"yaw":175.5,"t":1750000001000}
```

`tiles` — map tiles changed on disk; `w`/`d`/`m` are positional indexes into
`/api/state`, `layer` is `"surface"` or `"cave-N"`, `regions` is a list of
`[rx, rz]` pairs or `null` meaning "assume all" (lists over 512 collapse to
null), `deep:true` means overzoom (z<0) tiles were invalidated too:

```json
{"type":"tiles","w":0,"d":0,"m":0,"layer":"surface","regions":[[12,-34]],"deep":false,"v":43}
```

`db` — a XaeroPlus highlight DB changed; re-fetch its `/hl` tiles:

```json
{"type":"db","w":0,"db":"XaeroPlusNewChunks.db","v":44}
```

`state` — the world list changed; re-fetch `/api/state` (positional indexes
may have moved):

```json
{"type":"state","v":45}
```

## Reference client: `scripts/pos-sim.py`

Simulates accounts doing a random walk and posting at a configurable rate —
the way to test the whole seam without the game. Stdlib only.

```bash
# one token per account (a running server picks them up, no restart):
xaerotools tokens generate Account1
xaerotools tokens generate Account2

# one NAME=TOKEN per line:
printf 'Account1=%s\nAccount2=%s\n' "$T1" "$T2" > accounts.txt && chmod 600 accounts.txt

scripts/pos-sim.py --url http://127.0.0.1:45746 --accounts-file accounts.txt \
    --rate 1.0 --center 0,0 --speed 4.0 --dim-hop
```

`--rate` is posts/sec per account (default 1.0), `--speed` blocks/sec,
`--dim-hop` makes accounts occasionally switch dimension. It prints per-account
sent/error counts every 10 s and never prints tokens.

## POST /ingest/v1/region

Uploads one region file — the raw bytes of a client's `<rx>_<rz>.zip` or
`.xaero` exactly as Xaero's World Map saved it. The server rejects anything
that does not fully decode, then stores it twice under its ingest dir
(default `~/.local/share/xaerotools/ingest/`, `--ingest-dir` overrides):

- `players/<player>/world-map/<world>/…` — the uploaded bytes **verbatim**: a
  per-client backup of exactly what that account's game has mapped.
- `merged/world-map/<world>/…` — tile-merged across every uploader: tiles the
  upload carries win (they are the newest observation), tiles it lacks
  survive from what was already merged. Re-encoded as 7.8, self-checked by
  decoding before the atomic rename.

Both trees are ordinary Xaero layouts and are served automatically as roots
(origin `ingest` in `/api/roots`) — no restart, no manual root adding. The
first upload of a new world/dim/multiworld triggers a rescan; after that the
live watcher invalidates tiles per upload, so viewers see the map grow in
place. The ingest dir is the only place the server ever writes region data;
scanned roots stay read-only.

### Request

```
POST /ingest/v1/region?world=Multiplayer_2b2t&dim=null&mw=mw$default&rx=12&rz=-34
Authorization: Bearer <token>        (remote; loopback may send X-XT-Player: <player> instead)
Content-Type: application/octet-stream
```

A tokenless loopback upload identifies its player with the `X-XT-Player`
header (same safe-character rule as `world`). When a valid token is presented
the header is ignored — the token names the player. Tokenless with neither is
401.

Query parameters (all path segments exactly as they are named on the client's
disk):

| param  | meaning |
|--------|---------|
| `world` | world folder name, e.g. `Multiplayer_2b2t` (safe chars only: alphanumerics, space, `_-.$%()',+&@~`; no leading dot, max 128) |
| `dim`  | world-map dimension folder: `null`, `DIM0`, `DIM-1`, `DIM1`, or an escaped custom id (`minecraft$worlds%2b2t%2b2t_1`) |
| `mw`   | multiworld folder: `mw$default`, `mw$-542221765`, `cm$converted`, legacy `mw<x>,<y>,<z>` |
| `rx` `rz` | region coordinates from the filename; abs value ≤ 100,000 |
| `cave` | optional cave layer number for a `caves/<n>/` region. A server started with `--ingest-no-caves` refuses these with 403 — surface uploads only |

Body: the region file bytes, at most 32 MiB. Remember to URL-encode `$` as
`%24` (curl passes it through fine either way, but be strict in clients).

Working example — upload one region:

```bash
TOKEN=$(cat token.txt)
curl -sS -o /dev/null -w '%{http_code}\n' \
  -X POST 'http://127.0.0.1:45746/ingest/v1/region?world=Multiplayer_2b2t&dim=null&mw=mw%24default&rx=0&rz=0' \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/octet-stream' \
  --data-binary @'xaero/world-map/Multiplayer_2b2t/null/mw$default/0_0.zip'
```

### Responses

| status | meaning |
|--------|---------|
| 204 No Content | validated, backed up, merged |
| 401 / 403 | as for position (same tokens, same backoff); 403 also covers a token whose player name is not filesystem-safe, and any `cave=N` upload when the server runs `--ingest-no-caves` — drop the region, do not retry |
| 400 | bad `world`/`dim`/`mw` name, coordinates out of range, or a body that does not decode as a region — **including a truncated one**: the client caught the game mid-write and should retry after the file settles |
| 413 | body over 32 MiB |
| 429 | rate limited — slow down and keep going |

### Rate limit and client behaviour

Token bucket per player: **10 uploads/s sustained, burst 20** (separate from
the position bucket). Clients should:

- upload a region only after its mtime has settled for a few seconds (Xaero
  writes temp + rename, but a slow save can still be caught mid-flight —
  the truncated-region 400 exists exactly for that; re-read and retry);
- never upload names that are not live regions: skip `cache*/` dirs,
  `<version>_backup_<n>/` dirs, `*.temp`, `*.outdated`, `*.sync-conflict-*`
  — only `<rx>_<rz>.zip|.xaero` directly in a layer dir;
- throttle a full-map sync to a few uploads per second and honour 429 with
  backoff. A full sync is only needed once per server; after that the
  incremental watch keeps the remote map current.

The security model above applies unchanged: same bearer tokens, plain HTTP,
LAN/VPN only — for the homelab-across-the-internet case (a friend feeding
your server), put the port behind a VPN (Tailscale) or a TLS reverse proxy
and never expose it raw.

## POST /ingest/v1/preview

Streams a coarse live preview of terrain the client is *seeing right now* —
before Xaero has saved anything to disk. Xaero holds a freshly-mapped region
dirty in memory for up to 60 s (`SAVE_TIME`), so region uploads are
authoritative but never instant; this channel is what makes the shared map
move with the player. The server keeps the chunks in a bounded in-memory
canvas per dimension (nothing is written to disk; a restart starts blank),
serves them as the `/preview/{dim}/{z}/{x}/{y}` tile layer, and drops every
chunk that a later *region* upload covers — the real imagery replaces the
preview automatically.

### Request

```
POST /ingest/v1/preview?dim=overworld
Authorization: Bearer <token>        (remote; loopback may send X-XT-Player: <player> instead)
Content-Type: application/octet-stream
```

`dim` takes the same values as position ingest and is normalized the same
way. Auth is identical to the other ingest routes.

Body (all integers little-endian):

```
"XTPV"  u8 version = 1  u16 count            (1 ≤ count ≤ 256)
then count × {
  i32 cx  i32 cz                             (chunk coordinates)
  512 bytes: 16×16 RGB565 pixels, row-major  (index = z*16 + x)
}
```

Pixel value `0x0000` means "nothing visible in this column" and renders
transparent — clients that need to send true black should nudge it to any
non-zero value (e.g. `0x0841`). Chunks that are entirely empty should not be
sent at all.

### Responses

204 accepted (a `preview` event is broadcast); 400 for a malformed batch,
bad count, bad `dim` or out-of-range chunk coordinates; 401/403/429 as for
position ingest (rate: **4 batches/s sustained, burst 8** per player — batch
up to 256 chunks per POST instead of posting often).

### Client behaviour

- Re-send a chunk only when its pixels changed; treat a non-204 as "not
  sent" and let the next sweep retry it.
- The reference implementation is the companion addon's `PreviewScanner`:
  sweeps a radius around the player each second with a per-tick budget,
  hashes each chunk's pixels, batches changes, and commits its sent-state
  only on 204.

### The `preview` and `resync` events

`preview` — the canvas changed inside these regions (also emitted when a
region upload evicts preview chunks). `dim` is the normalized resource key;
viewers re-fetch the `/preview` tiles intersecting `regions`:

```json
{"type":"preview","dim":"minecraft:overworld","regions":[[12,-34]],"v":46}
```

`resync` — this socket lagged and missed broadcasts; refresh all tile layers
in place (do **not** clear them — re-fetch and swap):

```json
{"type":"resync"}
```

## POST /ingest/v1/highlights

Shares the chunks XaeroPlus finds — new chunks by either detection and their
inverses, old/modern chunks, portals, old biomes, breadcrumb trails — with a
server that keeps its **own** database of them. The server merges the rows
into `merged/world-map/<world>/XaeroPlus<Kind>.db` under its ingest dir, which
is already served as a map root, so they appear as the ordinary highlight
overlay for that world. The file stays a valid XaeroPlus v2 database and can
be copied straight back into a game instance.

Only rows travel, never the database. A real one runs to gigabytes and carries
no index on `foundTime`, so neither uploading nor rescanning it is affordable;
a client streams what it has found since its last accepted batch instead.

**Remote servers only.** A server on the same machine as the game already
reads those databases through a scanned root, and a second copy of the same
data would diverge from the first. Uploads from a loopback peer are refused
with 403, and a client should not offer the feature when its server URL is
local.

### Request

```
POST /ingest/v1/highlights?world=Multiplayer_2b2t&db=XaeroPlusNewChunks.db&dim=minecraft:overworld
Authorization: Bearer <token>
Content-Type: application/octet-stream
```

- `world` — the world folder the rows belong to, as in region ingest.
- `db` — the database file name, one of the nine timestamp-valued highlight
  databases: `XaeroPlusNewChunks.db`, `XaeroPlusNewChunksLiquidInverse.db`,
  `XaeroPlusPaletteNewChunks.db`, `XaeroPlusPaletteNewChunksInverse.db`,
  `XaeroPlusOldChunks.db`, `XaeroPlusModernChunks.db`, `XaeroPlusPortals.db`,
  `XaeroPlusOldBiomes.db`, `XaeroPlusBreadcrumbs.db`.

  `XaeroPlusLavaColumns.db` is **not** syncable and will be refused. Its value
  column is a lava-column height, not a first-sighting time, so the watermark
  every client pages by would order it by lava depth and drop nearly every
  row. Size is not the criterion — only rows found since the client's last
  sweep travel, so a database that is gigabytes on disk still streams a
  handful of rows a minute.
- `dim` — the dimension resource key (`minecraft:the_nether`), which is also
  the table name in the v2 schema.

Body (all integers little-endian):

```
"XTHL"  u8 version = 1  u16 count            (count ≤ 4096)
then count × {
  i32 x  i32 z                               (CHUNK coordinates)
  i64 foundTime                              (epoch ms of first sighting)
}
```

### Responses

204 accepted; 400 for a malformed batch, an unsyncable `db`, a bad `dim` key
or out-of-range chunk coordinates; 403 when the peer is loopback (see above);
401 when the token is missing or unknown, as for position ingest; 429 over the
rate limit (**4 batches/s sustained, burst 12** per player — a batch holds 4096
rows). The burst clears a full sweep of all nine databases, so a client may
send them back to back.

Rows merge by first sighting: the **oldest** `foundTime` for a chunk wins, so
re-sending a row the server already has is harmless and changes nothing.

### Client behaviour

- Keep a watermark per (world, db, dimension) and send only rows above it.
  Start it at the current time the first time a database is seen — walking the
  history is what this endpoint exists to avoid.
- Advance the watermark only on 204, so a failed batch retries.
- Stop for the session on 403 or 404: the first means the server is not remote,
  the second that it predates this endpoint. Neither improves on retry. A 401
  is the token, not the endpoint — stop until the token setting changes, and
  don't hot-loop it, because the backoff it triggers is server-global.
- Read each module's cache *field* rather than its public
  `getHighlightsState`: modules that own both a detection and its inverse
  (`LiquidNewChunks`, `PaletteNewChunks`, `OldChunks`) resolve that method
  through the user's render toggle and would hand back whichever of the two is
  currently being drawn.
- A module the user has disabled has an empty cache, so nothing is sent for
  it. Syncing the whole list needs no per-module setting.
- The reference implementation is the companion addon's `HighlightSync`, which
  reads XaeroPlus's in-memory find cache on the client tick rather than its
  database.
