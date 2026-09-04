//! `xaerotools import-zvcr` — converts a zvcr3d world download into Xaero's
//! World Map region format, so a downloaded archive can be merged underneath
//! your own captured data to fill the parts you never walked.
//!
//! Input is a zvcr directory (`<dim>/<sectorX>/<sectorZ>/r.<x>.<z>.zvcr3d`);
//! output is a normal Xaero tree that `xaerotools merge` accepts on either
//! side. Regions are independent, so the work fans out across cores and the
//! run is resumable: an existing output file is skipped unless `--overwrite`.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use rayon::prelude::*;
use xaero_zvcr::blockprops::BlockProps;
use xaero_zvcr::{region, zvcr};

pub fn import_zvcr_cmd(args: &[String]) {
    let mut src: Vec<PathBuf> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut world = String::from("Multiplayer_2b2t");
    let mut mw = String::from("mw$default");
    let mut threads: Option<usize> = None;
    let mut overwrite = false;
    let mut dry_run = false;
    let mut limit: Option<usize> = None;
    let mut nether_roof_removal = true;

    let mut i = 0;
    while i < args.len() {
        let need = |i: usize, what: &str| -> String {
            args.get(i + 1)
                .cloned()
                .unwrap_or_else(|| fail(&format!("{what} needs a value")))
        };
        match args[i].as_str() {
            "--src" => {
                src.push(PathBuf::from(need(i, "--src")));
                i += 1;
            }
            "-o" | "--out" => {
                out = Some(PathBuf::from(need(i, "--out")));
                i += 1;
            }
            "--world" => {
                world = need(i, "--world");
                i += 1;
            }
            "--mw" => {
                mw = need(i, "--mw");
                i += 1;
            }
            "--threads" => {
                threads = Some(need(i, "--threads").parse().unwrap_or_else(|_| {
                    fail("--threads needs a number");
                }));
                i += 1;
            }
            "--limit" => {
                limit = Some(need(i, "--limit").parse().unwrap_or_else(|_| {
                    fail("--limit needs a number");
                }));
                i += 1;
            }
            "--overwrite" => overwrite = true,
            "--dry-run" => dry_run = true,
            // The archive this targets maps the Nether with XaeroPlus's Nether
            // Cave Fix on. Turning it off writes the bedrock roof instead,
            // which is what older regions in that archive look like.
            "--no-nether-roof-removal" => nether_roof_removal = false,
            other => fail(&format!("unknown arg: {other}")),
        }
        i += 1;
    }

    if src.is_empty() {
        fail("usage: xaerotools import-zvcr --src DIR... -o OUT_ROOT [--world W] [--mw MW]");
    }
    let Some(out_root) = out else {
        fail("-o OUT_ROOT is required");
    };

    let props = match BlockProps::parse(xaero_zvcr::BLOCKPROPS) {
        Ok(p) => p,
        Err(e) => fail(&format!("baked block table is unreadable: {e}")),
    };

    let mut files: Vec<PathBuf> = Vec::new();
    for dir in &src {
        // An unreadable subtree fails the run: silently shrinking the input
        // would let the ring script stamp a half-converted ring as done.
        if let Err(e) = collect_regions(dir, &mut files) {
            fail(&format!("cannot read {}: {e}", dir.display()));
        }
    }
    // The same region under two --src dirs would race on one output file
    // (both pass the exists check, both write the same .part); the first
    // source given wins, as the download's own extract.sh has it.
    let mut seen: HashSet<std::ffi::OsString> = HashSet::new();
    files.retain(|f| {
        let name = f.file_name().map(|n| n.to_os_string()).unwrap_or_default();
        if seen.insert(name) {
            true
        } else {
            eprintln!("  {}: duplicate of an earlier --src, skipped", f.display());
            false
        }
    });
    files.sort();
    if let Some(n) = limit {
        files.truncate(n);
    }
    if files.is_empty() {
        fail("no r.<x>.<z>.zvcr3d files found under the given --src paths");
    }
    eprintln!(
        "import-zvcr: {} region files, MC {} block table, nether roof removal {}",
        files.len(),
        props.mc_version(),
        if nether_roof_removal { "on" } else { "off" }
    );

    if let Some(n) = threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .unwrap_or_else(|e| fail(&format!("thread pool: {e}")));
    }

    let done = AtomicUsize::new(0);
    let written = AtomicUsize::new(0);
    let skipped_existing = AtomicUsize::new(0);
    let skipped_void = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let bytes_out = AtomicU64::new(0);
    let tiles = AtomicU64::new(0);
    // Which dimensions the run actually produced, so only those get a config.
    let dims_seen = AtomicU8::new(0);
    let start = std::time::Instant::now();
    let total = files.len();

    files.par_iter().for_each(|path| {
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        // One line every few hundred regions: enough to see a multi-hour run is
        // alive, not so much that it buries the failures.
        if n.is_multiple_of(500) || n == total {
            let secs = start.elapsed().as_secs_f64();
            eprintln!(
                "  {n}/{total} regions  {:.0}/s  {} written  {} void  {} existing  {} failed",
                n as f64 / secs.max(0.001),
                written.load(Ordering::Relaxed),
                skipped_void.load(Ordering::Relaxed),
                skipped_existing.load(Ordering::Relaxed),
                failed.load(Ordering::Relaxed),
            );
        }
        // A panic in one region (a malformed download, a bug) is that
        // region's failure, not the end of a multi-hour run: rayon would
        // otherwise resume the unwind on the main thread and abort everything.
        let result = catch_unwind(AssertUnwindSafe(|| {
            convert_one(
                path,
                &out_root,
                &world,
                &mw,
                &props,
                nether_roof_removal,
                overwrite,
                dry_run,
                &dims_seen,
            )
        }))
        .unwrap_or_else(|payload| {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            Err(format!("converter panicked: {msg}"))
        });
        match result {
            Ok(Outcome::Written { bytes, non_empty }) => {
                written.fetch_add(1, Ordering::Relaxed);
                bytes_out.fetch_add(bytes, Ordering::Relaxed);
                tiles.fetch_add(non_empty as u64, Ordering::Relaxed);
            }
            Ok(Outcome::SkippedExisting) => {
                skipped_existing.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Outcome::SkippedVoid) => {
                skipped_void.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                failed.fetch_add(1, Ordering::Relaxed);
                eprintln!("  {}: {e}", path.display());
            }
        }
    });

    if !dry_run
        && let Err(e) =
            write_dimension_configs(&out_root, &world, &mw, dims_seen.load(Ordering::Relaxed))
    {
        eprintln!("could not write dimension config: {e}");
    }

    let secs = start.elapsed().as_secs_f64();
    println!(
        "converted {} of {} regions in {:.1}s ({:.1} regions/s)",
        written.load(Ordering::Relaxed),
        total,
        secs,
        total as f64 / secs.max(0.001)
    );
    println!(
        "  {} tiles with terrain, {:.2} GiB written, {} void regions skipped, {} already present, {} failed",
        tiles.load(Ordering::Relaxed),
        bytes_out.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0 * 1024.0),
        skipped_void.load(Ordering::Relaxed),
        skipped_existing.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed),
    );
    if failed.load(Ordering::Relaxed) > 0 {
        std::process::exit(1);
    }
}

enum Outcome {
    Written { bytes: u64, non_empty: usize },
    SkippedExisting,
    SkippedVoid,
}

#[allow(clippy::too_many_arguments)]
fn convert_one(
    path: &Path,
    out_root: &Path,
    world: &str,
    mw: &str,
    props: &BlockProps,
    nether_roof_removal: bool,
    overwrite: bool,
    dry_run: bool,
    dims_seen: &AtomicU8,
) -> Result<Outcome, String> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("unreadable file name")?;
    let (rx, rz) = zvcr::parse_region_name(name).ok_or("file name is not r.<x>.<z>.zvcr3d")?;

    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let (header, container) = zvcr::open(&bytes).map_err(|e| e.to_string())?;
    if header.protocol != xaero_zvcr::biomes::PROTOCOL {
        return Err(format!(
            "protocol {} but the baked registries are for {}",
            header.protocol,
            xaero_zvcr::biomes::PROTOCOL
        ));
    }

    dims_seen.fetch_or(dim_bit(header.dim), Ordering::Relaxed);
    let dest_dir = out_root
        .join(world)
        .join(header.dim.xaero_folder())
        .join(mw);
    let dest = dest_dir.join(region::region_file_name(rx, rz));
    if !overwrite && dest.exists() {
        return Ok(Outcome::SkippedExisting);
    }

    let opts = region::opts_for(header.dim, nether_roof_removal);
    let converted =
        region::convert(&container, header.dim, props, opts).map_err(|e| e.to_string())?;
    // The End is mostly void; writing empty regions would triple the file count
    // for nothing and give the merge nothing to merge.
    if converted.non_empty_tiles == 0 {
        return Ok(Outcome::SkippedVoid);
    }
    // The download's own observation time is the only honest mtime for the
    // output (see below); a region without one cannot be stamped truthfully,
    // and stamping it with the run time would be the silent failure the
    // stamp exists to prevent.
    if converted.newest_timestamp == 0 {
        return Err(
            "no snapshot timestamp in the download, cannot stamp an observation time".into(),
        );
    }

    let stream = xaero_core::encode_region(&converted.region);
    let container_bytes = xaero_core::write_region_container(&stream).map_err(|e| e.to_string())?;
    if dry_run {
        return Ok(Outcome::Written {
            bytes: container_bytes.len() as u64,
            non_empty: converted.non_empty_tiles,
        });
    }

    std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
    // Write beside the target and rename: a killed run must never leave a
    // half-written region for the merge to trip over.
    let tmp = dest.with_extension("zip.part");
    std::fs::write(&tmp, &container_bytes).map_err(|e| e.to_string())?;
    // Stamp the file with when the download actually observed this region, not
    // with now. The merge weighs conflicting tiles by mtime, so a truthful
    // stamp is what lets your own captures win where they are more recent —
    // and lets the download win where it is. Stamping "now" would silently
    // make every downloaded tile beat everything you ever mapped.
    //
    // Stamped on the temp file, before the rename: rename keeps the mtime, and
    // a stamp that fails then leaves only a .part behind instead of a
    // now-stamped region that the next run would skip as already present.
    let when = filetime::FileTime::from_unix_time(converted.newest_timestamp as i64, 0);
    if let Err(e) = filetime::set_file_mtime(&tmp, when) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("stamp observation time: {e}"));
    }
    std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    Ok(Outcome::Written {
        bytes: container_bytes.len() as u64,
        non_empty: converted.non_empty_tiles,
    })
}

fn dim_bit(dim: zvcr::Dim) -> u8 {
    match dim {
        zvcr::Dim::Overworld => 1,
        zvcr::Dim::Nether => 2,
        zvcr::Dim::End => 4,
    }
}

/// Xaero keeps one `dimension_config.txt` per dimension folder, naming the
/// multiworld and the dimension type. Without it the viewer still reads the
/// regions, but the dimension shows up unnamed and with no type.
fn write_dimension_configs(
    out_root: &Path,
    world: &str,
    mw: &str,
    seen: u8,
) -> std::io::Result<()> {
    for (dim, type_id) in [
        (zvcr::Dim::Overworld, "minecraft:overworld"),
        (zvcr::Dim::Nether, "minecraft:the_nether"),
        (zvcr::Dim::End, "minecraft:the_end"),
    ] {
        if seen & dim_bit(dim) == 0 {
            continue;
        }
        let path = out_root
            .join(world)
            .join(dim.xaero_folder())
            .join("dimension_config.txt");
        // Never clobber a config that is already there: a rerun into an
        // existing tree must not rename someone's multiworld.
        if path.exists() {
            continue;
        }
        std::fs::write(
            &path,
            format!("MWName:{mw}:Map 1\ncaveModeType:0\ndimensionTypeId:{type_id}\n"),
        )?;
    }
    Ok(())
}

fn collect_regions(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let at = |e: std::io::Error| std::io::Error::new(e.kind(), format!("{}: {e}", dir.display()));
    for entry in std::fs::read_dir(dir).map_err(at)? {
        let path = entry.map_err(at)?.path();
        if path.is_dir() {
            collect_regions(&path, out)?;
        } else if path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(zvcr::parse_region_name)
            .is_some()
        {
            out.push(path);
        }
    }
    Ok(())
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2)
}
