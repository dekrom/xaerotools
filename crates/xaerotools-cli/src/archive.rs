//! Shared archive walking for `render`, `stats` and `doctor`: layer
//! enumeration, flag-driven layer selection, read-only region probing and the
//! small number/date formatters those three print with.
//!
//! Everything here is read-only — nothing in this module opens a file for
//! writing, and DBs are left to `xaero_db::open_readonly`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;
use xaero_core::codec::{CodecError, FormatVersion};
use xaero_scan::{RegionIndex, layer_dir};
use xaerotools_server::WorldEntry;

/// One addressable map layer on disk: a `mw$*` folder, or one of its cave
/// layers under `caves/<n>/`.
#[derive(Debug, Clone)]
pub struct Layer {
    pub world: String,
    pub dim: String,
    pub mw: String,
    pub cave: Option<i32>,
    pub dir: PathBuf,
}

impl Layer {
    /// The `--layer` spelling: "surface" or "cave:-2".
    pub fn layer_name(&self) -> String {
        match self.cave {
            None => "surface".to_string(),
            Some(n) => format!("cave:{n}"),
        }
    }

    /// Short human label, e.g. "null/mw$default cave:-2".
    pub fn label(&self) -> String {
        match self.cave {
            None => format!("{}/{}", self.dim, self.mw),
            Some(n) => format!("{}/{} cave:{n}", self.dim, self.mw),
        }
    }
}

/// Every surface and cave layer of one world, in dim / mw / cave order.
pub fn layers_of(w: &WorldEntry) -> Vec<Layer> {
    let Some(wm) = &w.world.world_map_path else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for d in &w.world.dims {
        for m in &d.multiworlds {
            out.push(Layer {
                world: w.world.id.clone(),
                dim: d.folder.clone(),
                mw: m.id.clone(),
                cave: None,
                dir: layer_dir(wm, &d.folder, &m.id, None),
            });
            for &n in &m.cave_layers {
                out.push(Layer {
                    world: w.world.id.clone(),
                    dim: d.folder.clone(),
                    mw: m.id.clone(),
                    cave: Some(n),
                    dir: layer_dir(wm, &d.folder, &m.id, Some(n)),
                });
            }
        }
    }
    out
}

/// Worlds matching `--world` (exact id first, then a unique substring).
/// `None` keeps everything.
pub fn matching_worlds<'a>(worlds: &'a [WorldEntry], want: Option<&str>) -> Vec<&'a WorldEntry> {
    let Some(want) = want else {
        return worlds.iter().collect();
    };
    let exact: Vec<&WorldEntry> = worlds.iter().filter(|w| w.world.id == want).collect();
    if !exact.is_empty() {
        return exact;
    }
    worlds
        .iter()
        .filter(|w| w.world.id.contains(want))
        .collect()
}

/// Picks exactly one layer, defaulting whenever the choice is unambiguous and
/// listing the options when it is not.
pub fn select_layer(
    worlds: &[WorldEntry],
    want_world: Option<&str>,
    want_dim: Option<&str>,
    want_mw: Option<&str>,
    want_layer: Option<&str>,
) -> Result<Layer, String> {
    let candidates = matching_worlds(worlds, want_world);
    let world = match candidates.len() {
        0 => {
            return Err(format!(
                "no world matches --world {}\nknown worlds: {}",
                want_world.unwrap_or(""),
                list(worlds.iter().map(|w| w.world.id.clone()))
            ));
        }
        1 => candidates[0],
        _ => {
            return Err(format!(
                "several worlds match — pass --world <id>: {}",
                list(candidates.iter().map(|w| w.world.id.clone()))
            ));
        }
    };
    let dim = match want_dim {
        Some(f) => world
            .world
            .dims
            .iter()
            .find(|d| d.folder == f)
            .ok_or_else(|| {
                format!(
                    "world {} has no dimension folder {f}\nknown: {}",
                    world.world.id,
                    list(world.world.dims.iter().map(|d| d.folder.clone()))
                )
            })?,
        None => {
            let preferred = world
                .world
                .dims
                .iter()
                .find(|d| d.folder == "null" || d.folder == "DIM0");
            match preferred.or_else(|| (world.world.dims.len() == 1).then(|| &world.world.dims[0]))
            {
                Some(d) => d,
                None => {
                    return Err(format!(
                        "pass --dim <folder>: {}",
                        list(world.world.dims.iter().map(|d| d.folder.clone()))
                    ));
                }
            }
        }
    };
    let mw = match want_mw {
        Some(m) => dim.multiworlds.iter().find(|e| e.id == m).ok_or_else(|| {
            format!(
                "dimension {} has no multiworld {m}\nknown: {}",
                dim.folder,
                list(dim.multiworlds.iter().map(|e| e.id.clone()))
            )
        })?,
        None => {
            let preferred = dim.multiworlds.iter().find(|e| e.id == "mw$default");
            match preferred.or_else(|| (dim.multiworlds.len() == 1).then(|| &dim.multiworlds[0])) {
                Some(m) => m,
                None => {
                    return Err(format!(
                        "pass --mw <folder>: {}",
                        list(dim.multiworlds.iter().map(|e| e.id.clone()))
                    ));
                }
            }
        }
    };
    let cave = parse_layer_spec(want_layer)?;
    if let Some(n) = cave
        && !mw.cave_layers.contains(&n)
    {
        return Err(format!(
            "{}/{} has no cave layer {n}\nknown cave layers: {}",
            dim.folder,
            mw.id,
            list(mw.cave_layers.iter().map(|n| n.to_string()))
        ));
    }
    let wm = world
        .world
        .world_map_path
        .as_ref()
        .ok_or_else(|| format!("world {} has no world-map folder", world.world.id))?;
    Ok(Layer {
        world: world.world.id.clone(),
        dim: dim.folder.clone(),
        mw: mw.id.clone(),
        cave,
        dir: layer_dir(wm, &dim.folder, &mw.id, cave),
    })
}

/// "surface" (or absent) -> None, "cave:N" -> Some(N).
pub fn parse_layer_spec(spec: Option<&str>) -> Result<Option<i32>, String> {
    match spec {
        None | Some("surface") => Ok(None),
        Some(s) => match s.strip_prefix("cave:") {
            Some(n) => n
                .parse::<i32>()
                .map(Some)
                .map_err(|_| format!("--layer cave:N needs a number, got {s}")),
            None => Err(format!("--layer must be surface or cave:N, got {s}")),
        },
    }
}

fn list(items: impl Iterator<Item = String>) -> String {
    let v: Vec<String> = items.collect();
    if v.is_empty() {
        "(none)".to_string()
    } else {
        v.join(", ")
    }
}

// ------------------------------------------------------------ region probe --

/// What one region file turned out to be, from a strictly read-only pass.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// Save version, known even when the codec refuses the file.
    pub version: Option<FormatVersion>,
    /// Explored Minecraft chunks — one Xaero tile is one 16x16 chunk.
    pub chunks: usize,
    /// Decoded, but the stream ended mid-structure.
    pub truncated: bool,
    /// The save version is outside what the codec accepts.
    pub unsupported: bool,
    pub error: Option<String>,
}

/// Reads and decodes one region file. Never panics and never writes.
pub fn probe_region(path: &Path) -> Probe {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            return Probe {
                error: Some(format!("read: {e}")),
                ..Default::default()
            };
        }
    };
    let stream = match xaero_core::read_region_container(&bytes) {
        Ok(s) => s,
        Err(e) => {
            return Probe {
                error: Some(e.to_string()),
                ..Default::default()
            };
        }
    };
    match xaero_core::decode_region(&stream) {
        Ok(dec) => {
            // Markers may repeat; the renderer is last-wins, so count that way.
            let mut per_marker = [0usize; 256];
            for (marker, chunk) in &dec.region.chunks {
                per_marker[*marker as usize] = chunk.tiles.iter().flatten().count();
            }
            Probe {
                version: Some(dec.version),
                chunks: per_marker.iter().sum(),
                truncated: dec.truncated,
                unsupported: false,
                error: None,
            }
        }
        Err(e) => {
            let version = match &e {
                CodecError::UnsupportedVersion { version, .. } => Some(*version),
                _ => None,
            };
            Probe {
                version,
                chunks: 0,
                truncated: false,
                unsupported: version.is_some(),
                error: Some(e.to_string()),
            }
        }
    }
}

/// Deterministic, spatially spread sample of at most `n` regions — all of them
/// when `n` is 0 or exceeds the layer size. Sorted coords plus a fixed stride
/// beats a random draw here: the same archive always yields the same sample.
///
/// Zero-byte files are skipped: they can never decode, callers report them
/// exactly from the index instead, and letting them into the sample would just
/// spend a slot proving what a `stat` already said.
pub fn sample_regions(index: &RegionIndex, n: usize) -> Vec<PathBuf> {
    let mut keys: Vec<(i32, i32)> = index
        .entries
        .iter()
        .filter(|(_, m)| m.size > 0)
        .map(|(&k, _)| k)
        .collect();
    keys.sort_unstable();
    if n == 0 || n >= keys.len() {
        return keys
            .iter()
            .filter_map(|&(rx, rz)| index.region_path(rx, rz))
            .collect();
    }
    // Rounded up: a floored step with `take(n)` never reaches the tail of the
    // sorted keys (511 regions sampled at 256 would all come from one half).
    let step = keys.len().div_ceil(n).max(1);
    keys.iter()
        .step_by(step)
        .take(n)
        .filter_map(|&(rx, rz)| index.region_path(rx, rz))
        .collect()
}

/// Probes every path in parallel, ticking a one-line stderr counter on the long
/// passes — a `--deep`/`--full` sweep of a million-region layer runs for
/// minutes and silence reads as a hang. Results keep `paths` order.
pub fn probe_all(paths: &[PathBuf], label: &str) -> Vec<Probe> {
    let total = paths.len();
    let done = AtomicUsize::new(0);
    paths
        .par_iter()
        .map(|p| {
            let probe = probe_region(p);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if total > 8192 && n.is_multiple_of(4096) {
                eprint!(
                    "\r{:78}\r{label}: {} / {} regions",
                    "",
                    fmt_int(n as u64),
                    fmt_int(total as u64)
                );
            }
            probe
        })
        .collect()
}

/// Every zero-byte region file in the layer, sorted — a `stat` already proves
/// these are dead, so they are counted exactly rather than sampled.
pub fn empty_regions(index: &RegionIndex) -> Vec<PathBuf> {
    let mut keys: Vec<(i32, i32)> = index
        .entries
        .iter()
        .filter(|(_, m)| m.size == 0)
        .map(|(&k, _)| k)
        .collect();
    keys.sort_unstable();
    keys.iter()
        .filter_map(|&(rx, rz)| index.region_path(rx, rz))
        .collect()
}

// --------------------------------------------------------------- formatting --

/// 1005234 -> "1,005,234".
pub fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Binary units, two significant decimals below 100.
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else if v < 100.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.0} {}", UNITS[u])
    }
}

/// Unix milliseconds -> "YYYY-MM-DD" (UTC). Hinnant's civil-from-days; the
/// archive only ever needs day resolution, so no chrono dependency.
pub fn fmt_date(ms: u64) -> String {
    if ms == 0 {
        return "-".to_string();
    }
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_spec_round_trip() {
        assert_eq!(parse_layer_spec(None).unwrap(), None);
        assert_eq!(parse_layer_spec(Some("surface")).unwrap(), None);
        assert_eq!(parse_layer_spec(Some("cave:-2")).unwrap(), Some(-2));
        assert!(parse_layer_spec(Some("cave:x")).is_err());
        assert!(parse_layer_spec(Some("caves")).is_err());
    }

    #[test]
    fn formatters() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(1_005_234), "1,005,234");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(57_900_000_000), "53.9 GiB");
        // 2021-03-04T00:00:00Z and the epoch itself.
        assert_eq!(fmt_date(1_614_816_000_000), "2021-03-04");
        assert_eq!(fmt_date(1), "1970-01-01");
        assert_eq!(fmt_date(0), "-");
    }
}
