//! Live chunk preview — terrain the companion client has *seen* but Xaero has
//! not yet saved to disk.
//!
//! Xaero's World Map holds a freshly-mapped region dirty in memory for up to
//! `SAVE_TIME` (60 s) before writing it, so the region-upload pipeline is
//! authoritative but never instant. This channel closes that gap: the addon
//! computes a coarse 16x16 color summary per loaded chunk straight from the
//! game world and POSTs batches of them; the server keeps a bounded in-memory
//! canvas per dimension and serves it as its own tile layer, which the viewer
//! draws above the map. When the real region file arrives via
//! `/ingest/v1/region`, the chunks it covers are dropped from the canvas — the
//! authoritative imagery replaces the preview.
//!
//! Everything here is memory-only: a restart starts blank, and nothing is
//! ever written to disk.
//!
//! Batch wire format (little-endian), Content-Type application/octet-stream:
//!   "XTPV" u8 version=1 u16 count, then count x { i32 cx, i32 cz,
//!   512 bytes: 16x16 RGB565 pixels, row-major (index = z*16 + x) }.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::live::{bucket_allow, normalize_dim, Bucket};
use crate::{encode_png, now_ms, AppState, TILE};

/// Chunks per batch (a client scan pass sends at most this many).
pub(crate) const BATCH_MAX: usize = 256;
/// 4 magic + 1 version + 2 count + BATCH_MAX entries.
pub(crate) const PREVIEW_BODY_MAX: usize = 7 + BATCH_MAX * (8 + 512);
/// Total chunks kept per dimension; oldest-inserted evict first. At ~520 B a
/// chunk this bounds a dimension's canvas to ~160 MB, and a chunk that gets
/// covered by a real region upload leaves long before the cap matters.
const CANVAS_CAP: usize = 300_000;
/// Uploads per second per player (each carries up to BATCH_MAX chunks).
const RATE_PER_SEC: f64 = 4.0;
const RATE_BURST: f64 = 8.0;
/// |cx|/|cz| cap: the world border in chunks, with slack.
const CHUNK_COORD_CAP: i32 = 2_600_000;
/// Distinct dimension canvases held at once. A dimension is any well-formed
/// `namespace:path`, so without a cap one client could allocate canvases until
/// memory ran out; sixteen covers the vanilla three plus any modded server.
const MAX_DIMS: usize = 16;
/// Evicted chunks leave their key in `order`; past this many ghosts it is
/// rebuilt from the live set instead of growing for the rest of the session.
const GHOST_SLACK: usize = 65_536;

/// One chunk's preview: 16x16 RGB565 plus its average color for far zooms.
struct ChunkPix {
    pix: Box<[u8; 512]>,
    avg: [u8; 3],
}

#[derive(Default)]
struct DimCanvas {
    chunks: HashMap<(i32, i32), ChunkPix>,
    /// Insertion order for the eviction cap (pushed on first insert only).
    order: VecDeque<(i32, i32)>,
    /// Bumped on every mutation; the preview tiles' cache validator.
    gen: u64,
}

impl DimCanvas {
    /// A canvas whose generation can never repeat one from an earlier run:
    /// browsers revalidate preview tiles by ETag and keep them across server
    /// restarts, so a counter that restarted at zero would let a stale tile
    /// from the last session answer a fresh request with 304.
    fn fresh() -> DimCanvas {
        DimCanvas {
            gen: now_ms() << 16,
            ..Default::default()
        }
    }
}

pub(crate) struct PreviewState {
    dims: Mutex<HashMap<String, DimCanvas>>,
    rate: Mutex<HashMap<String, Bucket>>,
}

impl PreviewState {
    pub(crate) fn new() -> PreviewState {
        PreviewState {
            dims: Mutex::new(HashMap::new()),
            rate: Mutex::new(HashMap::new()),
        }
    }

    /// Total chunks held across dimensions (diagnostics).
    pub(crate) fn chunk_count(&self) -> usize {
        crate::lock_ok(&self.dims)
            .values()
            .map(|c| c.chunks.len())
            .sum()
    }

    /// Drops every preview chunk covered by region (rx, rz) of `dim_key` —
    /// called when the authoritative region file lands via ingest.
    pub(crate) fn evict_region(&self, dim_key: &str, rx: i32, rz: i32) -> bool {
        let mut dims = crate::lock_ok(&self.dims);
        let Some(canvas) = dims.get_mut(dim_key) else {
            return false;
        };
        let mut removed = false;
        for cx in rx * 32..rx * 32 + 32 {
            for cz in rz * 32..rz * 32 + 32 {
                removed |= canvas.chunks.remove(&(cx, cz)).is_some();
            }
        }
        if removed {
            canvas.gen += 1;
            // The keys just removed stay in `order` (a re-previewed chunk is
            // pushed again); prune them once they outnumber the slack rather
            // than on every eviction.
            if canvas.order.len() > canvas.chunks.len() + GHOST_SLACK {
                let DimCanvas { chunks, order, .. } = &mut *canvas;
                order.retain(|k| chunks.contains_key(k));
            }
        }
        removed
    }
}

fn rgb565_to_rgb(v: u16) -> [u8; 3] {
    let r = ((v >> 11) & 0x1F) as u32;
    let g = ((v >> 5) & 0x3F) as u32;
    let b = (v & 0x1F) as u32;
    [
        ((r * 255 + 15) / 31) as u8,
        ((g * 255 + 31) / 63) as u8,
        ((b * 255 + 15) / 31) as u8,
    ]
}

/// One parsed batch entry: chunk coords plus its 16x16 RGB565 pixels.
type BatchEntry = (i32, i32, Box<[u8; 512]>);

/// Parses one batch. Pure; unit-tested. Returns (cx, cz, pixels) entries.
pub(crate) fn parse_batch(body: &[u8]) -> Result<Vec<BatchEntry>, &'static str> {
    if body.len() < 7 || &body[0..4] != b"XTPV" {
        return Err("not a preview batch");
    }
    if body[4] != 1 {
        return Err("unknown preview version");
    }
    let count = u16::from_le_bytes([body[5], body[6]]) as usize;
    if count == 0 || count > BATCH_MAX {
        return Err("bad chunk count");
    }
    if body.len() != 7 + count * 520 {
        return Err("length does not match count");
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = 7 + i * 520;
        let cx = i32::from_le_bytes(body[off..off + 4].try_into().unwrap());
        let cz = i32::from_le_bytes(body[off + 4..off + 8].try_into().unwrap());
        if cx.abs() > CHUNK_COORD_CAP || cz.abs() > CHUNK_COORD_CAP {
            return Err("chunk coordinates out of range");
        }
        let mut pix = Box::new([0u8; 512]);
        pix.copy_from_slice(&body[off + 8..off + 520]);
        out.push((cx, cz, pix));
    }
    Ok(out)
}

#[derive(serde::Deserialize)]
pub(crate) struct PreviewQuery {
    dim: String,
}

pub(crate) async fn ingest_preview(
    State(st): State<Arc<AppState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
    Query(q): Query<PreviewQuery>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let declared = headers.get("x-xt-player").and_then(|v| v.to_str().ok());
    let player = match crate::live::ingest_player(&st, &headers, peer, declared).await {
        Ok(a) => a.player,
        Err(resp) => return resp,
    };
    let Some(dim) = normalize_dim(&q.dim) else {
        return (StatusCode::BAD_REQUEST, "unrecognized dim").into_response();
    };
    {
        let mut rate = crate::lock_ok(&st.preview.rate);
        let bucket = rate
            .entry(player)
            .or_insert_with(|| Bucket::new(RATE_BURST, now_ms()));
        if !bucket_allow(bucket, now_ms(), RATE_PER_SEC, RATE_BURST) {
            return (StatusCode::TOO_MANY_REQUESTS, "rate limited").into_response();
        }
    }
    let entries = match parse_batch(&body) {
        Ok(e) => e,
        Err(msg) => return (StatusCode::BAD_REQUEST, msg).into_response(),
    };

    let mut regions: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
    {
        let mut dims = crate::lock_ok(&st.preview.dims);
        if !dims.contains_key(&dim) && dims.len() >= MAX_DIMS {
            return (
                StatusCode::BAD_REQUEST,
                "too many preview dimensions on this server",
            )
                .into_response();
        }
        let canvas = dims.entry(dim.clone()).or_insert_with(DimCanvas::fresh);
        for (cx, cz, pix) in entries {
            regions.insert((cx.div_euclid(32), cz.div_euclid(32)));
            // Average of the *visible* pixels only — value 0 means "nothing
            // in this column" and must not darken partially-seen chunks.
            let mut acc = [0u32; 3];
            let mut n = 0u32;
            for i in 0..256 {
                let v = u16::from_le_bytes([pix[i * 2], pix[i * 2 + 1]]);
                if v == 0 {
                    continue;
                }
                let [r, g, b] = rgb565_to_rgb(v);
                acc[0] += r as u32;
                acc[1] += g as u32;
                acc[2] += b as u32;
                n += 1;
            }
            let n = n.max(1);
            let avg = [(acc[0] / n) as u8, (acc[1] / n) as u8, (acc[2] / n) as u8];
            if canvas
                .chunks
                .insert((cx, cz), ChunkPix { pix, avg })
                .is_none()
            {
                canvas.order.push_back((cx, cz));
            }
        }
        while canvas.chunks.len() > CANVAS_CAP {
            match canvas.order.pop_front() {
                Some(k) => {
                    canvas.chunks.remove(&k);
                }
                None => break,
            }
        }
        canvas.gen += 1;
    }

    let seq = st.live.seq.fetch_add(1, Ordering::Relaxed) + 1;
    let list: Vec<[i32; 2]> = regions.into_iter().map(|(a, b)| [a, b]).collect();
    let msg = serde_json::json!({"type": "preview", "dim": dim, "regions": list, "v": seq});
    let _ = st.live.tx.send(msg.to_string());
    StatusCode::NO_CONTENT.into_response()
}

/// GET /preview/{dim}/{z}/{x}/{y} — tiles of the preview canvas, same
/// geometry as the map tiles (one region = one 512px tile at z 0).
pub(crate) async fn preview_tile(
    State(st): State<Arc<AppState>>,
    headers: header::HeaderMap,
    AxPath((dim, z, x, y)): AxPath<(String, i32, i32, i32)>,
) -> Response {
    if !(-16..=0).contains(&z) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // The canvas generation is the content identity: any chunk landing or
    // being evicted bumps it. A matching If-None-Match costs no render.
    let gen = crate::lock_ok(&st.preview.dims)
        .get(&dim)
        .map(|c| c.gen)
        .unwrap_or(0);
    let etag = format!("\"pv.{z}.{x}.{y}.{gen}\"");
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|c| c.trim() == etag))
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    let rgba = render_preview_tile(&st, &dim, z, x, y);
    let body = match rgba {
        Some(buf) => match encode_png(&buf) {
            Ok(png) => png,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        None => crate::empty_tile_png().to_vec(),
    };
    (
        [
            (header::CONTENT_TYPE, "image/png".to_string()),
            (header::CACHE_CONTROL, "no-cache".to_string()),
            (header::ETAG, etag),
        ],
        body,
    )
        .into_response()
}

fn render_preview_tile(st: &AppState, dim: &str, z: i32, x: i32, y: i32) -> Option<Vec<u8>> {
    let dims = crate::lock_ok(&st.preview.dims);
    let canvas = dims.get(dim)?;
    if canvas.chunks.is_empty() {
        return None;
    }
    let span = 1i64 << (-z); // regions per tile axis
    let cx0 = x as i64 * span * 32; // chunk range of the tile
    let cz0 = y as i64 * span * 32;
    let cn = span * 32;
    // Pixels per chunk (16 px at z 0, halves per zoom-out step).
    let chunk_px = 16.0 * (TILE as f64) / (span as f64 * 512.0);
    let mut out: Option<Vec<u8>> = None;
    let chunks_in_tile = cn * cn;
    // Small windows walk the grid; huge windows walk the canvas instead.
    let mut blit = |cx: i64, cz: i64, chunk: &ChunkPix| {
        let buf = out.get_or_insert_with(|| vec![0u8; TILE * TILE * 4]);
        let fx = (cx - cx0) as f64 * chunk_px;
        let fy = (cz - cz0) as f64 * chunk_px;
        let px0 = fx.floor() as usize;
        let py0 = fy.floor() as usize;
        let px1 = ((fx + chunk_px).ceil() as usize).min(TILE);
        let py1 = ((fy + chunk_px).ceil() as usize).min(TILE);
        if chunk_px >= 2.0 {
            for py in py0..py1 {
                for px in px0..px1 {
                    let sx = (((px as f64 - fx) / chunk_px) * 16.0) as usize;
                    let sz = (((py as f64 - fy) / chunk_px) * 16.0) as usize;
                    let (sx, sz) = (sx.min(15), sz.min(15));
                    let v = u16::from_le_bytes([
                        chunk.pix[(sz * 16 + sx) * 2],
                        chunk.pix[(sz * 16 + sx) * 2 + 1],
                    ]);
                    if v == 0 {
                        continue; // "nothing here" sentinel stays transparent
                    }
                    let [r, g, b] = rgb565_to_rgb(v);
                    let i = (py * TILE + px) * 4;
                    buf[i..i + 4].copy_from_slice(&[r, g, b, 255]);
                }
            }
        } else {
            // Chunk maps to (about) one pixel: paint its average color.
            let (px, py) = (px0.min(TILE - 1), py0.min(TILE - 1));
            let i = (py * TILE + px) * 4;
            buf[i..i + 3].copy_from_slice(&chunk.avg);
            buf[i + 3] = 255;
        }
    };
    if chunks_in_tile <= canvas.chunks.len() as i64 {
        for cz in cz0..cz0 + cn {
            for cx in cx0..cx0 + cn {
                let (Ok(cxi), Ok(czi)) = (i32::try_from(cx), i32::try_from(cz)) else {
                    continue;
                };
                if let Some(chunk) = canvas.chunks.get(&(cxi, czi)) {
                    blit(cx, cz, chunk);
                }
            }
        }
    } else {
        for (&(cx, cz), chunk) in &canvas.chunks {
            let (cx, cz) = (cx as i64, cz as i64);
            if cx >= cx0 && cx < cx0 + cn && cz >= cz0 && cz < cz0 + cn {
                blit(cx, cz, chunk);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(entries: &[(i32, i32, u16)]) -> Vec<u8> {
        let mut b = b"XTPV".to_vec();
        b.push(1);
        b.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for &(cx, cz, fill) in entries {
            b.extend_from_slice(&cx.to_le_bytes());
            b.extend_from_slice(&cz.to_le_bytes());
            for _ in 0..256 {
                b.extend_from_slice(&fill.to_le_bytes());
            }
        }
        b
    }

    #[test]
    fn parses_valid_batches_and_rejects_junk() {
        let ok = parse_batch(&batch(&[(3, -7, 0xF800), (100, 100, 0x07E0)])).unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!((ok[0].0, ok[0].1), (3, -7));
        assert_eq!(ok[0].2[0..2], 0xF800u16.to_le_bytes());

        assert!(parse_batch(b"nope").is_err());
        assert!(parse_batch(&batch(&[])).is_err());
        let mut wrong_len = batch(&[(0, 0, 0)]);
        wrong_len.pop();
        assert!(parse_batch(&wrong_len).is_err());
        let mut bad_ver = batch(&[(0, 0, 0)]);
        bad_ver[4] = 9;
        assert!(parse_batch(&bad_ver).is_err());
        assert!(parse_batch(&batch(&[(i32::MAX, 0, 0)])).is_err());
    }

    #[test]
    fn rgb565_roundtrip_extremes() {
        assert_eq!(rgb565_to_rgb(0xFFFF), [255, 255, 255]);
        assert_eq!(rgb565_to_rgb(0x0000), [0, 0, 0]);
        assert_eq!(rgb565_to_rgb(0xF800), [255, 0, 0]);
        assert_eq!(rgb565_to_rgb(0x07E0), [0, 255, 0]);
        assert_eq!(rgb565_to_rgb(0x001F), [0, 0, 255]);
    }

    #[test]
    fn eviction_by_region() {
        let ps = PreviewState::new();
        {
            let mut dims = ps.dims.lock().unwrap();
            let canvas = dims.entry("minecraft:overworld".into()).or_default();
            for (cx, cz) in [(0, 0), (31, 31), (32, 0), (-1, 0)] {
                canvas.chunks.insert(
                    (cx, cz),
                    ChunkPix {
                        pix: Box::new([0; 512]),
                        avg: [0; 3],
                    },
                );
                canvas.order.push_back((cx, cz));
            }
        }
        // Region (0,0) covers chunks 0..=31 on both axes.
        assert!(ps.evict_region("minecraft:overworld", 0, 0));
        assert_eq!(ps.chunk_count(), 2); // (32,0) and (-1,0) survive
        assert!(!ps.evict_region("minecraft:the_end", 0, 0));
    }
}
