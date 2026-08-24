# ADR 007 — Live-share seam (fully implemented)

Status: accepted; **positions + live map implemented** (2026-08:
`POST /ingest/v1/position` with per-player bearer tokens, `/ws/live` pushing
player positions and tile/DB invalidations, live markers + Players panel in
the viewer — contract in `docs/INGEST.md`). **Region upload implemented**
(2026-08: `POST /ingest/v1/region` — decode-validated uploads stored as a
verbatim per-player backup tree plus a tile-merged shared tree under the
server's ingest dir, both auto-served as roots; contract in
`docs/INGEST.md`). **Live preview and highlight sync implemented** (2026-08:
`POST /ingest/v1/preview` carries the terrain a client is seeing before Xaero
has written it to disk, `POST /ingest/v1/highlights` streams XaeroPlus finds
row by row into the server's own databases; contracts in `docs/INGEST.md`).

## Context

The long-term goal is group play: each player's client (via a Meteor addon)
pings its position — and optionally streams freshly-mapped region data — to a
self-hosted XaeroTools server; everyone connected sees live positions and a
live-updating shared map in the browser viewer. It must be easy to run
(one binary, one port) and safe to expose (password/token auth; plain HTTP
never exposed raw to the internet — documented VPN guidance).

## Decision

Keep v1 strictly local, but shape the surfaces so live-share drops in without
reworking anything:

1. **Server**: every overlay the viewer can draw is conceptually a *layer
   provider* (base tiles, highlight DBs, waypoints today). Live share becomes
   one more provider fed by an authenticated ingest endpoint, reserved as:
   - `POST /ingest/v1/position` — `{token, player, dim, x, y, z, yaw}`
   - `POST /ingest/v1/region` — raw `region.xaero` container upload for a
     (world, dim, mw, rx, rz); server validates by decoding before accepting,
     then merges tile-level (same code path as `xaero-merge`).
   - Auth: per-client bearer tokens in the server config; never query params.
2. **Viewer**: overlays are already data-driven per world (highlight DBs are
   enumerated, not hardcoded). A future `stream` layer kind (WebSocket
   `/ws/positions`) renders markers that update in place.
3. **Session auth** (implemented already for `--lan`): cookie session with
   password + rate limiting. Live-share reuses it for viewers; ingest uses
   separate per-player tokens so a leaked viewer password can't inject data.
4. **Transport security**: no TLS in-process for v1 of live-share either;
   documentation mandates VPN (Tailscale) or a reverse proxy with TLS for
   anything beyond a trusted LAN.

## Consequences

- No code changes needed in the tile/overlay pipeline when live-share lands;
  only new endpoints + one UI layer kind.
- Region ingest reuses the merge codec path, so a malicious or corrupt upload
  cannot produce files the game (or we) cannot re-read: uploads that fail
  decode are rejected outright.
