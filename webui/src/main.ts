import L from 'leaflet';
import 'leaflet/dist/leaflet.css';
import './style.css';
import {
  vaultSync,
  fetchState,
  fetchWaypoints,
  fetchJob,
  toolsMerge,
  toolsDbMerge,
  fetchRoots,
  addRoot,
  removeRoot,
  fetchTokens,
  generateToken,
  revokeToken,
  removePlayer,
  fsList,
  fetchAtlasIndex,
  fetchAtlasStore,
  putAtlasStore,
  AtlasIndexJson,
  AtlasLocation,
  AtlasSetJson,
  DbMergeReport,
  DbEvent,
  HlPaletteJson,
  LiveEvent,
  MergeReport,
  PosEvent,
  PreviewEvent,
  RootJson,
  StateJson,
  TokenJson,
  TilesEvent,
  WaypointFileJson,
  WaypointJson,
  WorldJson,
} from './api';

const TRANSPARENT_TILE =
  'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==';

// block coords <-> leaflet
const toLatLng = (x: number, z: number): L.LatLngExpression => [-z, x];
const fromLatLng = (ll: L.LatLng) => ({ x: ll.lng, z: -ll.lat });

interface Selection {
  w: number;
  d: number;
  m: number;
  layer: string; // "surface" | "cave-N"
}

let state: StateJson;
let sel: Selection = { w: 0, d: 0, m: 0, layer: 'surface' };
let map: L.Map;
let baseLayer: L.TileLayer | null = null;
let waypointLayer = L.layerGroup();
let guideLayer = L.layerGroup();
let gridLayer: L.GridLayer | null = null;
let waypointFiles: WaypointFileJson[] = [];
let wpFilter = '';
/** Bumped per reloadWaypoints call so a slow response for one world cannot
 *  land after a switch and paint that world's waypoints on another. */
let wpReloadGen = 0;
/** Active overlay layers, keyed `${worldIndex}|${db}`.
 *
 * One ticked overlay can be two layers: the selected world's own copy of the
 * database, and the merged ingest tree's, which is where companion clients
 * stream their finds. Those are different world entries, so an overlay drawn
 * only from the selection shows a frozen archive copy while the live rows
 * pile up in a world nobody is looking at. */
let hlLayers = new Map<string, L.TileLayer>();
let hlEnabled = new Set<string>(); // persists across dim/world switches
let netherUnderlay: L.TileLayer | null = null;
let measure: { points: { x: number; z: number }[]; line: L.Polyline | null } | null = null;
/** While a popup is open, marker rebuilds are deferred: clearing a marker's
 *  layer closes its popup, and popup auto-pan itself fires the very moveend
 *  that triggers the rebuild — opening one would close it instantly. */
let popupOpen = false;
/** Waypoint picked in the sidebar whose popup should open once its marker
 *  exists (the setView pan rebuilds markers before the popup can show). */
let pendingWpPopup: string | null = null;
const wpKey = (wp: WaypointJson) => `${wp.x},${wp.z},${wp.name}`;
let atlasLayer = L.layerGroup();
let atlasData: AtlasLocation[] | null = null;
let atlasFilter = '';
let atlasUnderlay: L.GridLayer | null = null;
/** `set.url` the underlay is currently drawn from, '' when it is off. */
let atlasUnderlaySet = '';
/** When the POI snapshot we are showing was downloaded (unix ms, 0 = never). */
let atlasFetchedMs = 0;
/** Presence indexes per mirrored set; null once the server answered "no index". */
let atlasIndexes = new Map<string, AtlasIndex | null>();
/** Fallback for servers without an index: does the pyramid have any tiles? */
let atlasProbed = new Map<string, boolean>();
/** Atlas tiles the local mirror 404'd, keyed `set|z/iy/ix` — never asked twice. */
let atlasMissing = new Set<string>();

const ATLAS_URL = 'https://api.blackportal.cloud/api/locations';

// The whole-map datasets scripts/atlas-mirror.py knows how to download.
const ATLAS_DATASETS: [string, string, string][] = [
  ['overworld', 'Overworld', 'Overworld/256k/day'],
  ['the_nether', 'Nether', 'Nether/43k'],
  ['the_end', 'End', 'End/42k'],
];
const ATLAS_DEFAULT_DIR = '~/.local/share/xaerotools/atlas';

/** The overlay's palette entry, as the server reports it. Nothing here is
 *  hardcoded any more: the server paints the tiles, so a local copy of the
 *  colours could only ever drift from what actually lands on the map. */
function hlInfo(db: string): HlPaletteJson | null {
  for (const i of state?.hlPalette ?? []) if (db.includes(i.pattern)) return i;
  return null;
}

/** The colour this overlay is drawn in: the user's override, else the module's
 *  default. Always `#rrggbb`, which is what both /hl and <input type=color>
 *  want. */
function hlColor(db: string): string {
  return hlOverrides.get(db) ?? hlInfo(db)?.color ?? '#aaaaaa';
}

/** 0..1. Applied as layer opacity, so changing it never refetches a tile. */
function hlOpacity(db: string): number {
  return hlOpacities.get(db) ?? HL_OPACITY_DEFAULT;
}

function hlLabel(db: string): string {
  return db.replace(/^XaeroPlus/, '').replace(/\.db$/, '');
}

/** The name a DB travels under in the hash: the short label for the mod's
 *  own `XaeroPlus*.db` files, the full name for anything else, so readHash
 *  (which re-adds the prefix only to names without `.db`) round-trips both. */
function hlHashName(db: string): string {
  return /^XaeroPlus.*\.db$/.test(db) ? hlLabel(db) : db;
}

const HL_OPACITY_DEFAULT = 0.85;
/** Per-DB colour overrides, `#rrggbb`. Persisted; a shared link carries them. */
const hlOverrides = new Map<string, string>();
const hlOpacities = new Map<string, number>();

/** Stored preferences, entry by entry: a corrupt or hand-edited value is
 *  dropped rather than allowed to take the overlay panel down with it. */
function readHlStore(key: string): [string, unknown][] {
  try {
    const raw = localStorage.getItem(key);
    return raw ? Object.entries(JSON.parse(raw) as Record<string, unknown>) : [];
  } catch {
    return []; // storage disabled, or the entry is not JSON: use the defaults
  }
}

function loadHlPrefs() {
  for (const [db, v] of readHlStore('xt-hl-colors')) {
    if (typeof v === 'string' && /^#[0-9a-f]{6}$/i.test(v)) hlOverrides.set(db, v.toLowerCase());
  }
  for (const [db, v] of readHlStore('xt-hl-opacity')) {
    if (typeof v === 'number' && v >= 0 && v <= 1) hlOpacities.set(db, v);
  }
}

function saveHlPrefs() {
  try {
    localStorage.setItem('xt-hl-colors', JSON.stringify(Object.fromEntries(hlOverrides)));
    localStorage.setItem('xt-hl-opacity', JSON.stringify(Object.fromEntries(hlOpacities)));
  } catch {
    // Private mode / storage full: the session still works, it just forgets.
  }
}

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

function currentWorld() {
  return state.worlds[sel.w];
}
function currentDim() {
  return currentWorld()?.dims[sel.d];
}

/** The see-through-roof query every map-tile layer carries, '' when off.
 *  It is part of the tile's identity server-side, so both views cache and
 *  refresh independently. */
function roofQuery(): string {
  const cb = $('toggle-roof') as HTMLInputElement | null;
  if (!cb?.checked) return '';
  return `?roof=${roofAlpha('roof-obsidian', 95)},${roofAlpha('roof-snow', 10)}`;
}

/** One roof opacity input, clamped to the byte the server accepts. */
function roofAlpha(id: string, fallback: number): number {
  const v = Number(($(id) as HTMLInputElement | null)?.value);
  if (!Number.isFinite(v)) return fallback;
  return Math.max(0, Math.min(255, Math.round(v)));
}

function tileUrl(): string {
  return `./tiles/${sel.w}/${sel.d}/${sel.m}/${sel.layer}/{z}/{x}/{y}${roofQuery()}`;
}

/** Tells same-named worlds from different roots apart: the ingest trees say
 *  what they are, anything else falls back to its root folder's name. */
function worldLabel(w: WorldJson): string {
  if (w.origin === 'ingestMerged') return `${w.id} · live uploads (merged)`;
  if (w.origin === 'ingestPlayer') return `${w.id} · live backup: ${w.player ?? '?'}`;
  if (state.worlds.some((o) => o !== w && o.id === w.id)) {
    const base = w.root.split('/').filter(Boolean).pop() ?? w.root;
    return `${w.id} · ${base}`;
  }
  return w.id;
}

// ------------------------------------------------------------------- hash --

function readHash(): { sel: Selection; x: number; z: number; zoom: number } | null {
  const m = location.hash.match(
    /^#\/(\d+)\/(\d+)\/(\d+)\/([a-z0-9-]+)\/(-?\d+)\/(-?\d+)\/(-?\d+)(\?(.*))?$/
  );
  if (!m) return null;
  const params = new URLSearchParams(m[9] ?? '');
  for (const name of (params.get('hl') ?? '').split(',')) {
    if (name) hlEnabled.add(name.endsWith('.db') ? name : `XaeroPlus${name}.db`);
  }
  // Colours travel with the link, so a second screen shows the same map. The
  // opacity does not: that is a per-display comfort setting, not identity.
  for (const pair of (params.get('hlc') ?? '').split(',')) {
    const [name, hex] = pair.split(':');
    if (!name || !/^[0-9a-f]{6}$/i.test(hex ?? '')) continue;
    hlOverrides.set(name.endsWith('.db') ? name : `XaeroPlus${name}.db`, `#${hex.toLowerCase()}`);
  }
  const roof = params.get('roof');
  if (roof) {
    const [o, n] = roof.split(',');
    ($('toggle-roof') as HTMLInputElement).checked = true;
    if (o) ($('roof-obsidian') as HTMLInputElement).value = o;
    if (n) ($('roof-snow') as HTMLInputElement).value = n;
  }
  if (params.get('nether') === '1') ($('toggle-nether') as HTMLInputElement).checked = true;
  if (params.get('atlas') === '1') ($('toggle-atlas') as HTMLInputElement).checked = true;
  if (params.get('au') === '1') ($('toggle-atlas-under') as HTMLInputElement).checked = true;
  // A shared link can open straight into follow mode on another screen/PC.
  followName = params.get('follow');
  return {
    sel: { w: +m[1], d: +m[2], m: +m[3], layer: m[4] },
    x: +m[5],
    z: +m[6],
    zoom: +m[7],
  };
}

let hashTimer: number | null = null;

/** Debounced: a wheel step fires zoomend and moveend back to back, and Safari
 *  throws past ~100 replaceState calls per 30 s — which, thrown from inside
 *  Leaflet's event dispatch, would abort the tile layers' own moveend work. */
function writeHash() {
  if (hashTimer !== null) clearTimeout(hashTimer);
  hashTimer = window.setTimeout(writeHashNow, 250);
}

function writeHashNow() {
  hashTimer = null;
  const c = fromLatLng(map.getCenter());
  let h = `#/${sel.w}/${sel.d}/${sel.m}/${sel.layer}/${Math.round(c.x)}/${Math.round(
    c.z
  )}/${map.getZoom()}`;
  const params: string[] = [];
  if (hlEnabled.size > 0) params.push(`hl=${[...hlEnabled].map(hlHashName).join(',')}`);
  if (hlOverrides.size > 0) {
    const pairs = [...hlOverrides].map(([db, c]) => `${hlHashName(db)}:${c.slice(1)}`);
    params.push(`hlc=${pairs.join(',')}`);
  }
  const roofQ = roofQuery();
  if (roofQ) params.push(roofQ.slice(1));
  if (($('toggle-nether') as HTMLInputElement)?.checked) params.push('nether=1');
  if (($('toggle-atlas') as HTMLInputElement)?.checked) params.push('atlas=1');
  if (($('toggle-atlas-under') as HTMLInputElement)?.checked) params.push('au=1');
  if (followName) params.push(`follow=${encodeURIComponent(followName)}`);
  if (params.length) h += `?${params.join('&')}`;
  try {
    history.replaceState(null, '', h);
  } catch {
    /* Safari's replaceState rate limit: the next move rewrites it */
  }
}

// -------------------------------------------------------------------- map --

function setupMap() {
  map = L.map('map', {
    crs: L.CRS.Simple,
    minZoom: -16,
    maxZoom: 3,
    zoomControl: false,
    attributionControl: false,
    preferCanvas: false,
    // Off, or every live region update flashes: refreshLayerTiles() swaps a
    // loaded tile by re-assigning its src, which re-fires the load listener
    // Leaflet bound in createTile, and its fade-in restarts from opacity 0.
    // The map is blocky pixel art — the ramp buys nothing on pan/zoom either.
    fadeAnimation: false,
  });
  // Top-left belongs to the toolbar; keep zoom out of the way of panels.
  L.control.zoom({ position: 'bottomright' }).addTo(map);
  map.setView(toLatLng(0, 0), -2);
  waypointLayer.addTo(map);
  guideLayer.addTo(map);
  atlasLayer.addTo(map);
  liveLayer.addTo(map);
  map.on('mousemove', (e: L.LeafletMouseEvent) => {
    const { x, z } = fromLatLng(e.latlng);
    const dimType = currentDim()?.dimType;
    let extra = '';
    if (dimType === 'overworld') {
      extra = `  (nether: ${Math.floor(x / 8)}, ${Math.floor(z / 8)})`;
    } else if (dimType === 'the_nether') {
      extra = `  (overworld: ${Math.floor(x * 8)}, ${Math.floor(z * 8)})`;
    }
    $('coords').textContent = `X: ${Math.floor(x)} Z: ${Math.floor(z)}${extra}`;
  });
  map.on('click', (e: L.LeafletMouseEvent) => {
    if (measure) measureAddPoint(fromLatLng(e.latlng));
  });
  map.on('moveend zoomend', () => {
    writeHash();
    redrawGuides();
    if (!popupOpen) {
      redrawWaypoints();
      redrawAtlas();
    }
  });
  map.on('popupopen', () => {
    popupOpen = true;
    pendingWpPopup = null;
  });
  map.on('popupclose', () => {
    popupOpen = false;
    // Deferred so that switching straight to another marker's popup (close A,
    // open B) doesn't rebuild B's marker out from under the opening popup.
    setTimeout(() => {
      if (!popupOpen) {
        redrawWaypoints();
        redrawAtlas();
      }
    }, 0);
  });
  // Grabbing the map is the "stop following" gesture (panTo never fires this).
  map.on('dragstart', () => {
    if (followName) setFollow(null);
  });
}

// ------------------------------------------------------- nether 1:8 underlay

let netherUnderlayUrl = '';

/** Index of the Nether multiworld that pairs with the selected Overworld one:
 *  matched by id, since the two dimensions' multiworld lists are independent
 *  and the same index can name different worlds. */
function netherMwIdx(netherIdx: number): number {
  const w = currentWorld();
  if (!w || netherIdx < 0) return 0;
  const wantId = currentDim()?.mws[sel.m]?.id;
  const i = w.dims[netherIdx].mws.findIndex((m) => m.id === wantId);
  return i >= 0 ? i : 0;
}

function updateNetherToggle() {
  const row = $('row-nether');
  const cb = $('toggle-nether') as HTMLInputElement;
  const isOverworld = currentDim()?.dimType === 'overworld';
  const netherIdx = currentWorld()?.dims.findIndex((d) => d.dimType === 'the_nether') ?? -1;
  row.hidden = !isOverworld || netherIdx < 0;
  const mwIdx = netherMwIdx(netherIdx);
  const wantUrl =
    !row.hidden && cb.checked
      ? `./tiles/${sel.w}/${netherIdx}/${mwIdx}/surface/{z}/{x}/{y}${roofQuery()}`
      : '';
  // Same source = keep the layer; rebuilding blanks it until tiles reload.
  if (wantUrl === netherUnderlayUrl) return;
  netherUnderlayUrl = wantUrl;
  if (netherUnderlay) {
    map.removeLayer(netherUnderlay);
    netherUnderlay = null;
  }
  if (wantUrl) {
    // zoomOffset 3: one nether block spans 8 overworld blocks, so a nether
    // tile requested 3 zoom levels "later" lands exactly on 8x its area.
    netherUnderlay = L.tileLayer(
      wantUrl,
      {
        tileSize: 512,
        minZoom: -16,
        maxZoom: 3,
        zoomOffset: 3,
        maxNativeZoom: -3,
        minNativeZoom: -16,
        noWrap: true,
        errorTileUrl: TRANSPARENT_TILE,
        className: 'pixelated',
        opacity: 0.75,
        zIndex: 1,
      }
    );
    netherUnderlay.addTo(map);
    baseLayer?.setZIndex(2);
  }
}

// ----------------------------------------------------------------- measure --

function measureAddPoint(p: { x: number; z: number }) {
  if (!measure) return;
  measure.points.push(p);
  if (measure.line) map.removeLayer(measure.line);
  measure.line = L.polyline(
    measure.points.map((q) => toLatLng(q.x, q.z)),
    { color: '#ffd35c', weight: 2, dashArray: '6 4' }
  ).addTo(map);
  let total = 0;
  for (let i = 1; i < measure.points.length; i++) {
    const a = measure.points[i - 1];
    const b = measure.points[i];
    total += Math.hypot(b.x - a.x, b.z - a.z);
  }
  const dimType = currentDim()?.dimType;
  const extra = dimType === 'overworld' ? ` (${Math.round(total / 8)} nether blocks)` : '';
  $('coords').textContent = `measured: ${Math.round(total)} blocks${extra} — click to extend, untick to clear`;
}

function toggleMeasure() {
  const on = ($('toggle-measure') as HTMLInputElement).checked;
  if (measure?.line) map.removeLayer(measure.line);
  measure = on ? { points: [], line: null } : null;
  map.getContainer().style.cursor = on ? 'crosshair' : '';
}

// -------------------------------------------------- atlas underlay (local) --

/** A decoded `/api/atlas/index` payload: which tiles the mirror actually has. */
interface AtlasIndex extends AtlasIndexJson {
  bytes: Uint8Array;
  /** Bit offset of each level's grid within `bytes`. */
  offsets: number[];
}

/** Fetches (once per set) the mirror's presence bits, if the server has them.
 *  Without them, one probe still tells us whether the mirror has any tiles at
 *  all — an empty dataset must not be offered as a layer. */
async function ensureAtlasIndex(set: AtlasSetJson): Promise<void> {
  if (atlasIndexes.has(set.url)) return;
  atlasIndexes.set(set.url, null); // claim the slot: one request per set, ever
  const json = await fetchAtlasIndex(set.url);
  if (json) {
    const bin = atob(json.bits);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    const offsets: number[] = [];
    let bit = 0;
    for (const side of json.sides) {
      offsets.push(bit);
      bit += side * side;
    }
    atlasIndexes.set(set.url, { ...json, bytes, offsets });
  } else {
    try {
      const probe = await fetch(`./atlas/${set.url}/${set.zMin}/0/0.png`);
      atlasProbed.set(set.url, probe.ok);
    } catch {
      atlasProbed.set(set.url, false);
    }
  }
  // Re-run the toggle now that we know whether this mirror is worth showing.
  updateAtlasUnderlay();
}

/** False only when we know the tile is absent — an unknown mirror is optimistic. */
function atlasHas(set: AtlasSetJson, z: number, iy: number, ix: number): boolean {
  if (atlasMissing.has(`${set.url}|${z}/${iy}/${ix}`)) return false;
  const idx = atlasIndexes.get(set.url);
  if (!idx || z < idx.zMin || z > idx.zMax) return true;
  const side = idx.sides[z - idx.zMin];
  if (ix >= side || iy >= side) return false; // outside the pyramid entirely
  const bit = idx.offsets[z - idx.zMin] + iy * side + ix;
  return (idx.bytes[bit >> 3] & (0x80 >> (bit & 7))) !== 0;
}

// Draws locally mirrored Atlas WDL pyramids (vips "google" layout). The
// pyramid grid is anchored at the dataset's own origin, which need not align
// with our region grid, so each canvas tile composites the (up to four) atlas
// tiles that overlap it at the closest available pyramid level. Tiles the
// mirror lacks are remembered and redrawn from the next level up, so a partial
// mirror loses resolution instead of going blank.
const AtlasUnderlayGrid = L.GridLayer.extend({
  createTile(this: any, coords: L.Coords, done: (err: unknown, tile: HTMLElement) => void) {
    const set: AtlasSetJson = this.options.set;
    const size = this.getTileSize().x;
    const tile = document.createElement('canvas');
    tile.width = size;
    tile.height = size;
    const ctx = tile.getContext('2d')!;
    const S = size * Math.pow(2, -coords.z); // blocks per canvas tile
    const azIdeal = set.zMax - Math.round(Math.log2(S / set.bptMax));
    const az = Math.max(set.zMin, Math.min(set.zMax, azIdeal));
    const bpt = set.bptMax * Math.pow(2, set.zMax - az); // blocks per atlas tile
    const X0 = coords.x * S;
    const Z0 = coords.y * S;
    const pxPerBlock = size / S;
    let pending = 0;
    let queued = false;
    const finish = () => {
      if (queued && pending === 0) done(null, tile);
    };
    // Paints the world rect [wx,wx+ww) x [wz,wz+wh) from level z, walking one
    // level up the pyramid whenever the mirror is missing that tile.
    const drawCell = (z: number, wx: number, wz: number, ww: number, wh: number) => {
      const b = set.bptMax * Math.pow(2, set.zMax - z);
      const ix = Math.floor((wx - set.originX) / b);
      const iy = Math.floor((wz - set.originZ) / b);
      if (z < set.zMin || ix < 0 || iy < 0) {
        pending--;
        finish();
        return;
      }
      if (!atlasHas(set, z, iy, ix)) {
        drawCell(z - 1, wx, wz, ww, wh);
        return;
      }
      const img = new Image();
      img.onload = () => {
        const sx = ((wx - (set.originX + ix * b)) / b) * img.width;
        const sy = ((wz - (set.originZ + iy * b)) / b) * img.height;
        const sw = (ww / b) * img.width;
        const sh = (wh / b) * img.height;
        const dw = ww * pxPerBlock;
        const dh = wh * pxPerBlock;
        ctx.imageSmoothingEnabled = dw < sw;
        ctx.drawImage(img, sx, sy, sw, sh, (wx - X0) * pxPerBlock, (wz - Z0) * pxPerBlock, dw, dh);
        pending--;
        finish();
      };
      img.onerror = () => {
        atlasMissing.add(`${set.url}|${z}/${iy}/${ix}`);
        drawCell(z - 1, wx, wz, ww, wh);
      };
      img.src = `./atlas/${set.url}/${z}/${iy}/${ix}.png`;
    };
    // Clamp to the pyramid's real extent at this level before the cell-count
    // guard: a far-out canvas tile spans many index cells, but only the few
    // inside the pyramid exist — counting the void used to blank the whole
    // underlay at overview zooms. Without an index, the google layout's
    // level-doubling (1 tile at zMin) still bounds the extent.
    const idx = atlasIndexes.get(set.url);
    const side = idx ? idx.sides[az - idx.zMin] : Math.pow(2, az - set.zMin);
    const ix0 = Math.max(0, Math.floor((X0 - set.originX) / bpt));
    const ix1 = Math.min(side - 1, Math.floor((X0 + S - 1e-6 - set.originX) / bpt));
    const iy0 = Math.max(0, Math.floor((Z0 - set.originZ) / bpt));
    const iy1 = Math.min(side - 1, Math.floor((Z0 + S - 1e-6 - set.originZ) / bpt));
    if (ix1 >= ix0 && iy1 >= iy0 && (ix1 - ix0 + 1) * (iy1 - iy0 + 1) <= 64) {
      for (let iy = iy0; iy <= iy1; iy++) {
        for (let ix = ix0; ix <= ix1; ix++) {
          const tx = set.originX + ix * bpt;
          const tz = set.originZ + iy * bpt;
          const cx0 = Math.max(X0, tx);
          const cz0 = Math.max(Z0, tz);
          const cx1 = Math.min(X0 + S, tx + bpt);
          const cz1 = Math.min(Z0 + S, tz + bpt);
          if (cx1 <= cx0 || cz1 <= cz0) continue;
          pending++;
          drawCell(az, cx0, cz0, cx1 - cx0, cz1 - cz0);
        }
      }
    }
    queued = true;
    if (pending === 0) setTimeout(finish, 0);
    return tile;
  },
});

function atlasSetForDim(): AtlasSetJson | null {
  const dimType = currentDim()?.dimType;
  return (state.atlas ?? []).find((s) => s.dim === dimType) ?? null;
}

/** True once the index or the probe has answered for this set. Until then the
 *  underlay stays off: offering it would fire a burst of 404s at a mirror that
 *  may well be empty, then take the layer away again. */
function atlasKnown(set: AtlasSetJson): boolean {
  return !!atlasIndexes.get(set.url) || atlasProbed.has(set.url);
}

/** Tiles on disk for a set: null until (or unless) we have learnt anything. */
function atlasTileCount(set: AtlasSetJson): number | null {
  const idx = atlasIndexes.get(set.url);
  if (idx) return idx.tiles;
  const probe = atlasProbed.get(set.url);
  return probe === false ? 0 : null;
}

function updateAtlasUnderlay() {
  const row = $('row-atlas-under');
  const cb = $('toggle-atlas-under') as HTMLInputElement;
  const set = atlasSetForDim();
  // An empty mirror is not worth a toggle: it would 404 on every tile in view.
  const usable = !!set && atlasKnown(set) && atlasTileCount(set) !== 0;
  row.hidden = !usable;
  // Every finishing probe calls back in here; rebuilding an unchanged layer
  // would throw away its tiles and redraw the underlay for nothing.
  const want = usable && cb.checked ? set.url : '';
  if (want !== atlasUnderlaySet) {
    if (atlasUnderlay) {
      map.removeLayer(atlasUnderlay);
      atlasUnderlay = null;
    }
    atlasUnderlaySet = want;
    if (want) {
      atlasUnderlay = new (AtlasUnderlayGrid as any)({
        set,
        tileSize: 512,
        minZoom: -16,
        maxZoom: 3,
        noWrap: true,
        keepBuffer: 2,
        opacity: 0.9,
        zIndex: 0,
        className: 'pixelated',
      });
      atlasUnderlay!.addTo(map);
    }
  }
  // Probe every mirrored set, not just this dimension's: the offline panel
  // has to be able to say which dimensions are still missing.
  for (const s of state.atlas ?? []) void ensureAtlasIndex(s);
  updateAtlasMirror();
}

// ------------------------------------------------------- offline mirror UI --

/** Per-dimension mirror status plus the command that completes the mirror. */
function updateAtlasMirror() {
  const body = $('atlas-mirror-body');
  const dims = new Set((currentWorld()?.dims ?? []).map((d) => d.dimType));
  const lines: string[] = [];
  const missing: string[] = [];
  for (const [dimType, label, dataset] of ATLAS_DATASETS) {
    if (!dims.has(dimType)) continue;
    const set = (state.atlas ?? []).find((s) => s.dim === dimType) ?? null;
    const tiles = set ? atlasTileCount(set) : null;
    const idx = set ? atlasIndexes.get(set.url) : null;
    let status: string;
    if (!set || tiles === 0) {
      status = 'not mirrored';
      missing.push(dataset);
    } else if (idx && idx.expected > 0 && idx.tiles < idx.expected) {
      const pct = Math.round((idx.tiles / idx.expected) * 100);
      status = `${idx.tiles.toLocaleString()} / ${idx.expected.toLocaleString()} tiles (${pct}%)`;
      missing.push(dataset);
    } else if (idx) {
      status = `complete — ${idx.tiles.toLocaleString()} tiles`;
    } else {
      status = 'mirrored';
    }
    lines.push(`<div>${escapeHtml(label)} — ${escapeHtml(status)}</div>`);
  }
  const dest = state.atlasDir || ATLAS_DEFAULT_DIR;
  const cmd = `scripts/atlas-mirror.py --dest ${dest} --fetch ${missing.join(' ')}`;
  body.innerHTML =
    lines.join('') +
    (missing.length
      ? '<p>Imagery comes only from this mirror — nothing is fetched from the Atlas ' +
        'server while you pan. To download the missing datasets (large, one time):</p>' +
        `<code>${escapeHtml(cmd)}</code><button id="atlas-mirror-copy">Copy command</button>`
      : '<p>Every dimension is mirrored locally — the map works fully offline.</p>');
  const copy = document.getElementById('atlas-mirror-copy');
  if (copy) {
    copy.onclick = () => {
      void navigator.clipboard.writeText(cmd);
      copy.textContent = 'copied';
    };
  }
}

// ------------------------------------------------------------------- atlas --

/** Loads the POI snapshot without touching any third party: the server's
 *  on-disk store first, then this browser's copy. Deliberately never expires —
 *  stale POIs beat a round trip to someone else's origin on every page load. */
async function loadAtlasStore(): Promise<AtlasLocation[] | null> {
  const stored = await fetchAtlasStore();
  if (stored) {
    atlasFetchedMs = stored.fetchedMs;
    return stored.locations;
  }
  const cached = localStorage.getItem('xt-atlas-cache');
  if (!cached) return null;
  try {
    const { time, data } = JSON.parse(cached);
    atlasFetchedMs = time;
    return data as AtlasLocation[];
  } catch {
    return null;
  }
}

/** Third-party rows, kept only when every field we render has the type we
 *  expect. A bad row is dropped rather than trusted into HTML or storage, and
 *  this runs on every load path — download, server store, browser cache — so
 *  nothing persisted under an older build gets replayed unchecked. */
function sanitizeAtlas(raw: unknown): AtlasLocation[] {
  if (!Array.isArray(raw)) return [];
  const str = (v: unknown) => (typeof v === 'string' ? v : null);
  const out: AtlasLocation[] = [];
  for (const l of raw as Record<string, unknown>[]) {
    if (!l || typeof l !== 'object') continue;
    const name = str(l.name);
    const x = Number(l.x);
    const y = Number(l.y);
    const z = Number(l.z);
    const dimension = Number(l.dimension);
    if (name === null || ![x, y, z, dimension].every(Number.isFinite)) continue;
    out.push({
      name,
      description: str(l.description) ?? '',
      tags: str(l.tags),
      dimension,
      x,
      y,
      z,
      wiki: str(l.wiki),
      videoUrl: str(l.videoUrl),
      dateAddedUtc: str(l.dateAddedUtc) ?? '',
    });
  }
  return out;
}

/** The only code path in the viewer that talks to api.blackportal.cloud, and
 *  it only ever runs from a click. Slims the payload to the ten fields we
 *  render and hands it to the server so no browser has to fetch it again. */
async function downloadAtlas(): Promise<boolean> {
  $('atlas-count').textContent = 'downloading from api.blackportal.cloud…';
  try {
    const res = await fetch(ATLAS_URL);
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    atlasData = sanitizeAtlas(await res.json());
    atlasFetchedMs = Date.now();
    if (!(await putAtlasStore(atlasData))) {
      // No server-side store (older build): keep it in this browser instead.
      try {
        localStorage.setItem(
          'xt-atlas-cache',
          JSON.stringify({ time: atlasFetchedMs, data: atlasData })
        );
      } catch {
        /* quota full: the POIs just won't survive this tab */
      }
    }
    return true;
  } catch (e) {
    $('atlas-count').textContent = `Atlas unreachable (${e})`;
    return false;
  }
}

/** `allowRemote` is true only for a deliberate click. A permalink or a restored
 *  preference may show POIs we already have, but must never make the recipient
 *  fetch a megabyte from a third party they never chose to contact. */
async function toggleAtlas(allowRemote: boolean) {
  const cb = $('toggle-atlas') as HTMLInputElement;
  const on = cb.checked;
  ($('atlas-filter') as HTMLInputElement).hidden = !on;
  if (allowRemote) localStorage.setItem('xt-atlas', on ? '1' : '0');
  if (!on) {
    atlasLayer.clearLayers();
    $('atlas-refresh').hidden = true;
    $('atlas-count').textContent = '';
    return;
  }
  if (!atlasData) {
    $('atlas-count').textContent = 'loading…';
    atlasData = sanitizeAtlas(await loadAtlasStore());
  }
  if (!atlasData) {
    if (!allowRemote) {
      // Nothing downloaded yet: leave the layer honestly off and let the
      // checkbox itself be the download button.
      cb.checked = false;
      ($('atlas-filter') as HTMLInputElement).hidden = true;
      $('atlas-count').textContent =
        'Atlas POIs not downloaded — tick to fetch them once (~1 MB from api.blackportal.cloud).';
      return;
    }
    if (!(await downloadAtlas())) return;
  }
  $('atlas-refresh').hidden = false;
  redrawAtlas();
}

/** Explicit, manual re-download — the snapshot has no expiry of its own. */
async function refreshAtlas() {
  if (await downloadAtlas()) redrawAtlas();
}

function redrawAtlas() {
  atlasLayer.clearLayers();
  if (!($('toggle-atlas') as HTMLInputElement).checked || !atlasData) return;
  const dimType = currentDim()?.dimType;
  const wantDim = dimType === 'overworld' ? 0 : dimType === 'the_end' ? 2 : -1;
  if (wantDim < 0) {
    $('atlas-count').textContent = 'no Atlas data for this dimension';
    return;
  }
  const f = atlasFilter.toLowerCase();
  const v = viewWindow();
  let shown = 0;
  let matched = 0;
  for (const loc of atlasData) {
    if (loc.dimension !== wantDim) continue;
    if (f && !loc.name.toLowerCase().includes(f) && !(loc.tags ?? '').toLowerCase().includes(f)) {
      continue;
    }
    matched++;
    if (loc.x < v.x0 || loc.x > v.x1 || loc.z < v.z0 || loc.z > v.z1) continue;
    shown++;
    const marker = L.circleMarker(toLatLng(loc.x, loc.z), {
      radius: 7,
      color: '#8a5a00',
      weight: 1.5,
      fillColor: '#ffb52e',
      fillOpacity: 0.9,
    });
    const tags = (loc.tags ?? '')
      .split(',')
      .filter(Boolean)
      .map((t) => `<code>${escapeHtml(t.trim())}</code>`)
      .join(' ');
    const wiki = loc.wiki ? safeUrl(loc.wiki) : null;
    const video = loc.videoUrl ? safeUrl(loc.videoUrl) : null;
    const links = [
      wiki ? `<a href="${escapeHtml(wiki)}" target="_blank" rel="noopener">wiki</a>` : '',
      video ? `<a href="${escapeHtml(video)}" target="_blank" rel="noopener">video</a>` : '',
    ]
      .filter(Boolean)
      .join(' · ');
    marker.bindPopup(
      `<div class="wp-popup"><b>${escapeHtml(loc.name)}</b><br>` +
        (loc.description ? `${escapeHtml(loc.description)}<br>` : '') +
        `<span class="muted">${escapeHtml(`${loc.x}, ${loc.y}, ${loc.z}`)} · added ${escapeHtml(
          (loc.dateAddedUtc ?? '').slice(0, 10)
        )}</span><br>${tags}${links ? `<br>${links}` : ''}</div>`
    );
    marker.addTo(atlasLayer);
  }
  const age = atlasFetchedMs
    ? ` · downloaded ${new Date(atlasFetchedMs).toISOString().slice(0, 10)}`
    : '';
  $('atlas-count').textContent = `${matched} locations (${shown} in view)${age}`;
}

function replaceBaseLayer() {
  // Keep the outgoing layer visible until the incoming one has painted —
  // removing it first blanks the whole view for the round-trip.
  const old = baseLayer;
  baseLayer = L.tileLayer(tileUrl(), {
    tileSize: 512,
    minZoom: -16,
    maxZoom: 3,
    maxNativeZoom: 0,
    minNativeZoom: -16,
    noWrap: true,
    keepBuffer: 3,
    errorTileUrl: TRANSPARENT_TILE,
    className: 'pixelated',
    zIndex: 2,
  });
  baseLayer.on('loading', () => ($('tile-loading').hidden = false));
  baseLayer.on('load', () => ($('tile-loading').hidden = true));
  if (old) {
    // 'load' can be missed entirely (nothing in view), so a timer backstops it.
    const dropOld = () => {
      clearTimeout(timer);
      if (map.hasLayer(old)) map.removeLayer(old);
    };
    const timer = setTimeout(dropOld, 4000);
    baseLayer.once('load', dropOld);
  }
  baseLayer.addTo(map);
  const w = currentWorld();
  const d = currentDim();
  const mw = d?.mws[sel.m];
  $('map-name').textContent = w
    ? `${worldLabel(w)} · ${d?.folder ?? ''} · ${mw?.display ?? ''} · ${sel.layer}`
    : '';
}

// ---------------------------------------------------- live uploads overlay --
// Regions uploaded by companion clients land in the ingest dir's merged tree,
// which is its own world entry. Drawing that tree over the world being viewed
// makes fresh mapping show up in place instead of behind a world switch.

let ingestOverlay: L.TileLayer | null = null;
/** The merged-tree map the overlay currently mirrors, null when off. */
let ingestOverlayFrom: Selection | null = null;

/** The merge tool's base-domain rule: Multiplayer_2b2t ⇄ Multiplayer_2b2t.org. */
function worldKey(id: string): string {
  return id
    .replace(/^Multiplayer_/, '')
    .replace(/\.(org|net|com)$/i, '')
    .toLowerCase();
}

/** Where the current selection's data would land in the merged ingest tree. */
function mergedCounterpart(): Selection | null {
  const w = currentWorld();
  if (!w || w.origin === 'ingestMerged' || w.origin === 'ingestPlayer') return null;
  let wi = state.worlds.findIndex((o) => o.origin === 'ingestMerged' && o.id === w.id);
  if (wi < 0) {
    wi = state.worlds.findIndex(
      (o) => o.origin === 'ingestMerged' && worldKey(o.id) === worldKey(w.id)
    );
  }
  if (wi < 0) return null;
  const mw = state.worlds[wi];
  const di = mw.dims.findIndex((d) => d.folder === currentDim()?.folder);
  if (di < 0) return null;
  const mwId = currentDim()?.mws[sel.m]?.id;
  const mi = mw.dims[di].mws.findIndex((m) => m.id === mwId);
  if (mi < 0) return null;
  if (sel.layer !== 'surface') {
    const cave = +sel.layer.slice(5);
    if (!mw.dims[di].mws[mi].caveLayers.includes(cave)) return null;
  }
  return { w: wi, d: di, m: mi, layer: sel.layer };
}

/** The roof view the uploads overlay was built with, so a change rebuilds it. */
let ingestOverlayRoof = '';

function updateIngestOverlay() {
  const row = $('row-live-overlay');
  const cb = $('toggle-live-overlay') as HTMLInputElement;
  const from = mergedCounterpart();
  row.hidden = !from;
  const want = from && cb.checked ? from : null;
  // Rebuilding an unchanged layer blanks its tiles until they reload — a
  // "flash" on every roots rescan (which live ingest triggers for each new
  // layer a mapping client discovers). Keep the layer whenever it would be
  // rebuilt from the same source.
  const same =
    want &&
    ingestOverlayFrom &&
    want.w === ingestOverlayFrom.w &&
    want.d === ingestOverlayFrom.d &&
    want.m === ingestOverlayFrom.m &&
    want.layer === ingestOverlayFrom.layer &&
    roofQuery() === ingestOverlayRoof;
  if (same || (!want && !ingestOverlayFrom)) return;
  ingestOverlayRoof = roofQuery();
  if (ingestOverlay) {
    map.removeLayer(ingestOverlay);
    ingestOverlay = null;
  }
  ingestOverlayFrom = want;
  const f = ingestOverlayFrom;
  if (!f) return;
  ingestOverlay = L.tileLayer(`./tiles/${f.w}/${f.d}/${f.m}/${f.layer}/{z}/{x}/{y}${roofQuery()}`, {
    tileSize: 512,
    minZoom: -16,
    maxZoom: 3,
    maxNativeZoom: 0,
    minNativeZoom: -16,
    noWrap: true,
    keepBuffer: 3,
    errorTileUrl: TRANSPARENT_TILE,
    className: 'pixelated',
    zIndex: 3, // above the world's own tiles, below highlight overlays
  });
  ingestOverlay.addTo(map);
}

// ------------------------------------------------------ live preview layer --
// Terrain companion clients are seeing right now (POST /ingest/v1/preview),
// served from the server's in-memory canvas — visible seconds after a chunk
// loads in someone's game, long before Xaero saves the region to disk. Real
// region uploads evict what they cover, so the preview yields to map data.

let previewLayer: L.TileLayer | null = null;
/** Dim resource key the layer currently shows, '' when off. */
let previewDim = '';

/** A dimension's resource key — what live events and waypoint files carry.
 *  `dimType` is only a behaviour hint (a custom dimension that behaves like the
 *  Overworld reports "overworld"), so the id wins whenever the server has it. */
function dimKeyOf(d: { dimId?: string | null; dimType: string | null; folder: string }): string {
  if (d.dimId) return d.dimId;
  const t = d.dimType;
  if (t === 'overworld' || t === 'the_nether' || t === 'the_end') return `minecraft:${t}`;
  return t ?? d.folder;
}

/** The current dimension's resource key (what the preview canvas is keyed by). */
function currentDimKey(): string | null {
  const dim = currentDim();
  if (!dim) return null;
  if (dim.dimId) return dim.dimId;
  const t = dim.dimType;
  if (t === 'overworld' || t === 'the_nether' || t === 'the_end') return `minecraft:${t}`;
  return t ?? null;
}

function updatePreviewLayer() {
  const cb = $('toggle-live-preview') as HTMLInputElement;
  const key = currentDimKey();
  const want = cb.checked && key ? key : '';
  if (want === previewDim) return;
  if (previewLayer) {
    map.removeLayer(previewLayer);
    previewLayer = null;
  }
  previewDim = want;
  if (!want) return;
  previewLayer = L.tileLayer(`./preview/${encodeURIComponent(want)}/{z}/{x}/{y}`, {
    tileSize: 512,
    minZoom: -16,
    maxZoom: 3,
    maxNativeZoom: 0,
    minNativeZoom: -16,
    noWrap: true,
    keepBuffer: 2,
    errorTileUrl: TRANSPARENT_TILE,
    className: 'pixelated',
    zIndex: 4, // above the world tiles and uploads overlay, below highlights
  });
  previewLayer.addTo(map);
}

// ------------------------------------------------------------------ guides --

// Canvas rasterizers glitch on paths millions of pixels long, so guides are
// clamped to a window around the viewport and redrawn while panning.
function viewWindow() {
  const b = map.getBounds().pad(2);
  const a = fromLatLng(b.getNorthWest());
  const c = fromLatLng(b.getSouthEast());
  return { x0: a.x, z0: a.z, x1: c.x, z1: c.z };
}

function redrawGuides() {
  guideLayer.clearLayers();
  if (!($('toggle-guides') as HTMLInputElement).checked) return;
  const dimType = currentDim()?.dimType;
  if (dimType !== 'overworld' && dimType !== 'the_nether') return;
  const limit = dimType === 'the_nether' ? 3_750_000 : 30_000_000;
  const border = { color: '#d84040', weight: 1.5, opacity: 0.8, interactive: false };
  const hw = { color: '#3faf5f', weight: 1, opacity: 0.6, interactive: false };
  const dg = { color: '#2f8f8f', weight: 1, opacity: 0.5, interactive: false };
  const v = viewWindow();
  const x0 = Math.max(v.x0, -limit);
  const x1 = Math.min(v.x1, limit);
  const z0 = Math.max(v.z0, -limit);
  const z1 = Math.min(v.z1, limit);
  if (x0 > x1 || z0 > z1) return;
  const seg = (ax: number, az: number, bx: number, bz: number, style: L.PolylineOptions) =>
    L.polyline([toLatLng(ax, az), toLatLng(bx, bz)], style).addTo(guideLayer);
  // world border edges (only the parts inside the window)
  if (v.z0 <= -limit && -limit <= v.z1) seg(x0, -limit, x1, -limit, border);
  if (v.z0 <= limit && limit <= v.z1) seg(x0, limit, x1, limit, border);
  if (v.x0 <= -limit && -limit <= v.x1) seg(-limit, z0, -limit, z1, border);
  if (v.x0 <= limit && limit <= v.x1) seg(limit, z0, limit, z1, border);
  // axis highways
  if (z0 <= 0 && 0 <= z1) seg(x0, 0, x1, 0, hw);
  if (x0 <= 0 && 0 <= x1) seg(0, z0, 0, z1, hw);
  // diagonals x = z and x = -z, clipped to the window box
  const clip = (sign: 1 | -1) => {
    // points where the line x = sign*z crosses the window
    const lo = Math.max(x0, sign === 1 ? z0 : -z1, -limit);
    const hi = Math.min(x1, sign === 1 ? z1 : -z0, limit);
    if (lo <= hi) seg(lo, sign * lo, hi, sign * hi, dg);
  };
  clip(1);
  clip(-1);
}

// -------------------------------------------------------------------- grid --

const RegionGrid = L.GridLayer.extend({
  createTile(coords: L.Coords) {
    const tile = document.createElement('canvas');
    tile.width = 512;
    tile.height = 512;
    const ctx = tile.getContext('2d')!;
    ctx.strokeStyle = 'rgba(255,255,255,0.25)';
    ctx.lineWidth = 1;
    ctx.strokeRect(0.5, 0.5, 511, 511);
    if (coords.z >= 0) {
      ctx.fillStyle = 'rgba(255,255,255,0.4)';
      ctx.font = '11px monospace';
      ctx.fillText(`r ${coords.x},${coords.y}`, 6, 14);
    }
    return tile;
  },
});

function toggleGrid() {
  const on = ($('toggle-grid') as HTMLInputElement).checked;
  if (gridLayer) {
    map.removeLayer(gridLayer);
    gridLayer = null;
  }
  if (on) {
    gridLayer = new (RegionGrid as any)({
      tileSize: 512,
      minZoom: -4,
      maxZoom: 3,
      maxNativeZoom: 0,
      minNativeZoom: -16,
      // Above the Nether underlay (1) and base (2): the grid is a reference,
      // it must never be painted over by imagery added later.
      zIndex: 6,
    });
    gridLayer!.addTo(map);
  }
}

// --------------------------------------------------------------- waypoints --

function dimMatches(file: WaypointFileJson): boolean {
  const dim = currentDim();
  if (!dim) return false;
  const norm = (s: string) => s.replace(/^minecraft:/, '');
  if (file.dimKey) return norm(file.dimKey) === norm(dimKeyOf(dim));
  return file.dimFolder === dim.folder;
}

function visibleWaypoints(): { wp: WaypointJson; file: WaypointFileJson }[] {
  const showArchived = ($('toggle-archived') as HTMLInputElement).checked;
  const out: { wp: WaypointJson; file: WaypointFileJson }[] = [];
  for (const file of waypointFiles) {
    if (!dimMatches(file)) continue;
    for (const wp of file.waypoints) {
      if (wp.disabled) continue;
      if (wp.archived && !showArchived) continue;
      if (wpFilter && !wp.name.toLowerCase().includes(wpFilter)) continue;
      out.push({ wp, file });
    }
  }
  return out;
}

function redrawWaypoints() {
  waypointLayer.clearLayers();
  const list = $('wp-list');
  // Rebuilt on every map move: the user's scroll position must survive it.
  const scroll = list.scrollTop;
  list.innerHTML = '';
  if (!($('toggle-waypoints') as HTMLInputElement).checked) return;
  const v = viewWindow();
  for (const { wp } of visibleWaypoints()) {
    // Cull far-off markers: huge pixel offsets break canvas rendering and
    // cost time; the sidebar list still shows everything.
    const onMap = wp.x >= v.x0 && wp.x <= v.x1 && wp.z >= v.z0 && wp.z <= v.z1;
    if (!onMap) {
      const li = wpListItem(wp);
      list.appendChild(li);
      continue;
    }
    const marker = L.circleMarker(toLatLng(wp.x, wp.z), {
      radius: 6,
      color: wp.archived ? wp.rgb : '#111',
      weight: wp.archived ? 2 : 1.5,
      dashArray: wp.archived ? '3 3' : undefined,
      fillColor: wp.rgb,
      fillOpacity: wp.archived ? 0.25 : 0.95,
    });
    marker.bindPopup(
      `<div class="wp-popup"><b>${escapeHtml(wp.name)}</b>${
        wp.archived ? ' <span class="muted">(archived — deleted in game, kept by vault)</span>' : ''
      }<br>` +
        `<span class="muted">${wp.x}, ${wp.y ?? '~'}, ${wp.z} · set ${escapeHtml(
          wp.set
        )}</span><br>` +
        `<span class="muted">/tp ${wp.x} ${wp.y ?? 100} ${wp.z}</span></div>`
    );
    marker.addTo(waypointLayer);
    if (pendingWpPopup === wpKey(wp)) {
      pendingWpPopup = null;
      marker.openPopup();
    }
    list.appendChild(wpListItem(wp, marker));
  }
  list.scrollTop = scroll;
}

function wpListItem(wp: WaypointJson, marker?: L.CircleMarker): HTMLLIElement {
  const li = document.createElement('li');
  const dot = document.createElement('span');
  dot.className = 'dot';
  dot.style.background = wp.archived ? 'transparent' : wp.rgb;
  dot.style.borderColor = wp.rgb;
  const name = document.createElement('span');
  name.textContent = wp.name || '(unnamed)';
  if (wp.archived) {
    li.classList.add('archived');
    li.title = 'archived — deleted in game, kept by the vault';
  }
  const coords = document.createElement('span');
  coords.className = 'coords';
  coords.textContent = `${wp.x}, ${wp.z}`;
  li.append(dot, name, coords);
  li.onclick = () => {
    // If the view doesn't move, the marker survives and opens directly; if it
    // does, the moveend rebuild opens the popup via pendingWpPopup instead.
    pendingWpPopup = wpKey(wp);
    map.setView(toLatLng(wp.x, wp.z), Math.max(map.getZoom(), -1));
    marker?.openPopup();
  };
  return li;
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

/** Links come from a community-edited third-party list, so only ever emit an
 *  absolute http(s) URL — a `javascript:` href would run in our own origin. */
function safeUrl(u: string): string | null {
  try {
    const p = new URL(u);
    return p.protocol === 'http:' || p.protocol === 'https:' ? p.href : null;
  } catch {
    return null;
  }
}

async function reloadWaypoints() {
  const gen = ++wpReloadGen;
  waypointFiles = [];
  try {
    if (currentWorld()?.hasWaypoints) {
      const files = await fetchWaypoints(sel.w);
      if (gen !== wpReloadGen) return; // a newer selection owns the list now
      waypointFiles = files;
    }
  } catch (e) {
    console.warn('waypoints failed', e);
  }
  if (gen !== wpReloadGen) return;
  redrawWaypoints();
}

// -------------------------------------------------------------- live mode --
// One WebSocket (/ws/live) carries tile/DB invalidations and player
// positions; the browser never polls.

interface LivePlayer {
  name: string;
  dim: string;
  x: number;
  y: number;
  z: number;
  yaw: number;
  t: number; // server time of last update (unix ms)
  rxAt: number; // performance.now() at last receive (age/interp math)
  marker: L.Marker | null;
  trailLine: L.Polyline | null;
  trail: { x: number; z: number; at: number }[];
  fromX: number;
  fromZ: number;
  animStart: number;
  animDur: number;
}

const livePlayers = new Map<string, LivePlayer>();
const liveLayer = L.layerGroup();
/** Player the view is glued to (kiosk/second-screen mode); null = free. */
let followName: string | null = null;
let liveWs: WebSocket | null = null;
/** True between onopen and onclose. An "open" socket can still be dead
 *  (NAT/proxy drop): tickLive closes one that has been silent too long. */
let liveConnected = false;
/** performance.now() of the last message on the socket, heartbeats included. */
let liveLastRx = 0;
let liveBackoff = 1000;
let liveLastSeq = -1;
let liveEverConnected = false;
let resyncing = false;
/** Tile/preview/db events discarded while a state resync was in flight. */
let resyncDropped = false;
let animPending = false;

function connectLive() {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  // Path derived from the page's own, mirroring the tiles' relative URLs:
  // behind a reverse proxy at a subpath, absolute /ws/live misses the mount.
  const base = location.pathname.replace(/[^/]*$/, '');
  const ws = new WebSocket(`${proto}://${location.host}${base}ws/live`);
  liveWs = ws;
  ws.onmessage = (e) => {
    liveLastRx = performance.now();
    try {
      handleLiveEvent(JSON.parse(e.data) as LiveEvent);
    } catch (err) {
      console.warn('bad live event', err);
    }
  };
  ws.onopen = () => {
    liveBackoff = 1000;
    liveConnected = true;
    // Starts the silence clock: a stale value from the previous connection
    // would get this socket closed before its hello even arrives.
    liveLastRx = performance.now();
    $('live-status').textContent = '';
  };
  ws.onclose = () => {
    liveConnected = false;
    $('live-status').textContent = 'live: reconnecting…';
    const delay = liveBackoff * (0.7 + Math.random() * 0.6); // jitter
    liveBackoff = Math.min(liveBackoff * 2, 15000);
    setTimeout(connectLive, delay);
  };
}

function handleLiveEvent(ev: LiveEvent) {
  switch (ev.type) {
    case 'hello': {
      // Anything broadcast while we were disconnected is gone; refresh the
      // tile layers in place unless the server seq shows nothing was missed.
      // (Never redraw(): that blanks every visible tile until it refetches.)
      const needRefresh = liveEverConnected && ev.v !== liveLastSeq;
      liveEverConnected = true;
      liveLastSeq = ev.v;
      for (const p of livePlayers.values()) removePlayerVisuals(p);
      livePlayers.clear();
      for (const pos of ev.players) applyPos(pos, true);
      renderPlayerList();
      if (needRefresh) refreshAllTileLayers(ev.v);
      break;
    }
    case 'resync':
      // This socket lagged behind the broadcast channel; events were skipped.
      refreshAllTileLayers(liveLastSeq + 1);
      break;
    case 'hb':
      // Idle keepalive: its receipt already fed liveLastRx in onmessage.
      break;
    case 'pos':
      applyPos(ev, false);
      break;
    case 'player_removed': {
      liveLastSeq = ev.v;
      const p = livePlayers.get(ev.player);
      if (p) {
        removePlayerVisuals(p);
        livePlayers.delete(ev.player);
        renderPlayerList();
      }
      break;
    }
    case 'tiles':
      liveLastSeq = ev.v;
      if (resyncing) resyncDropped = true;
      else handleTilesEvent(ev);
      break;
    case 'preview':
      liveLastSeq = ev.v;
      if (resyncing) resyncDropped = true;
      else handlePreviewEvent(ev);
      break;
    case 'db':
      liveLastSeq = ev.v;
      if (resyncing) resyncDropped = true;
      else handleDbEvent(ev);
      break;
    case 'state':
      liveLastSeq = ev.v;
      resyncState();
      break;
  }
}

/** Refreshes the loaded tiles of `layer` that intersect the changed regions.
 * Only tiles at the layer's current internal zoom (transient parent/child
 * tiles reload on their own), with all math in URL-space zoom so the nether
 * underlay's zoomOffset works; without `deep` only native (URL z=0) tiles
 * have fresh server stamps. New content is preloaded, then swapped in. */
function refreshLayerTiles(
  layer: L.TileLayer,
  regions: [number, number][] | null,
  deep: boolean,
  seq: number
) {
  const anyLayer = layer as any;
  const tiles = anyLayer._tiles as Record<string, { coords: any; el: HTMLElement }> | undefined;
  if (!tiles) return;
  const tileZoom = anyLayer._tileZoom;
  const offset = anyLayer.options.zoomOffset ?? 0;
  for (const key in tiles) {
    const t = tiles[key];
    const c = t.coords;
    if (c.z !== tileZoom) continue;
    const urlZ = c.z + offset;
    if (!deep && urlZ !== 0) continue;
    if (regions) {
      const span = Math.pow(2, -urlZ);
      const hit = regions.some(
        ([rx, rz]) =>
          rx >= c.x * span && rx < (c.x + 1) * span && rz >= c.y * span && rz < (c.y + 1) * span
      );
      if (!hit) continue;
    }
    const el = t.el as HTMLImageElement;
    if (!el || el.tagName !== 'IMG') continue;
    const base = anyLayer.getTileUrl(c);
    const url = `${base}${base.includes('?') ? '&' : '?'}v=${seq}`;
    const img = new Image();
    img.onload = () => {
      el.src = url; // now cached: swaps without blanking
    };
    img.src = url;
  }
}

function handleTilesEvent(ev: TilesEvent) {
  // The uploads overlay mirrors a different world entry (the merged ingest
  // tree), so its refresh must run before the current-world filter.
  const f = ingestOverlayFrom;
  if (ingestOverlay && f && ev.w === f.w && ev.d === f.d && ev.m === f.m && ev.layer === f.layer) {
    refreshLayerTiles(ingestOverlay, ev.regions, ev.deep, ev.v);
  }
  if (ev.w !== sel.w) return;
  if (baseLayer && ev.d === sel.d && ev.m === sel.m && ev.layer === sel.layer) {
    refreshLayerTiles(baseLayer, ev.regions, ev.deep, ev.v);
  }
  if (netherUnderlay && ev.layer === 'surface') {
    const netherIdx = currentWorld()?.dims.findIndex((d) => d.dimType === 'the_nether') ?? -1;
    if (netherIdx >= 0 && ev.d === netherIdx && ev.m === netherMwIdx(netherIdx)) {
      refreshLayerTiles(netherUnderlay, ev.regions, ev.deep, ev.v);
    }
  }
}

function handleDbEvent(ev: DbEvent) {
  // Not gated on sel.w: the database a companion client just wrote belongs to
  // the merged ingest tree, which is a different world entry from the one in
  // view. Its layer is keyed by that world, and is exactly the one to refresh.
  // The event names a world and a database but not a dimension, so ask the
  // sources: the only layers that exist are theirs.
  for (const src of hlSources()) {
    if (src.w !== ev.w) continue;
    const layer = hlLayers.get(hlKey(src.w, src.d, ev.db));
    // Highlight stamps are mtime-based, fresh at every zoom.
    if (layer) refreshLayerTiles(layer, null, true, ev.v);
  }
}

function handlePreviewEvent(ev: PreviewEvent) {
  if (previewLayer && ev.dim === previewDim) {
    refreshLayerTiles(previewLayer, ev.regions, true, ev.v);
  }
}

/** Refreshes every loaded tile of every layer in place (preload, then swap) —
 * the recovery path after missed broadcasts. redraw() would blank the map. */
function refreshAllTileLayers(seq: number) {
  for (const layer of [baseLayer, netherUnderlay, ingestOverlay, previewLayer]) {
    if (layer) refreshLayerTiles(layer, null, true, seq);
  }
  for (const l of hlLayers.values()) refreshLayerTiles(l, null, true, seq);
}

/** The server rescanned its roots: world/dim/mw indices are re-dealt, so
 * refetch state and re-resolve the selection by stable identity. Tile/db
 * events are dropped until the new state is in.
 *
 * When the resolved indices are unchanged — the common case, since live
 * ingest rescans every time a mapping client uploads into a brand-new
 * layer — nothing visible is rebuilt: tearing the tile layers down just to
 * recreate identical ones made the whole map flash on every such upload. */
let resyncAgain = false;

async function resyncState() {
  if (resyncing) {
    // A second rescan finished during the fetch; its state is newer than
    // what is in flight, so go once more when this pass is done.
    resyncAgain = true;
    return;
  }
  resyncing = true;
  try {
    const before = { ...sel };
    // Root + id identifies a world entry; the ingest trees reuse the id of
    // the world they mirror, so an id-only match can land on the wrong one.
    const oldRoot = currentWorld()?.root;
    const oldWorld = currentWorld()?.id;
    const oldDim = currentDim()?.folder;
    const oldMw = currentDim()?.mws[sel.m]?.id;
    state = await fetchState();
    if (state.worlds.length === 0) {
      location.reload();
      return;
    }
    let wi = state.worlds.findIndex((w) => w.id === oldWorld && w.root === oldRoot);
    if (wi < 0) wi = state.worlds.findIndex((w) => w.id === oldWorld);
    sel.w = wi >= 0 ? wi : 0;
    const di = currentWorld()?.dims.findIndex((d) => d.folder === oldDim) ?? -1;
    sel.d = di >= 0 ? di : 0;
    const mi = currentDim()?.mws.findIndex((m) => m.id === oldMw) ?? -1;
    sel.m = mi >= 0 ? mi : 0;
    if (sel.w === before.w && sel.d === before.d && sel.m === before.m) {
      // Same view: keep every layer in place. The world list may have grown
      // (new ingest tree/layer), so refresh the chrome around the map only;
      // updateIngestOverlay rebuilds nothing unless its source moved.
      rebuildSidebar();
      updateIngestOverlay();
      // The world list growing is also how a *new* overlay arrives: the first
      // breadcrumb or palette row a client ever uploads creates the database
      // and triggers this rescan. Without this the checkbox for it does not
      // exist until the page is reloaded.
      rebuildHighlightPanel();
    } else {
      applySelection(); // rebuilds all tile layers fresh
    }
    await refreshRootsUi();
  } catch (e) {
    console.warn('state resync failed', e);
  } finally {
    resyncing = false;
    if (resyncDropped) {
      resyncDropped = false;
      // Their seqs were already recorded, so the next hello comparison can't
      // catch the loss — refresh in place exactly like a lagged reconnect.
      refreshAllTileLayers(liveLastSeq);
    }
  }
  if (resyncAgain) {
    resyncAgain = false;
    void resyncState();
  }
}

// ------------------------------------------------------------ live players --

function applyPos(ev: PosEvent, initial: boolean) {
  const nowP = performance.now();
  let p = livePlayers.get(ev.player);
  if (!p) {
    p = {
      name: ev.player,
      dim: ev.dim,
      x: ev.x,
      y: ev.y,
      z: ev.z,
      yaw: ev.yaw,
      t: ev.t,
      rxAt: nowP,
      marker: null,
      trailLine: null,
      trail: [],
      fromX: ev.x,
      fromZ: ev.z,
      animStart: 0,
      animDur: 0,
    };
    livePlayers.set(ev.player, p);
  } else {
    if (ev.t < p.t) return; // out-of-order update
    const gapMs = nowP - p.rxAt;
    const dist = Math.hypot(ev.x - p.x, ev.z - p.z);
    const teleport = dist > Math.max(1, gapMs / 1000) * 150; // beyond even boatfly/pitch40
    if (ev.dim !== p.dim || teleport || initial) {
      p.trail = [];
      p.fromX = ev.x;
      p.fromZ = ev.z;
      p.animDur = 0; // snap, don't slide across the map
      if (ev.dim !== p.dim) removePlayerVisuals(p);
    } else {
      const cur = displayedPos(p, nowP);
      p.fromX = cur.x;
      p.fromZ = cur.z;
      p.animStart = nowP;
      p.animDur = Math.min(1000, Math.max(100, gapMs));
    }
    p.dim = ev.dim;
    p.x = ev.x;
    p.y = ev.y;
    p.z = ev.z;
    p.yaw = ev.yaw;
    p.t = ev.t;
    p.rxAt = nowP;
  }
  p.trail.push({ x: ev.x, z: ev.z, at: nowP });
  while (p.trail.length > 30 || (p.trail.length > 0 && nowP - p.trail[0].at > 60_000)) {
    p.trail.shift();
  }
  updatePlayerVisuals(p);
  renderPlayerList();
  scheduleAnim();
  if (p.name === followName) followPan(p, initial);
}

function displayedPos(p: LivePlayer, nowP: number): { x: number; z: number } {
  if (p.animDur <= 0 || nowP >= p.animStart + p.animDur) return { x: p.x, z: p.z };
  const f = (nowP - p.animStart) / p.animDur;
  return { x: p.fromX + (p.x - p.fromX) * f, z: p.fromZ + (p.z - p.fromZ) * f };
}

function dimKeyMatchesCurrent(dimKey: string): boolean {
  const dim = currentDim();
  if (!dim) return false;
  const norm = (s: string) => s.replace(/^minecraft:/, '');
  return norm(dimKey) === norm(dimKeyOf(dim));
}

function updatePlayerVisuals(p: LivePlayer) {
  if (!dimKeyMatchesCurrent(p.dim)) {
    removePlayerVisuals(p);
    return;
  }
  const pos = displayedPos(p, performance.now());
  if (!p.marker) {
    p.marker = L.marker(toLatLng(pos.x, pos.z), {
      icon: L.divIcon({
        className: 'pl-icon',
        iconSize: [0, 0],
        html:
          `<div class="pl-marker"><div class="pl-arrow"></div>` +
          `<div class="pl-name">${escapeHtml(p.name)}</div></div>`,
      }),
      interactive: false,
      zIndexOffset: 1000,
    });
    p.marker.addTo(liveLayer);
  } else {
    p.marker.setLatLng(toLatLng(pos.x, pos.z));
  }
  const arrow = p.marker.getElement()?.querySelector('.pl-arrow') as HTMLElement | null;
  if (arrow) arrow.style.transform = `rotate(${(p.yaw + 180) % 360}deg)`;
  updateTrail(p);
}

function updateTrail(p: LivePlayer) {
  const show = ($('toggle-trails') as HTMLInputElement).checked && p.marker !== null;
  const nowP = performance.now();
  const pts = show ? p.trail.filter((q) => nowP - q.at <= 60_000) : [];
  if (pts.length < 2) {
    if (p.trailLine) {
      liveLayer.removeLayer(p.trailLine);
      p.trailLine = null;
    }
    return;
  }
  const latlngs = pts.map((q) => toLatLng(q.x, q.z));
  if (!p.trailLine) {
    p.trailLine = L.polyline(latlngs, {
      color: '#ffd23e',
      weight: 2,
      opacity: 0.35,
      interactive: false,
    });
    p.trailLine.addTo(liveLayer);
  } else {
    p.trailLine.setLatLngs(latlngs);
  }
}

function removePlayerVisuals(p: LivePlayer) {
  if (p.marker) {
    liveLayer.removeLayer(p.marker);
    p.marker = null;
  }
  if (p.trailLine) {
    liveLayer.removeLayer(p.trailLine);
    p.trailLine = null;
  }
}

/** Re-derives every marker from the position store (dim/world switches). */
function redrawLivePlayers() {
  for (const p of livePlayers.values()) {
    removePlayerVisuals(p);
    updatePlayerVisuals(p);
  }
  renderPlayerList();
}

function scheduleAnim() {
  if (!animPending) {
    animPending = true;
    requestAnimationFrame(animStep);
  }
}

function animStep() {
  animPending = false;
  const nowP = performance.now();
  let active = false;
  for (const p of livePlayers.values()) {
    if (!p.marker || p.animDur <= 0) continue;
    const cur = displayedPos(p, nowP);
    p.marker.setLatLng(toLatLng(cur.x, cur.z));
    if (nowP >= p.animStart + p.animDur) p.animDur = 0;
    else active = true;
  }
  if (active) scheduleAnim();
}

function dimBadge(dimKey: string): { label: string; cls: string } {
  const d = dimKey.replace(/^minecraft:/, '');
  if (d === 'overworld') return { label: 'OW', cls: 'ow' };
  if (d === 'the_nether') return { label: 'N', cls: 'nether' };
  if (d === 'the_end') return { label: 'E', cls: 'end' };
  return { label: d.slice(0, 6), cls: 'custom' };
}

function renderPlayerList() {
  const list = $('player-list');
  // Rebuilt every live tick: the user's scroll position must survive it.
  const scroll = list.scrollTop;
  list.innerHTML = '';
  const players = [...livePlayers.values()].sort((a, b) => a.name.localeCompare(b.name));
  if (!liveConnected) {
    // The ticks keep calling in here while the socket is down; a roster
    // count must not clobber the reconnect notice onclose wrote.
    $('live-status').textContent = 'live: reconnecting…';
  } else if (players.length === 0) {
    $('live-status').textContent = followName
      ? `waiting for ${followName}…`
      : 'no live players — connect the companion addon (see the Share panel)';
  } else {
    $('live-status').textContent = followName
      ? livePlayers.has(followName)
        ? `following ${followName} — drag the map to stop`
        : `waiting for ${followName}…`
      : '';
  }
  if (players.length === 0) return;
  const nowP = performance.now();
  for (const p of players) {
    const li = document.createElement('li');
    const age = Math.max(0, nowP - p.rxAt) / 1000;
    const badge = dimBadge(p.dim);
    const b = document.createElement('span');
    b.className = `dim-badge ${badge.cls}`;
    b.textContent = badge.label;
    const name = document.createElement('span');
    name.textContent = p.name;
    const coords = document.createElement('span');
    coords.className = 'coords';
    coords.textContent = `${Math.round(p.x)}, ${Math.round(p.z)}`;
    const ageEl = document.createElement('span');
    ageEl.className = 'age';
    ageEl.textContent = age > 30 ? 'offline' : `${Math.round(age)}s`;
    const following = p.name === followName;
    const follow = document.createElement('button');
    follow.className = 'pl-btn' + (following ? ' active' : '');
    follow.textContent = '◎';
    follow.title = following
      ? `Stop following ${p.name}`
      : `Follow ${p.name} — the map pans with them (and the permalink carries it)`;
    follow.onclick = (e) => {
      e.stopPropagation();
      setFollow(following ? null : p.name);
    };
    const del = document.createElement('button');
    del.className = 'pl-btn';
    del.textContent = '×';
    del.title = `Remove ${p.name}'s marker everywhere (returns if the account reports again)`;
    del.onclick = async (e) => {
      e.stopPropagation();
      try {
        await removePlayer(p.name);
      } catch (err) {
        $('live-status').textContent = String(err);
      }
    };
    li.append(b, name, coords, ageEl, follow, del);
    li.classList.toggle('stale', age > 30);
    li.classList.toggle('following', following);
    li.title =
      `${p.name} · ${p.dim} · ${Math.round(p.x)}, ${Math.round(p.y)}, ${Math.round(p.z)}` +
      (dimKeyMatchesCurrent(p.dim) ? '' : ' — click to switch dimension');
    li.onclick = () => jumpToPlayer(p);
    list.appendChild(li);
  }
  list.scrollTop = scroll;
}

/** Switches the current world's view to the dimension a player is in.
 *  False when this world has no map data for it. */
function switchToDim(dimKey: string): boolean {
  if (dimKeyMatchesCurrent(dimKey)) return true;
  const norm = (s: string) => s.replace(/^minecraft:/, '');
  const idx = currentWorld()?.dims.findIndex((d) => norm(dimKey) === norm(dimKeyOf(d))) ?? -1;
  if (idx < 0) return false;
  sel.d = idx;
  sel.m = 0;
  sel.layer = 'surface';
  applySelection();
  return true;
}

function jumpToPlayer(p: LivePlayer) {
  if (!switchToDim(p.dim)) {
    $('live-status').textContent = `no map data for ${p.dim}`;
    return;
  }
  map.setView(toLatLng(p.x, p.z), Math.max(map.getZoom(), 0));
}

// ------------------------------------------------------------- follow mode --

/** Glues the view to `name` until the user drags the map (or null to stop).
 *  Persisted in the permalink (`?follow=`), so the URL can be opened on a
 *  second screen or another PC as a self-updating map display. */
function setFollow(name: string | null) {
  followName = name;
  const p = name ? livePlayers.get(name) : undefined;
  if (p) followPan(p, true);
  renderPlayerList();
  writeHash();
}

/** One follow step: hop dimensions with the player, keep them centered. The
 *  zoom is the user's; only a fresh follow (`jump`) pulls it to readable. */
function followPan(p: LivePlayer, jump = false) {
  if (p.name !== followName) return;
  if (!switchToDim(p.dim)) return; // no map data there; keep the last view
  if (jump) {
    map.setView(toLatLng(p.x, p.z), Math.max(map.getZoom(), 0));
  } else {
    // Positions land ~1/s and each pan restarts mid-flight; a linear ease
    // over the full gap chains consecutive pans without rubber-banding.
    map.panTo(toLatLng(p.x, p.z), { animate: true, duration: 1.0, easeLinearity: 1 });
  }
}

/** Roster ages, marker staleness, trail decay, and the dead-socket watchdog. */
function tickLive() {
  // The server heartbeats idle sockets every 25 s, so a minute of silence on
  // an "open" socket means the path is dead in a way no close event reported
  // (NAT/proxy drop). Closing it hands recovery to the backoff reconnect.
  if (liveConnected && liveWs && performance.now() - liveLastRx > 60_000) {
    liveWs.close();
  }
  renderPlayerList();
  const nowP = performance.now();
  for (const p of livePlayers.values()) {
    p.marker?.getElement()?.classList.toggle('stale', (nowP - p.rxAt) / 1000 > 30);
    // Trails otherwise shrink only when new positions arrive: a player who
    // stops reporting would keep a frozen minute of history on screen.
    while (p.trail.length > 0 && nowP - p.trail[0].at > 60_000) p.trail.shift();
    updateTrail(p);
  }
}

// -------------------------------------------------------------- map roots --

async function refreshRootsUi() {
  if (!state.toolsEnabled) {
    $('roots-group').hidden = true;
    return;
  }
  $('roots-group').hidden = false;
  try {
    renderRoots(await fetchRoots());
  } catch (e) {
    console.warn('roots fetch failed', e);
  }
}

function renderRoots(roots: RootJson[]) {
  const list = $('root-list');
  list.innerHTML = '';
  for (const r of roots) {
    const li = document.createElement('li');
    const name = document.createElement('span');
    name.textContent = r.path;
    name.title = `${r.worlds} world(s)${r.origin === 'cli' ? ' · from a --root flag' : ''}`;
    li.appendChild(name);
    if (r.origin === 'config') {
      const del = document.createElement('button');
      del.textContent = '×';
      del.title = 'Remove this root';
      del.onclick = async () => {
        $('root-status').textContent = 'removing…';
        try {
          renderRoots(await removeRoot(r.path));
          $('root-status').textContent = '';
        } catch (e) {
          $('root-status').textContent = String(e);
        }
      };
      li.appendChild(del);
    }
    list.appendChild(li);
  }
}

function setupRoots() {
  refreshRootsUi();
  $('root-add').onclick = async () => {
    const path = inp('root-add-path');
    if (!path) return;
    $('root-status').textContent = 'scanning…';
    try {
      renderRoots(await addRoot(path));
      $('root-status').textContent = 'added';
      ($('root-add-path') as HTMLInputElement).value = '';
      $('fs-browser').hidden = true;
    } catch (e) {
      $('root-status').textContent = String(e);
    }
  };
  $('root-browse').onclick = () => {
    const b = $('fs-browser');
    b.hidden = !b.hidden;
    if (!b.hidden) browseTo(inp('root-add-path') || undefined);
  };
}

// -------------------------------------------------------------- live share --

async function refreshShareUi() {
  ($('share-url') as HTMLInputElement).value = window.location.origin;
  $('share-ingest-dir').textContent = state.ingestDir
    ? `Uploads land in ${state.ingestDir} — a backup per player plus the merged map, all shown as extra roots.`
    : '';
  $('share-lan-note').hidden = state.toolsEnabled;
  $('share-tokens-group').hidden = !state.toolsEnabled;
  if (!state.toolsEnabled) return;
  try {
    renderTokens(await fetchTokens());
  } catch (e) {
    console.warn('tokens fetch failed', e);
  }
}

function renderTokens(tokens: TokenJson[]) {
  const list = $('token-list');
  list.innerHTML = '';
  for (const t of tokens) {
    const li = document.createElement('li');
    const name = document.createElement('span');
    name.textContent = t.player;
    name.title = `token ${t.prefix}…, created ${
      t.createdMs ? new Date(t.createdMs).toLocaleDateString() : 'unknown'
    }`;
    li.appendChild(name);
    const del = document.createElement('button');
    del.textContent = '×';
    del.title = `Revoke ${t.player}'s token (stops working within a second)`;
    del.onclick = async () => {
      $('token-status').textContent = 'revoking…';
      try {
        renderTokens(await revokeToken(t.player));
        $('token-status').textContent = `revoked ${t.player}`;
      } catch (e) {
        $('token-status').textContent = String(e);
      }
    };
    li.appendChild(del);
    list.appendChild(li);
  }
}

function setupShare() {
  refreshShareUi();
  const copy = (id: string, btn: string) => {
    $(btn).onclick = async () => {
      const value = ($(id) as HTMLInputElement).value;
      try {
        await navigator.clipboard.writeText(value);
        $(btn).textContent = 'Copied';
      } catch {
        ($(id) as HTMLInputElement).select();
        document.execCommand('copy');
        $(btn).textContent = 'Copied';
      }
      setTimeout(() => ($(btn).textContent = 'Copy'), 1500);
    };
  };
  copy('share-url', 'share-url-copy');
  copy('token-reveal-value', 'token-copy');
  $('token-generate').onclick = async () => {
    const player = inp('token-player');
    if (!player) return;
    $('token-status').textContent = 'generating…';
    try {
      const out = await generateToken(player);
      renderTokens(out.tokens);
      $('token-reveal').hidden = false;
      $('token-reveal-player').textContent = out.player;
      ($('token-reveal-value') as HTMLInputElement).value = out.token;
      ($('token-player') as HTMLInputElement).value = '';
      $('token-status').textContent = '';
    } catch (e) {
      $('token-status').textContent = String(e);
    }
  };
  // Re-list on every open so tokens minted via the CLI show up too.
  document
    .querySelector<HTMLButtonElement>('.tb-btn[data-panel="panel-share"]')
    ?.addEventListener('click', () => refreshShareUi());
}

async function browseTo(path?: string) {
  try {
    const listing = await fsList(path);
    $('fs-path').textContent = listing.path;
    ($('root-add-path') as HTMLInputElement).value = listing.path;
    const ul = $('fs-list');
    ul.innerHTML = '';
    if (listing.parent) {
      const up = document.createElement('li');
      up.textContent = '↑ ..';
      up.onclick = () => browseTo(listing.parent!);
      ul.appendChild(up);
    }
    for (const d of listing.dirs) {
      const li = document.createElement('li');
      li.textContent = d;
      li.onclick = () => browseTo(`${listing.path === '/' ? '' : listing.path}/${d}`);
      ul.appendChild(li);
    }
  } catch (e) {
    $('root-status').textContent = String(e);
  }
}

// ---------------------------------------------------------- floating panels --

interface PanelState {
  x: number;
  y: number;
  open: boolean;
  collapsed: boolean;
}

const panelStates = new Map<string, PanelState>();
const panelUnavailable = new Set<string>();
const panelOrder: string[] = [];

const clampNum = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, v));

function defaultPanelState(id: string, idx: number): PanelState {
  switch (id) {
    case 'panel-world':
      return { x: 12, y: 54, open: true, collapsed: false };
    case 'panel-wp':
      return { x: Math.max(12, innerWidth - 296), y: 54, open: true, collapsed: false };
    case 'tools-view':
      return {
        x: Math.max(20, Math.round((innerWidth - Math.min(860, innerWidth * 0.94)) / 2)),
        y: 64,
        open: false,
        collapsed: false,
      };
    default:
      return { x: 44 + idx * 28, y: 86 + idx * 28, open: false, collapsed: false };
  }
}

function savePanel(id: string) {
  const st = panelStates.get(id);
  if (st) localStorage.setItem(`xt-panel-${id}`, JSON.stringify(st));
}

function bringToFront(id: string) {
  const i = panelOrder.indexOf(id);
  if (i >= 0) panelOrder.splice(i, 1);
  panelOrder.push(id);
  panelOrder.forEach((pid, idx) => {
    const el = document.getElementById(pid);
    if (el) el.style.zIndex = String(800 + idx);
  });
}

function applyPanelState(p: HTMLElement) {
  const st = panelStates.get(p.id)!;
  p.hidden = !st.open || panelUnavailable.has(p.id);
  p.classList.toggle('collapsed', st.collapsed);
  p.style.left = `${st.x}px`;
  p.style.top = `${st.y}px`;
  const cb = p.querySelector<HTMLButtonElement>('.fp-collapse');
  if (cb) cb.textContent = st.collapsed ? '+' : '–';
  const btn = document.querySelector(`.tb-btn[data-panel="${p.id}"]`);
  btn?.classList.toggle('active', st.open && !panelUnavailable.has(p.id));
}

function setPanelOpen(id: string, open: boolean) {
  const st = panelStates.get(id);
  const p = document.getElementById(id);
  if (!st || !p) return;
  st.open = open;
  if (open) {
    st.collapsed = false;
    bringToFront(id);
  }
  applyPanelState(p);
  savePanel(id);
}

/** Hides a panel and its toolbar button entirely (e.g. no XaeroPlus DBs). */
function setPanelAvailable(id: string, avail: boolean) {
  panelUnavailable[avail ? 'delete' : 'add'](id);
  const btn = document.querySelector(`.tb-btn[data-panel="${id}"]`);
  btn?.classList.toggle('unavailable', !avail);
  const p = document.getElementById(id);
  if (p && panelStates.has(id)) applyPanelState(p);
  else if (p && !avail) p.hidden = true;
}

function makeDraggable(p: HTMLElement, handle: HTMLElement) {
  handle.addEventListener('pointerdown', (e) => {
    if ((e.target as HTMLElement).tagName === 'BUTTON') return;
    const st = panelStates.get(p.id)!;
    const offX = e.clientX - st.x;
    const offY = e.clientY - st.y;
    handle.setPointerCapture(e.pointerId);
    const move = (ev: PointerEvent) => {
      st.x = clampNum(ev.clientX - offX, 0, Math.max(0, innerWidth - 120));
      st.y = clampNum(ev.clientY - offY, 0, Math.max(0, innerHeight - 60));
      p.style.left = `${st.x}px`;
      p.style.top = `${st.y}px`;
    };
    const up = () => {
      handle.removeEventListener('pointermove', move);
      handle.removeEventListener('pointerup', up);
      handle.removeEventListener('pointercancel', up);
      savePanel(p.id);
    };
    handle.addEventListener('pointermove', move);
    handle.addEventListener('pointerup', up);
    // A touch turning into a scroll cancels the pointer; without this the
    // listeners stay and stack up on the next drag.
    handle.addEventListener('pointercancel', up);
    e.preventDefault();
  });
}

function initPanels() {
  const panels = [...document.querySelectorAll<HTMLElement>('.float-panel')];
  panels.forEach((p, idx) => {
    const body = document.createElement('div');
    body.className = 'fp-body';
    while (p.firstChild) body.appendChild(p.firstChild);
    const header = document.createElement('div');
    header.className = 'fp-header';
    const title = document.createElement('span');
    title.className = 'fp-title';
    title.textContent = p.dataset.title ?? p.id;
    const colBtn = document.createElement('button');
    colBtn.className = 'fp-collapse';
    colBtn.title = 'Collapse';
    colBtn.onclick = () => {
      const st = panelStates.get(p.id)!;
      st.collapsed = !st.collapsed;
      applyPanelState(p);
      savePanel(p.id);
    };
    const closeBtn = document.createElement('button');
    closeBtn.textContent = '×';
    closeBtn.title = 'Close';
    closeBtn.onclick = () => setPanelOpen(p.id, false);
    header.append(title, colBtn, closeBtn);
    p.append(header, body);

    let st: PanelState | null = null;
    try {
      st = JSON.parse(localStorage.getItem(`xt-panel-${p.id}`) ?? 'null');
    } catch {
      /* corrupt saved state: use defaults */
    }
    if (!st) st = defaultPanelState(p.id, idx);
    st.x = clampNum(st.x, 0, Math.max(0, innerWidth - 120));
    st.y = clampNum(st.y, 0, Math.max(0, innerHeight - 60));
    panelStates.set(p.id, st);
    applyPanelState(p);
    makeDraggable(p, header);
    p.addEventListener('pointerdown', () => bringToFront(p.id));
    if (st.open) bringToFront(p.id);
  });
  document.querySelectorAll<HTMLButtonElement>('.tb-btn').forEach((btn) => {
    btn.onclick = () => {
      const id = btn.dataset.panel!;
      const st = panelStates.get(id);
      if (!st) return;
      setPanelOpen(id, !st.open);
    };
  });
}

// ----------------------------------------------------------------- sidebar --

function rebuildSidebar() {
  const worldSel = $('world-select') as HTMLSelectElement;
  worldSel.innerHTML = '';
  state.worlds.forEach((w, i) => {
    const opt = document.createElement('option');
    opt.value = String(i);
    opt.textContent = worldLabel(w);
    if (i === sel.w) opt.selected = true;
    worldSel.appendChild(opt);
  });

  const dimList = $('dim-list');
  dimList.innerHTML = '';
  currentWorld()?.dims.forEach((d, i) => {
    const btn = document.createElement('button');
    btn.className = 'dim-btn' + (i === sel.d ? ' active' : '');
    btn.textContent =
      d.dimType === 'overworld'
        ? 'Overworld'
        : d.dimType === 'the_nether'
          ? 'Nether'
          : d.dimType === 'the_end'
            ? 'End'
            : d.folder;
    // The server decodes a real name for custom dimensions; without it every
    // `minecraft$worlds%...` folder renders as another identical "Overworld".
    if (d.label && d.label !== btn.textContent) btn.textContent = d.label;
    if (d.dimId) btn.title = d.dimId;
    btn.onclick = () => {
      // Overworld and Nether are the same world at 1:8. Keeping the raw
      // coordinates would land you 8x away from where you were looking, which
      // is exactly wrong when you are tracing a nether highway.
      const fromType = currentDim()?.dimType;
      const toType = d.dimType;
      const centre = fromLatLng(map.getCenter());
      let scale = 1;
      if (fromType === 'overworld' && toType === 'the_nether') scale = 1 / 8;
      else if (fromType === 'the_nether' && toType === 'overworld') scale = 8;

      sel.d = i;
      sel.m = 0;
      sel.layer = 'surface';
      applySelection();

      if (scale !== 1) {
        // Zoom compensates so the view still covers the same ground. Going to
        // the Nether divides coordinates by 8, so the same ground occupies an
        // eighth of the coordinate span and the view must zoom IN three levels
        // (8 = 2^3); coming back out reverses it.
        const dz = scale > 1 ? -3 : 3;
        map.setView(
          toLatLng(centre.x * scale, centre.z * scale),
          Math.max(-16, Math.min(3, map.getZoom() + dz)),
        );
      }
    };
    dimList.appendChild(btn);
  });

  const mwSel = $('mw-select') as HTMLSelectElement;
  mwSel.innerHTML = '';
  currentDim()?.mws.forEach((m, i) => {
    const opt = document.createElement('option');
    opt.value = String(i);
    opt.textContent = m.display === m.id ? m.id : `${m.display} (${m.id})`;
    if (i === sel.m) opt.selected = true;
    mwSel.appendChild(opt);
  });

  const layerSel = $('layer-select') as HTMLSelectElement;
  layerSel.innerHTML = '';
  const mw = currentDim()?.mws[sel.m];
  const caveLayers = mw?.caveLayers ?? [];
  // The server names these properly: the Integer.MIN_VALUE sentinel means the
  // full column, not "layer -2147483648".
  const caveLabels = mw?.caveLabels ?? [];
  const layers = ['surface', ...caveLayers.map((n) => `cave-${n}`)];
  layers.forEach((l, li) => {
    const opt = document.createElement('option');
    opt.value = l;
    opt.textContent =
      l === 'surface' ? 'Surface' : (caveLabels[li - 1] ?? `Cave layer ${l.slice(5)}`);
    if (l === sel.layer) opt.selected = true;
    layerSel.appendChild(opt);
  });
}

function applySelection() {
  sel.w = Math.min(sel.w, state.worlds.length - 1);
  sel.d = Math.min(sel.d, Math.max(0, (currentWorld()?.dims.length ?? 1) - 1));
  sel.m = Math.min(sel.m, Math.max(0, (currentDim()?.mws.length ?? 1) - 1));
  // A measurement is in one dimension's coordinates; it does not carry over.
  if (measure) {
    if (measure.line) map.removeLayer(measure.line);
    measure = { points: [], line: null };
  }
  rebuildSidebar();
  replaceBaseLayer();
  updateIngestOverlay();
  updatePreviewLayer();
  rebuildHighlightPanel();
  updateNetherToggle();
  updateAtlasUnderlay();
  redrawGuides();
  reloadWaypoints();
  redrawAtlas();
  redrawLivePlayers();
  writeHash();
}

// ------------------------------------------------- XaeroPlus highlight DBs --

/** The merged ingest tree's counterpart of the current *dimension*.
 *
 * Not mergedCounterpart(): that resolves a whole map layer and gives up unless
 * the multiworld and cave layer line up too. A highlight database is keyed by
 * world and dimension alone — it has no notion of a cave layer — so tying it
 * to the tile overlay's stricter match would drop the live rows the moment you
 * looked at a cave. */
function mergedHlCounterpart(): { w: number; d: number } | null {
  const w = currentWorld();
  const folder = currentDim()?.folder;
  if (!w || !folder || w.origin === 'ingestMerged' || w.origin === 'ingestPlayer') return null;
  let wi = state.worlds.findIndex((o) => o.origin === 'ingestMerged' && o.id === w.id);
  if (wi < 0) {
    wi = state.worlds.findIndex(
      (o) => o.origin === 'ingestMerged' && worldKey(o.id) === worldKey(w.id)
    );
  }
  if (wi < 0) return null;
  const di = state.worlds[wi].dims.findIndex((d) => d.folder === folder);
  return di < 0 ? null : { w: wi, d: di };
}

/** Where one overlay's rows can live: the world in view, and — when there is
 *  one — the merged ingest tree companion clients upload into. Both are drawn,
 *  so a tick shows the archive copy *and* whatever arrived a second ago
 *  without anyone having to know the difference. */
function hlSources(): { w: number; d: number; live: boolean }[] {
  const out = [{ w: sel.w, d: sel.d, live: false }];
  const merged = mergedHlCounterpart();
  if (merged) out.push({ w: merged.w, d: merged.d, live: true });
  return out;
}

/** The dimension is in the key: the same world's overlay at another dimension
 *  is a different tile URL, and reusing the key would leave the old one up. */
function hlKey(w: number, d: number, db: string): string {
  return `${w}|${d}|${db}`;
}

/** Rebuilds the overlay list. Only the panel's DOM: the layers themselves are
 *  reconciled by syncHlLayers, because this runs on every roots rescan — which
 *  live ingest triggers for each new layer a mapping client discovers — and
 *  dropping a layer just to recreate an identical one blanks its tiles until
 *  they reload. */
function rebuildHighlightPanel() {
  const list = $('hl-list');
  list.innerHTML = '';
  // Union across sources: an overlay the live tree has but the viewed world
  // does not — the first breadcrumb trail of a session, say — must still get
  // a row, or there is no way to switch it on.
  const own = new Set(
    (currentWorld()?.databases ?? []).filter((d) => !d.includes('Drawing'))
  );
  const dbs = new Set(own);
  const merged = mergedHlCounterpart();
  for (const db of merged ? state.worlds[merged.w].databases : []) {
    if (!db.includes('Drawing')) dbs.add(db); // Drawing has its own format
  }
  const sorted = [...dbs].sort((a, b) => hlLabel(a).localeCompare(hlLabel(b)));
  setPanelAvailable('hl-panel', sorted.length > 0);
  for (const db of sorted) list.appendChild(hlRow(db, !own.has(db)));
  if (sorted.length > 0) list.appendChild(hlResetRow());
  syncHlLayers();
}

/** One overlay's controls: on/off, colour, opacity. */
function hlRow(db: string, liveOnly: boolean): HTMLElement {
  const info = hlInfo(db);
  const wrap = document.createElement('div');
  wrap.className = 'hl-entry';

  const label = document.createElement('label');
  label.className = 'row hl-row';
  label.title = info
    ? `${info.detection}${info.syncable ? '' : ' — not streamed live'}`
    : 'Unknown XaeroPlus database';

  // Declared before the checkbox that shows and hides it.
  const opts = document.createElement('div');
  opts.className = 'hl-opts';
  opts.hidden = !hlEnabled.has(db);

  const cb = document.createElement('input');
  cb.type = 'checkbox';
  cb.checked = hlEnabled.has(db);
  cb.onchange = () => {
    if (cb.checked) hlEnabled.add(db);
    else hlEnabled.delete(db);
    syncHlLayers();
    opts.hidden = !cb.checked;
    writeHash();
  };

  // The swatch *is* the colour picker: one control, and it keeps showing the
  // colour the tiles are actually painted in.
  const swatch = document.createElement('input');
  swatch.type = 'color';
  swatch.className = 'hl-swatch';
  swatch.value = hlColor(db);
  swatch.title = 'Overlay colour';
  // `change`, not `input`: the colour is baked into the PNG server-side, so
  // every distinct value is a re-render. Dragging through a gradient would
  // ask for hundreds of them.
  swatch.onchange = () => {
    setHlColor(db, swatch.value);
  };
  swatch.onclick = (e) => e.stopPropagation(); // the row's label would re-toggle

  const text = document.createElement('span');
  text.className = 'hl-name';
  text.textContent = info?.label ?? hlLabel(db);

  label.append(cb, swatch, text);
  if (liveOnly) {
    const badge = document.createElement('span');
    badge.className = 'hl-badge';
    badge.textContent = 'live';
    badge.title = 'Only in the uploaded map — this world has no copy of its own';
    label.appendChild(badge);
  }
  wrap.appendChild(label);

  const range = document.createElement('input');
  range.type = 'range';
  range.min = '5';
  range.max = '100';
  range.step = '5';
  range.value = String(Math.round(hlOpacity(db) * 100));
  range.title = 'Opacity';
  // Opacity is a layer property, not part of the tile, so this one can follow
  // the drag: nothing is refetched.
  range.oninput = () => {
    const v = +range.value / 100;
    hlOpacities.set(db, v);
    for (const src of hlSources()) hlLayers.get(hlKey(src.w, src.d, db))?.setOpacity(v);
  };
  range.onchange = saveHlPrefs;
  opts.append(range);
  wrap.appendChild(opts);
  return wrap;
}

function hlResetRow(): HTMLElement {
  const row = document.createElement('div');
  row.className = 'hl-reset';
  const btn = document.createElement('button');
  btn.textContent = 'Reset colours & opacity';
  btn.onclick = () => {
    hlOverrides.clear();
    hlOpacities.clear();
    saveHlPrefs();
    rebuildHighlightPanel();
    writeHash();
  };
  row.appendChild(btn);
  return row;
}

function setHlColor(db: string, hex: string) {
  const value = hex.toLowerCase();
  if (value === (hlInfo(db)?.color ?? '').toLowerCase()) hlOverrides.delete(db);
  else hlOverrides.set(db, value);
  saveHlPrefs();
  // The colour is part of the tile URL, so the layer has to be rebuilt — but
  // only this overlay's, and only if it is on screen.
  for (const src of hlSources()) {
    const key = hlKey(src.w, src.d, db);
    const layer = hlLayers.get(key);
    if (!layer) continue;
    map.removeLayer(layer);
    hlLayers.delete(key);
  }
  syncHlLayers();
  writeHash();
}

/** Brings the live layer set in line with the ticked overlays and the current
 *  sources, adding and removing only what actually changed — rebuilding a
 *  layer that did not change blanks its tiles until they reload. */
function syncHlLayers() {
  const sources = hlSources();
  const want = new Map<string, { w: number; d: number; db: string }>();
  for (const db of hlEnabled) {
    for (const src of sources) {
      if (!state.worlds[src.w]?.databases.includes(db)) continue;
      want.set(hlKey(src.w, src.d, db), { w: src.w, d: src.d, db });
    }
  }
  for (const [key, layer] of hlLayers) {
    if (want.has(key)) continue;
    map.removeLayer(layer);
    hlLayers.delete(key);
  }
  for (const [key, o] of want) {
    if (hlLayers.has(key)) continue;
    hlLayers.set(key, addHlLayer(o.w, o.d, o.db));
  }
}

function addHlLayer(w: number, d: number, db: string): L.TileLayer {
  const color = hlColor(db).replace('#', '');
  const layer = L.tileLayer(`./hl/${w}/${encodeURIComponent(db)}/${d}/{z}/{x}/{y}?c=${color}`, {
    tileSize: 512,
    minZoom: -16,
    maxZoom: 3,
    maxNativeZoom: 0,
    minNativeZoom: -16,
    noWrap: true,
    keepBuffer: 2,
    errorTileUrl: TRANSPARENT_TILE,
    // Chunk-rects must stay crisp over the pixelated base at overzoom.
    className: 'pixelated',
    opacity: hlOpacity(db),
    zIndex: 5,
  });
  layer.addTo(map);
  return layer;
}

function wireEvents() {
  ($('world-select') as HTMLSelectElement).onchange = (e) => {
    sel.w = +(e.target as HTMLSelectElement).value;
    sel.d = 0;
    sel.m = 0;
    sel.layer = 'surface';
    applySelection();
  };
  ($('mw-select') as HTMLSelectElement).onchange = (e) => {
    sel.m = +(e.target as HTMLSelectElement).value;
    sel.layer = 'surface';
    applySelection();
  };
  ($('layer-select') as HTMLSelectElement).onchange = (e) => {
    sel.layer = (e.target as HTMLSelectElement).value;
    applySelection();
  };
  $('toggle-waypoints').onchange = redrawWaypoints;
  $('toggle-archived').onchange = redrawWaypoints;
  $('vault-sync-btn').onclick = async () => {
    const status = $('vault-status');
    status.textContent = 'syncing…';
    try {
      const r = await vaultSync();
      status.textContent = `${r.seen} live synced · ${r.added} new · ${r.newly_archived} newly archived · ${r.archived_total} archived total`;
      await reloadWaypoints();
    } catch (e) {
      status.textContent = String(e);
    }
  };
  $('toggle-guides').onchange = redrawGuides;
  $('toggle-grid').onchange = toggleGrid;
  $('toggle-measure').onchange = toggleMeasure;
  const liveOverlayCb = $('toggle-live-overlay') as HTMLInputElement;
  liveOverlayCb.checked = localStorage.getItem('xt-live-overlay') !== '0'; // default on
  liveOverlayCb.onchange = () => {
    localStorage.setItem('xt-live-overlay', liveOverlayCb.checked ? '1' : '0');
    updateIngestOverlay();
  };
  updateIngestOverlay();
  const previewCb = $('toggle-live-preview') as HTMLInputElement;
  previewCb.checked = localStorage.getItem('xt-live-preview') !== '0'; // default on
  previewCb.onchange = () => {
    localStorage.setItem('xt-live-preview', previewCb.checked ? '1' : '0');
    updatePreviewLayer();
  };
  updatePreviewLayer();
  const roofCb = $('toggle-roof') as HTMLInputElement;
  const applyRoof = () => {
    $('roof-opts').hidden = !roofCb.checked;
    localStorage.setItem('xt-roof', roofCb.checked ? '1' : '0');
    // Every layer drawn from region data carries the view in its URL.
    replaceBaseLayer();
    updateNetherToggle();
    updateIngestOverlay();
    writeHash();
  };
  roofCb.onchange = applyRoof;
  $('roof-obsidian').onchange = applyRoof;
  $('roof-snow').onchange = applyRoof;
  // The checkbox was settled in boot() before the first layer build (hash
  // link first, remembered choice second), so no rebuild is needed here.
  $('roof-opts').hidden = !roofCb.checked;
  $('toggle-nether').onchange = updateNetherToggle;
  $('toggle-atlas').onchange = () => toggleAtlas(true);
  $('atlas-refresh').onclick = refreshAtlas;
  $('toggle-atlas-under').onchange = () => {
    localStorage.setItem(
      'xt-atlas-under',
      ($('toggle-atlas-under') as HTMLInputElement).checked ? '1' : '0'
    );
    updateAtlasUnderlay();
    writeHash();
  };
  if (localStorage.getItem('xt-atlas-under') === '1') {
    ($('toggle-atlas-under') as HTMLInputElement).checked = true;
    updateAtlasUnderlay();
  }
  ($('atlas-filter') as HTMLInputElement).oninput = (e) => {
    atlasFilter = (e.target as HTMLInputElement).value;
    redrawAtlas();
  };
  if (localStorage.getItem('xt-atlas') === '1') {
    ($('toggle-atlas') as HTMLInputElement).checked = true;
  }
  if (($('toggle-atlas') as HTMLInputElement).checked) {
    // Restored preference or `?atlas=1` from a shared link: show what is
    // already stored, but never reach off-box without a click.
    toggleAtlas(false);
  }
  ($('wp-search') as HTMLInputElement).oninput = (e) => {
    wpFilter = (e.target as HTMLInputElement).value.toLowerCase();
    redrawWaypoints();
  };
  $('goto-btn').onclick = () => {
    const xs = ($('goto-x') as HTMLInputElement).value.trim();
    const zs = ($('goto-z') as HTMLInputElement).value.trim();
    if (!xs || !zs) return; // `+''` is 0, which would silently jump to the axis
    const x = +xs;
    const z = +zs;
    if (Number.isFinite(x) && Number.isFinite(z)) {
      map.setView(toLatLng(x, z), Math.max(map.getZoom(), 0));
    }
  };
}

// ------------------------------------------------------------------- tools --

/** Starts a tool job and polls it to completion, narrating into `status`. */
async function runToolJob(
  start: () => Promise<{ job: number }>,
  status: HTMLElement
): Promise<unknown> {
  status.textContent = 'starting…';
  const { job } = await start();
  for (;;) {
    await new Promise((r) => setTimeout(r, 800));
    const s = await fetchJob(job);
    if (s.state === 'running') {
      status.textContent = `working… ${(s.elapsedMs / 1000).toFixed(0)}s`;
      continue;
    }
    if (s.state === 'failed') throw new Error(s.error ?? 'job failed');
    status.textContent = `done in ${(s.elapsedMs / 1000).toFixed(1)}s`;
    return s.result;
  }
}

const inp = (id: string) => ($(id) as HTMLInputElement).value.trim();

function renderMergeReport(r: MergeReport) {
  const rows = r.units
    .map(
      (u) =>
        `<tr><td>${escapeHtml(u.world)}</td><td>${escapeHtml(u.dim)}${
          u.cave != null ? ` (cave ${u.cave})` : ''
        }</td><td>${escapeHtml(u.mw)}</td><td>${u.only_a}</td><td>${u.only_b}</td><td>${
          u.conflicts
        }</td></tr>` +
        u.merge_errors
          .map((e) => `<tr><td colspan="6" class="err">${escapeHtml(e)}</td></tr>`)
          .join('')
    )
    .join('');
  const totals = r.units.reduce(
    (t, u) => ({
      a: t.a + u.only_a,
      b: t.b + u.only_b,
      c: t.c + u.conflicts,
    }),
    { a: 0, b: 0, c: 0 }
  );
  const aliases = r.suggested_aliases.length
    ? `<p class="warn">Split worlds detected: ${r.suggested_aliases
        .map(([a, b]) => `${escapeHtml(a)} ⇄ ${escapeHtml(b)}`)
        .join(', ')} — tick “auto-pair split worlds” to merge them as one.</p>`
    : '';
  const only = r.only_worlds.length
    ? `<p class="muted">only on one side (copied as-is): ${r.only_worlds
        .map(escapeHtml)
        .join(', ')}</p>`
    : '';
  $('mg-report').innerHTML =
    `<p><b>${r.applied ? 'APPLIED' : 'Dry run'}</b> — ${r.world_pairs.length} world pair(s), ` +
    `${totals.a + totals.b} regions copied, ${totals.c} tile-merged, ` +
    `${r.dbs.length} DB(s), ${r.waypoint_files_merged} waypoint file(s), ${r.aux_copied} aux files.</p>` +
    aliases +
    only +
    (rows
      ? `<table><thead><tr><th>world</th><th>dim</th><th>map</th><th>only A</th><th>only B</th><th>conflicts</th></tr></thead><tbody>${rows}</tbody></table>`
      : '');
}

function renderDbReport(r: DbMergeReport) {
  const rows = r.tables
    .map(
      (t) =>
        `<tr><td>${escapeHtml(t.table)}</td><td>${t.dest_rows_before}</td><td>${
          t.source_rows
        }</td><td>${t.overlap}</td><td><b>${t.dest_rows_after}</b></td></tr>`
    )
    .join('');
  $('db-report').innerHTML =
    `<p><b>${r.applied ? 'APPLIED' : 'Dry run'}</b> — ${r.tables.length} table(s). ` +
    `Overlaps keep the oldest foundTime.</p>` +
    `<table><thead><tr><th>table</th><th>base</th><th>+ sources</th><th>overlap</th><th>result</th></tr></thead><tbody>${rows}</tbody></table>`;
}

function setupTools() {
  if (!state.toolsEnabled) {
    setPanelAvailable('tools-view', false);
    return;
  }
  const roots = [...new Set(state.worlds.map((w) => w.root))];
  $('dl-roots').innerHTML = roots
    .map((r) => `<option value="${escapeHtml(r)}"></option>`)
    .join('');
  const dbPaths = state.worlds.flatMap((w) =>
    w.mapPath ? w.databases.map((d) => `${w.mapPath}/${d}`) : []
  );
  $('dl-dbs').innerHTML = dbPaths
    .map((p) => `<option value="${escapeHtml(p)}"></option>`)
    .join('');

  const runMerge = async (apply: boolean) => {
    const status = $('mg-status');
    ($('mg-dry') as HTMLButtonElement).disabled = true;
    ($('mg-apply') as HTMLButtonElement).disabled = true;
    try {
      const req = {
        a: inp('mg-a'),
        b: inp('mg-b'),
        out: inp('mg-out'),
        apply,
        prefer: ($('mg-prefer') as HTMLSelectElement).value,
        autoAlias: ($('mg-autoalias') as HTMLInputElement).checked,
        aliases: [] as [string, string][],
      };
      const report = (await runToolJob(() => toolsMerge(req), status)) as MergeReport;
      renderMergeReport(report);
      // Apply unlocks only after a clean dry run of the same inputs.
      ($('mg-apply') as HTMLButtonElement).disabled =
        apply || report.units.some((u) => u.merge_errors.length > 0);
    } catch (e) {
      status.textContent = String(e);
    } finally {
      ($('mg-dry') as HTMLButtonElement).disabled = false;
    }
  };
  $('mg-dry').onclick = () => runMerge(false);
  $('mg-apply').onclick = () => runMerge(true);
  for (const id of ['mg-a', 'mg-b', 'mg-out', 'mg-prefer', 'mg-autoalias']) {
    $(id).addEventListener('input', () => {
      ($('mg-apply') as HTMLButtonElement).disabled = true;
    });
  }

  const runDb = async (apply: boolean) => {
    const status = $('db-status');
    ($('db-dry') as HTMLButtonElement).disabled = true;
    ($('db-apply') as HTMLButtonElement).disabled = true;
    try {
      const req = {
        base: inp('db-base'),
        sources: ($('db-sources') as HTMLTextAreaElement).value
          .split('\n')
          .map((s) => s.trim())
          .filter(Boolean),
        out: inp('db-out'),
        apply,
      };
      const report = (await runToolJob(() => toolsDbMerge(req), status)) as DbMergeReport;
      renderDbReport(report);
      ($('db-apply') as HTMLButtonElement).disabled = apply;
    } catch (e) {
      status.textContent = String(e);
    } finally {
      ($('db-dry') as HTMLButtonElement).disabled = false;
    }
  };
  $('db-dry').onclick = () => runDb(false);
  $('db-apply').onclick = () => runDb(true);
  for (const id of ['db-base', 'db-sources', 'db-out']) {
    $(id).addEventListener('input', () => {
      ($('db-apply') as HTMLButtonElement).disabled = true;
    });
  }
}

/** First run, nothing found: a folder picker in the page, because the person
 *  double-clicking the binary cannot be sent back to a terminal for --root. */
function renderWelcome() {
  const card = document.createElement('div');
  card.className = 'welcome';
  const inner = document.createElement('div');
  inner.className = 'welcome-card';
  inner.innerHTML = `
    <h1>XaeroTools</h1>
    <p>No Xaero's World Map data found on this computer yet.</p>`;
  card.appendChild(inner);
  document.body.replaceChildren(card);
  if (!state.toolsEnabled) {
    inner.insertAdjacentHTML(
      'beforeend',
      `<p>The host has not added any map folders yet — ask them to add one.</p>`
    );
    return;
  }
  inner.insertAdjacentHTML(
    'beforeend',
    `
    <p>Pick the folder your maps live in — a <code>.minecraft</code> folder, a
    launcher instance, or any backup copy. Folders are only read, never changed.</p>
    <div class="welcome-row">
      <input id="welcome-path" placeholder="path to .minecraft (or a backup folder)" spellcheck="false">
      <button id="welcome-add">Add folder</button>
    </div>
    <p id="welcome-status"></p>
    <div class="welcome-fs">
      <div id="welcome-fs-path"></div>
      <ul id="welcome-fs-list"></ul>
    </div>`
  );
  const status = $('welcome-status');
  const pathInput = $('welcome-path') as HTMLInputElement;
  const tryAdd = async (path: string) => {
    if (!path) return;
    status.textContent = 'scanning…';
    try {
      await addRoot(path);
      // The server canonicalizes paths, so judge success by what it now
      // discovers rather than by matching the string we sent.
      if ((await fetchState()).worlds.length > 0) {
        location.reload();
        return;
      }
      // A folder with no maps in it would sit uselessly in the config —
      // undo, explain, let them pick again.
      await removeRoot(path).catch(() => {});
      status.textContent =
        'No Xaero maps in that folder. Pick the folder that contains a "xaero" folder — usually .minecraft.';
    } catch (e) {
      status.textContent = String(e);
    }
  };
  $('welcome-add').onclick = () => tryAdd(pathInput.value.trim());
  pathInput.onkeydown = (e) => {
    if (e.key === 'Enter') tryAdd(pathInput.value.trim());
  };
  const browse = async (path?: string) => {
    try {
      const listing = await fsList(path);
      pathInput.value = listing.path;
      $('welcome-fs-path').textContent = listing.path;
      const ul = $('welcome-fs-list');
      ul.innerHTML = '';
      if (listing.parent) {
        const up = document.createElement('li');
        up.textContent = '↑ ..';
        up.onclick = () => browse(listing.parent!);
        ul.appendChild(up);
      }
      for (const d of listing.dirs) {
        const li = document.createElement('li');
        li.textContent = d;
        li.onclick = () => browse(`${listing.path === '/' ? '' : listing.path}/${d}`);
        ul.appendChild(li);
      }
    } catch (e) {
      status.textContent = String(e);
    }
  };
  browse();
}

async function boot() {
  state = await fetchState();
  if (state.worlds.length === 0) {
    renderWelcome();
    return;
  }
  // Before readHash: a link's colours are the sharper intent and win over
  // whatever this browser remembered.
  loadHlPrefs();
  setupMap();
  initPanels();
  const fromHash = readHash();
  // View and roof choice are settled before the first layer build: building
  // at the default view first would fire a burst of spawn-tile requests
  // (the most expensive renders there are) that the setView then abandons,
  // and restoring the roof afterwards would rebuild every layer a second
  // time. A hash link wins over the remembered roof choice.
  if (fromHash) {
    sel = fromHash.sel;
    map.setView(toLatLng(fromHash.x, fromHash.z), fromHash.zoom);
  }
  const roofCb = $('toggle-roof') as HTMLInputElement;
  if (!roofCb.checked && localStorage.getItem('xt-roof') === '1') roofCb.checked = true;
  applySelection();
  // A permalink pasted into the open tab. Our own writeHash goes through
  // replaceState, which never fires this, so it only runs for outside edits.
  addEventListener('hashchange', () => {
    const h = readHash();
    if (!h) return;
    sel = h.sel;
    applySelection();
    map.setView(toLatLng(h.x, h.z), h.zoom);
    // The link may carry ?atlas=1: show stored POIs, never fetch remotely.
    if (($('toggle-atlas') as HTMLInputElement).checked) toggleAtlas(false);
  });
  wireEvents();
  setupTools();
  setupRoots();
  setupShare();
  if (localStorage.getItem('xt-trails') === '1') {
    ($('toggle-trails') as HTMLInputElement).checked = true;
  }
  $('toggle-trails').onchange = () => {
    localStorage.setItem(
      'xt-trails',
      ($('toggle-trails') as HTMLInputElement).checked ? '1' : '0'
    );
    redrawLivePlayers();
  };
  connectLive();
  setInterval(tickLive, 5000);
}

boot().catch((e) => {
  document.body.innerHTML = `<p style="padding:2em">Failed to start: ${escapeHtml(String(e))}</p>`;
});
