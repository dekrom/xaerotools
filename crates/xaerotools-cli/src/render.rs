//! `xaerotools render` — stitch a block-coordinate bounding box of one map
//! layer into a single PNG.
//!
//! The image is produced one region-row at a time and streamed straight into
//! the PNG encoder, so peak memory is a band (image width x one region) rather
//! than the whole picture. Nothing under the scanned root is ever opened for
//! writing.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;
use xaero_core::render::{ColorTable, RenderOpts};
use xaero_scan::index_regions;

use crate::COLORTABLE;
use crate::archive::{fmt_bytes, fmt_int, select_layer};

/// Region side in blocks, and in pixels at native scale.
const REGION: i64 = 512;
/// Refuse anything wider or taller than this unless --max-px says otherwise.
const DEFAULT_MAX_PX: usize = 16384;

pub fn render_cmd(args: &[String]) {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut world: Option<String> = None;
    let mut dim: Option<String> = None;
    let mut mw: Option<String> = None;
    let mut layer: Option<String> = None;
    let mut bbox: Option<(i64, i64, i64, i64)> = None;
    let mut all = false;
    let mut roof = None;
    let mut cell: usize = REGION as usize;
    let mut max_px = DEFAULT_MAX_PX;
    let mut out: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                i += 1;
                roots.push(PathBuf::from(&args[i]));
            }
            "--world" => {
                i += 1;
                world = Some(args[i].clone());
            }
            "--dim" => {
                i += 1;
                dim = Some(args[i].clone());
            }
            "--mw" => {
                i += 1;
                mw = Some(args[i].clone());
            }
            "--layer" => {
                i += 1;
                layer = Some(args[i].clone());
            }
            "--cave" => {
                i += 1;
                layer = Some(format!("cave:{}", args[i]));
            }
            "--bbox" => {
                i += 1;
                bbox = Some(parse_bbox(&args[i]));
            }
            "--roof" => roof = Some(crate::ROOF_DEFAULT),
            "--all" => all = true,
            "--zoom" => {
                i += 1;
                cell = cell_from_zoom(&args[i]);
            }
            "--scale" => {
                i += 1;
                cell = cell_from_scale(&args[i]);
            }
            "--max-px" => {
                i += 1;
                max_px = args[i].parse().unwrap_or_else(|_| {
                    eprintln!("--max-px must be a number");
                    std::process::exit(2);
                });
            }
            "-o" => {
                i += 1;
                out = Some(PathBuf::from(&args[i]));
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let Some(out) = out else {
        eprintln!("render needs -o out.png");
        std::process::exit(2);
    };
    if bbox.is_none() && !all {
        eprintln!("render needs --bbox x1,z1,x2,z2 (block coords) or --all");
        std::process::exit(2);
    }
    if roots.is_empty() {
        roots = xaero_scan::default_root_candidates();
    }
    let worlds = xaerotools_server::discover_worlds(&roots);
    if worlds.is_empty() {
        eprintln!("no Xaero data found — pass --root <path to .minecraft or xaero folder>");
        std::process::exit(1);
    }
    let sel = select_layer(
        &worlds,
        world.as_deref(),
        dim.as_deref(),
        mw.as_deref(),
        layer.as_deref(),
    )
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2);
    });
    let index = index_regions(&sel.dir).unwrap_or_else(|e| {
        eprintln!("{}: {e}", sel.dir.display());
        std::process::exit(1);
    });
    let Some((bx0, bz0, bx1, bz1)) = index.bounds() else {
        eprintln!("{} holds no regions", sel.dir.display());
        std::process::exit(1);
    };

    // --bbox is in BLOCK coords, snaps outward to whole 512-block regions and
    // is then trimmed to the regions that actually exist, so asking for more
    // world than the layer holds costs nothing.
    let (rx0, rz0, rx1, rz1) = match bbox {
        None => (bx0, bz0, bx1, bz1),
        Some((x1, z1, x2, z2)) => (
            bx0.max(x1.div_euclid(REGION) as i32),
            bz0.max(z1.div_euclid(REGION) as i32),
            bx1.min(x2.div_euclid(REGION) as i32),
            bz1.min(z2.div_euclid(REGION) as i32),
        ),
    };
    if rx0 > rx1 || rz0 > rz1 {
        eprintln!(
            "that box holds no regions — {} covers blocks {}..{} x {}..{}",
            sel.label(),
            bx0 as i64 * REGION,
            (bx1 as i64 + 1) * REGION - 1,
            bz0 as i64 * REGION,
            (bz1 as i64 + 1) * REGION - 1
        );
        std::process::exit(1);
    }
    let cols = (rx1 - rx0 + 1) as usize;
    let rows = (rz1 - rz0 + 1) as usize;
    let w = cols * cell;
    let h = rows * cell;
    if w > max_px || h > max_px {
        eprintln!(
            "that box renders to {w} x {h} px, over the {max_px} px cap.\n\
             {} spans blocks {}..{} x {}..{} — a few outlying regions can stretch\n\
             that a long way, so --bbox is usually what you want, not --all.\n\
             Zoom out (--zoom -1 halves each axis, down to -9) or shrink --bbox;\n\
             raise the cap with --max-px N if you really want it.",
            sel.label(),
            bx0 as i64 * REGION,
            (bx1 as i64 + 1) * REGION - 1,
            bz0 as i64 * REGION,
            (bz1 as i64 + 1) * REGION - 1
        );
        std::process::exit(2);
    }
    // One sweep of the index rather than a probe per grid cell: at --zoom -9 a
    // capped box is 16384 x 16384 regions, far more cells than the index has.
    let present = index
        .entries
        .keys()
        .filter(|&&(rx, rz)| rx >= rx0 && rx <= rx1 && rz >= rz0 && rz <= rz1)
        .count();

    eprintln!(
        "{} {}  blocks {}..{} x {}..{}  ->  {w} x {h} px ({} px/block, {} regions)",
        sel.world,
        sel.label(),
        rx0 as i64 * REGION,
        (rx1 as i64 + 1) * REGION - 1,
        rz0 as i64 * REGION,
        (rz1 as i64 + 1) * REGION - 1,
        cell as f64 / REGION as f64,
        fmt_int(present as u64)
    );

    let ct = ColorTable::parse(COLORTABLE).expect("embedded color table");
    // Nether folders get the nether's ambient light and logical height; the
    // cave selection forces every tile's cave mode so legacy tiles follow the
    // layer they sit in.
    let nether = sel.dim == "DIM-1";
    let opts = RenderOpts {
        dim_ambient: if nether { 0.1 } else { 0.0 },
        logical_height: if nether { 128 } else { 384 },
        cave_override: sel.cave,
        roof,
        ..Default::default()
    };

    let file = std::fs::File::create(&out).unwrap_or_else(|e| {
        eprintln!("create {}: {e}", out.display());
        std::process::exit(1);
    });
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().expect("png header");
    let mut stream = writer.stream_writer().expect("png stream");

    let failed = AtomicUsize::new(0);
    let mut band = vec![0u8; w * cell * 4];
    // One reused cell buffer per column: a tall render is thousands of rows,
    // and re-allocating the whole strip every row churns the allocator badly.
    let mut cells: Vec<Vec<u8>> = vec![Vec::new(); cols];
    for (row, rz) in (rz0..=rz1).enumerate() {
        band.fill(0);
        cells.par_iter_mut().enumerate().for_each(|(col, buf)| {
            buf.clear();
            let Some(path) = index.region_path(rx0 + col as i32, rz) else {
                return;
            };
            if !render_cell(&path, cell, &ct, &opts, buf) {
                failed.fetch_add(1, Ordering::Relaxed);
            }
        });
        for (col, px) in cells.iter().enumerate() {
            if px.is_empty() {
                continue;
            }
            for cy in 0..cell {
                let dst = (cy * w + col * cell) * 4;
                band[dst..dst + cell * 4].copy_from_slice(&px[cy * cell * 4..(cy + 1) * cell * 4]);
            }
        }
        stream.write_all(&band).expect("png data");
        eprint!("\rrow {}/{rows}", row + 1);
    }
    eprintln!();
    stream.finish().expect("png finish");
    writer.finish().expect("png finish");

    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    let failed = failed.load(Ordering::Relaxed);
    if failed > 0 {
        eprintln!("{failed} region(s) could not be decoded — run `xaerotools doctor` for details");
    }
    println!("wrote {} ({w} x {h}, {})", out.display(), fmt_bytes(size));
}

/// Renders one region into `out` as a `cell` x `cell` RGBA square. Returns
/// false and leaves `out` empty when the file is unreadable or undecodable, so
/// the caller can leave that square transparent and count it.
fn render_cell(
    path: &std::path::Path,
    cell: usize,
    ct: &ColorTable,
    opts: &RenderOpts,
    out: &mut Vec<u8>,
) -> bool {
    out.clear();
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(stream) = xaero_core::read_region_container(&bytes) else {
        return false;
    };
    let Ok(dec) = xaero_core::decode_region(&stream) else {
        return false;
    };
    let rgba = xaero_core::render::render_region(&dec, ct, opts);
    if cell == REGION as usize {
        *out = rgba;
    } else {
        out.resize(cell * cell * 4, 0);
        downscale_box(&rgba, cell, out);
    }
    true
}

/// Alpha-weighted box downscale of a 512x512 RGBA region into a cell x cell
/// buffer, matching what the server's compose path does for zoomed-out tiles.
/// `out` must already be `cell * cell * 4` zeroed bytes.
fn downscale_box(src: &[u8], cell: usize, out: &mut [u8]) {
    let size = REGION as usize;
    let f = size / cell;
    for cy in 0..cell {
        for cx in 0..cell {
            let mut acc = [0u64; 4];
            for sy in 0..f {
                let row = ((cy * f + sy) * size + cx * f) * 4;
                for sx in 0..f {
                    let si = row + sx * 4;
                    let a = src[si + 3] as u64;
                    acc[0] += src[si] as u64 * a;
                    acc[1] += src[si + 1] as u64 * a;
                    acc[2] += src[si + 2] as u64 * a;
                    acc[3] += a;
                }
            }
            if let Some(r) = acc[0].checked_div(acc[3]) {
                let di = (cy * cell + cx) * 4;
                out[di] = r as u8;
                out[di + 1] = (acc[1] / acc[3]) as u8;
                out[di + 2] = (acc[2] / acc[3]) as u8;
                out[di + 3] = (acc[3] / (f * f) as u64) as u8;
            }
        }
    }
}

fn parse_bbox(s: &str) -> (i64, i64, i64, i64) {
    let nums: Vec<i64> = s
        .split(',')
        .map(|p| {
            p.trim().parse().unwrap_or_else(|_| {
                eprintln!("--bbox wants four block coords: x1,z1,x2,z2");
                std::process::exit(2);
            })
        })
        .collect();
    if nums.len() != 4 {
        eprintln!("--bbox wants four block coords: x1,z1,x2,z2");
        std::process::exit(2);
    }
    (
        nums[0].min(nums[2]),
        nums[1].min(nums[3]),
        nums[0].max(nums[2]),
        nums[1].max(nums[3]),
    )
}

/// --zoom 0 is native (512 px per region); each step down halves both axes.
fn cell_from_zoom(s: &str) -> usize {
    let z: i32 = s.parse().unwrap_or_else(|_| {
        eprintln!("--zoom must be a number in 0..-9");
        std::process::exit(2);
    });
    if !(-9..=0).contains(&z) {
        eprintln!("--zoom must be in 0..-9 (0 = native 512 px per region)");
        std::process::exit(2);
    }
    (REGION as usize) >> (-z)
}

/// --scale is pixels per block; 1 is native, and only halvings tile cleanly.
fn cell_from_scale(s: &str) -> usize {
    let v: f64 = s.parse().unwrap_or_else(|_| {
        eprintln!("--scale must be a number of pixels per block");
        std::process::exit(2);
    });
    let cell = (v * REGION as f64).round();
    if !(1.0..=REGION as f64).contains(&cell) || !(cell as usize).is_power_of_two() {
        eprintln!(
            "--scale must be 1 or a halving of it (1, 0.5, 0.25 … {:.6}); \
             --zoom 0..-9 spells the same thing",
            1.0 / REGION as f64
        );
        std::process::exit(2);
    }
    cell as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbox_is_normalized() {
        assert_eq!(parse_bbox("100,-200,-50,300"), (-50, -200, 100, 300));
        assert_eq!(parse_bbox(" 0 , 0 , 1 , 1 "), (0, 0, 1, 1));
    }

    #[test]
    fn zoom_and_scale_agree() {
        assert_eq!(cell_from_zoom("0"), 512);
        assert_eq!(cell_from_zoom("-3"), 64);
        assert_eq!(cell_from_scale("1"), 512);
        assert_eq!(cell_from_scale("0.125"), 64);
    }

    #[test]
    fn downscale_averages_a_flat_square() {
        let src = vec![10u8; 512 * 512 * 4];
        let mut out = vec![0u8; 8 * 8 * 4];
        downscale_box(&src, 8, &mut out);
        assert!(out.iter().all(|&b| b == 10));
    }
}
