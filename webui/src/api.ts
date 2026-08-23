export interface MwJson {
  id: string;
  display: string;
  caveLayers: number[];
  /** Display name per caveLayers entry, same order. */
  caveLabels?: string[];
}

export interface DimJson {
  folder: string;
  dimType: string | null;
  /** Decoded resource key, e.g. "minecraft:worlds/2b2t/2b2t_1". */
  dimId?: string | null;
  /** Human-readable dimension name; distinguishes custom dimensions. */
  label?: string;
  mws: MwJson[];
}

export interface WorldJson {
  id: string;
  root: string;
  /** The world's world-map folder on disk (tools tab path pickers). */
  mapPath: string | null;
  /** A normal root, or one of the ingest-managed trees: the shared merged
   *  upload map, or one uploader's verbatim backup (`player` set). */
  origin?: 'user' | 'ingestMerged' | 'ingestPlayer';
  player?: string | null;
  dims: DimJson[];
  databases: string[];
  hasWaypoints: boolean;
}

/** One locally mirrored Atlas tile pyramid (served under /atlas/). */
export interface AtlasSetJson {
  dim: string;
  url: string;
  /** World coordinate of the pyramid's top-left corner (blocks). */
  originX: number;
  originZ: number;
  /** Blocks covered by one 256px tile at zMax. */
  bptMax: number;
  zMin: number;
  zMax: number;
}

export interface StateJson {
  worlds: WorldJson[];
  atlas: AtlasSetJson[];
  /** Mirror root behind /atlas/, for the "how do I mirror this" hint. */
  atlasDir?: string | null;
  /** Merge tools are local-only; false when served with --lan. */
  toolsEnabled: boolean;
  /** Where region uploads land (per-player backups + merged tree). */
  ingestDir?: string;
}

// -------------------------------------------------------------------- atlas --

/** Which tiles of one mirrored pyramid are actually on disk (`/api/atlas/index`). */
export interface AtlasIndexJson {
  set: string;
  zMin: number;
  zMax: number;
  /** Tile rows/columns per side at each level, zMin..zMax. */
  sides: number[];
  /** Presence bits, levels concatenated, row-major, MSB first, base64. */
  bits: string;
  /** Tiles present on disk, and how many a complete mirror would hold. */
  tiles: number;
  expected: number;
}

/** Null when the server does not implement the index (older builds 404). */
export async function fetchAtlasIndex(set: string): Promise<AtlasIndexJson | null> {
  try {
    const res = await fetch(`./api/atlas/index?set=${encodeURIComponent(set)}`);
    return res.ok ? await res.json() : null;
  } catch {
    return null;
  }
}

/** One Atlas POI, as the viewer renders it. */
export interface AtlasLocation {
  name: string;
  description: string;
  tags: string | null;
  dimension: number;
  x: number;
  y: number;
  z: number;
  wiki: string | null;
  videoUrl: string | null;
  dateAddedUtc: string;
}

/** The POI snapshot the server keeps on disk so upstream is hit at most once. */
export interface AtlasStoreJson {
  fetchedMs: number;
  count: number;
  locations: AtlasLocation[];
}

/** Null when nothing has been downloaded yet, or the server predates the store. */
export async function fetchAtlasStore(): Promise<AtlasStoreJson | null> {
  try {
    const res = await fetch('./api/atlas/locations');
    return res.ok ? await res.json() : null;
  } catch {
    return null;
  }
}

/** True when the server took the snapshot; false means "keep it in this browser". */
export async function putAtlasStore(locations: AtlasLocation[]): Promise<boolean> {
  try {
    const res = await fetch('./api/atlas/locations', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(locations),
    });
    return res.ok;
  } catch {
    return false;
  }
}

// ------------------------------------------------------------ merge tools --

export interface MergeUnitReport {
  world: string;
  dim: string;
  mw: string;
  cave: number | null;
  only_a: number;
  only_b: number;
  conflicts: number;
  merge_errors: string[];
}

export interface TableMergeReport {
  table: string;
  source_rows: number;
  dest_rows_before: number;
  overlap: number;
  dest_rows_after: number;
}

export interface DbMergeReport {
  dest: string;
  sources: string[];
  tables: TableMergeReport[];
  applied: boolean;
}

export interface MergeReport {
  applied: boolean;
  world_pairs: [string, string][];
  only_worlds: string[];
  units: MergeUnitReport[];
  aux_copied: number;
  waypoint_files_merged: number;
  dbs: DbMergeReport[];
  suggested_aliases: [string, string][];
}

export interface JobStatus {
  state: 'running' | 'done' | 'failed';
  elapsedMs: number;
  result?: unknown;
  error?: string;
}

async function postJson(url: string, body: object): Promise<{ job: number }> {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(await res.text());
  return res.json();
}

export interface MergeRequest {
  a: string;
  b: string;
  out: string;
  apply: boolean;
  prefer: string;
  autoAlias: boolean;
  aliases: [string, string][];
}

export const toolsMerge = (req: MergeRequest) => postJson('./api/tools/merge', req);

export interface DbMergeRequest {
  base: string;
  sources: string[];
  out: string;
  apply: boolean;
}

export const toolsDbMerge = (req: DbMergeRequest) => postJson('./api/tools/dbmerge', req);

export async function fetchJob(id: number): Promise<JobStatus> {
  const res = await fetch(`./api/jobs/${id}`);
  if (!res.ok) throw new Error(`job: ${res.status}`);
  return res.json();
}

export interface WaypointJson {
  name: string;
  initials: string;
  x: number;
  y: number | null;
  z: number;
  color: number;
  rgb: string;
  disabled: boolean;
  purpose: number;
  set: string;
  /** Deleted in game, preserved by the vault. */
  archived: boolean;
}

export interface VaultSyncReport {
  seen: number;
  added: number;
  revived: number;
  newly_archived: number;
  total: number;
  archived_total: number;
}

export async function vaultSync(): Promise<VaultSyncReport> {
  const res = await fetch('./api/vault/sync', { method: 'POST' });
  if (!res.ok) throw new Error(`vault sync: ${res.status} ${await res.text()}`);
  return (await res.json()).report;
}

export interface WaypointFileJson {
  dimFolder: string;
  dimKey: string | null;
  file: string;
  waypoints: WaypointJson[];
}

export async function fetchState(): Promise<StateJson> {
  const res = await fetch('./api/state');
  if (!res.ok) throw new Error(`state: ${res.status}`);
  return res.json();
}

// -------------------------------------------------------------- live mode --

export interface PosEvent {
  type: 'pos';
  player: string;
  dim: string;
  x: number;
  y: number;
  z: number;
  yaw: number;
  /** Server receive time, unix ms. */
  t: number;
}

export interface TilesEvent {
  type: 'tiles';
  w: number;
  d: number;
  m: number;
  layer: string;
  /** Changed regions, or null = assume everything changed. */
  regions: [number, number][] | null;
  /** True when overzoom (z<0) stamps were bumped too. */
  deep: boolean;
  v: number;
}

export interface DbEvent {
  type: 'db';
  w: number;
  db: string;
  v: number;
}

export interface HelloEvent {
  type: 'hello';
  players: PosEvent[];
  v: number;
}

export interface StateChangedEvent {
  type: 'state';
  v: number;
}

/** A marker was removed via DELETE /api/players; drop it from the roster. */
export interface PlayerRemovedEvent {
  type: 'player_removed';
  player: string;
  v: number;
}

/** The live-preview canvas changed inside these regions (dim resource key). */
export interface PreviewEvent {
  type: 'preview';
  dim: string;
  regions: [number, number][];
  v: number;
}

/** This socket missed broadcasts (server-side lag): refresh layers in place. */
export interface ResyncEvent {
  type: 'resync';
}

/** Idle-socket keepalive; carries nothing, its arrival is the payload. */
export interface HbEvent {
  type: 'hb';
}

export type LiveEvent =
  | PosEvent
  | TilesEvent
  | DbEvent
  | HelloEvent
  | StateChangedEvent
  | PlayerRemovedEvent
  | PreviewEvent
  | ResyncEvent
  | HbEvent;

/** Removes a live player marker for every viewer (it returns the moment that
 *  account reports again). An already-gone player (404) is not an error. */
export async function removePlayer(player: string): Promise<void> {
  const res = await fetch('./api/players', {
    method: 'DELETE',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ player }),
  });
  if (!res.ok && res.status !== 404) throw new Error(await res.text());
}

// ------------------------------------------------------------- root config --

export interface RootJson {
  path: string;
  /** "cli" roots come from --root flags; only "config" roots are removable. */
  origin: 'cli' | 'config';
  worlds: number;
}

export interface FsListJson {
  path: string;
  parent: string | null;
  dirs: string[];
}

async function requestJson<T>(url: string, method: string, body?: object): Promise<T> {
  const res = await fetch(url, {
    method,
    headers: body ? { 'Content-Type': 'application/json' } : undefined,
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) throw new Error(await res.text());
  return res.json();
}

export const fetchRoots = () => requestJson<RootJson[]>('./api/roots', 'GET');
export const addRoot = (path: string) => requestJson<RootJson[]>('./api/roots', 'POST', { path });
export const removeRoot = (path: string) =>
  requestJson<RootJson[]>('./api/roots', 'DELETE', { path });

/** One ingest token as the UI may see it — never the token itself. */
export interface TokenJson {
  player: string;
  /** First 8 chars, for telling tokens apart. */
  prefix: string;
  createdMs: number;
}

/** POST /api/tokens: the full token appears here once and is never listed again. */
export interface TokenGeneratedJson {
  player: string;
  token: string;
  tokens: TokenJson[];
}

export const fetchTokens = () => requestJson<TokenJson[]>('./api/tokens', 'GET');
export const generateToken = (player: string) =>
  requestJson<TokenGeneratedJson>('./api/tokens', 'POST', { player });
export const revokeToken = (player: string) =>
  requestJson<TokenJson[]>('./api/tokens', 'DELETE', { player });
export const fsList = (path?: string) =>
  requestJson<FsListJson>(
    path ? `./api/fs/list?path=${encodeURIComponent(path)}` : './api/fs/list',
    'GET'
  );

export async function fetchWaypoints(world: number): Promise<WaypointFileJson[]> {
  const res = await fetch(`./api/waypoints/${world}`);
  if (!res.ok) throw new Error(`waypoints: ${res.status}`);
  return res.json();
}
