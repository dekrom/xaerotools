//! `xaerotools doctor` — find what in an archive cannot be read, or can only
//! be read from a copy the game no longer looks at.
//!
//! Regions are sampled (`--sample N`, `--full` for every one) because a 431 GB
//! archive is a million decodes otherwise; databases are always all opened,
//! read-only, since that is cheap. Findings are grouped by cause with a few
//! example paths each, so the output stays readable when a whole layer is bad.
//!
//! Findings are not errors: the exit status says whether doctor could survey
//! the archive, not whether the archive is perfect, so `doctor && backup` is
//! not blocked by a region that has been corrupt for two years.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::json;
use xaero_scan::{AlternateSource, index_regions, scan_region_alternates};

use crate::archive::{
    Layer, empty_regions, fmt_int, layers_of, matching_worlds, probe_all, sample_regions,
};

/// Regions decoded per layer when --sample/--full are not given.
const DEFAULT_SAMPLE: usize = 200;

/// One class of finding, with the paths that showed it.
struct Issue {
    kind: &'static str,
    detail: String,
    count: usize,
    /// True when `count` covers the whole layer (a readdir was enough to know),
    /// false when it is only what the decoded sample showed.
    exact: bool,
    examples: Vec<String>,
}

struct LayerReport {
    layer: Layer,
    regions: u64,
    checked: usize,
    issues: Vec<Issue>,
}

struct DbReport {
    db: String,
    error: String,
}

pub fn doctor_cmd(args: &[String]) {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut world: Option<String> = None;
    let mut json = false;
    let mut full = false;
    let mut sample = DEFAULT_SAMPLE;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                roots.push(PathBuf::from(crate::value(args, &mut i, "--root")));
            }
            "--world" => {
                world = Some(crate::value(args, &mut i, "--world"));
            }
            "--sample" => {
                sample = crate::value(args, &mut i, "--sample")
                    .parse()
                    .unwrap_or_else(|_| {
                        eprintln!("--sample must be a number of regions per layer");
                        std::process::exit(2);
                    });
            }
            "--full" => full = true,
            "--json" => json = true,
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    if full {
        sample = 0; // 0 means "every region" to sample_regions
    }
    if roots.is_empty() {
        roots = xaero_scan::default_root_candidates();
    }
    let worlds = xaerotools_server::discover_worlds(&roots);
    if worlds.is_empty() {
        eprintln!("no Xaero data found — pass --root <path to .minecraft or xaero folder>");
        std::process::exit(1);
    }
    let selected = matching_worlds(&worlds, world.as_deref());
    if selected.is_empty() {
        eprintln!("no world matches --world {}", world.unwrap_or_default());
        std::process::exit(1);
    }

    let mut json_worlds = Vec::new();
    let mut found = 0usize;
    let mut checked = 0usize;
    let mut total = 0u64;
    for w in &selected {
        let mut reports = Vec::new();
        for layer in layers_of(w) {
            if progress_enabled() {
                eprint!("\r{:78}\rchecking {} {} …", "", w.world.id, layer.label());
            }
            if let Some(r) = check_layer(&w.world.id, layer, sample) {
                checked += r.checked;
                total += r.regions;
                found += r.issues.iter().map(|is| is.count).sum::<usize>();
                reports.push(r);
            }
        }
        if progress_enabled() {
            eprint!("\r{:78}\r", "");
        }
        let dbs = check_dbs(w);
        found += dbs.len();
        if json {
            json_worlds.push(json!({
                "world": w.world.id,
                "layers": reports.iter().map(layer_json).collect::<Vec<_>>(),
                "databases": dbs.iter()
                    .map(|d| json!({ "db": d.db, "error": d.error }))
                    .collect::<Vec<_>>(),
            }));
        } else {
            print_world(&w.world.id, &reports, &dbs);
        }
    }
    // The hint only makes sense when something really was left unchecked —
    // a --full pass still reports fewer checked than total, because zero-byte
    // files are counted from the index instead of being decoded.
    let hint = if full || checked as u64 >= total {
        ""
    } else {
        " — re-run with --full to check every region"
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "worlds": json_worlds,
                "regionsChecked": checked,
                "regionsTotal": total,
                "findings": found,
                "full": full,
            }))
            .unwrap()
        );
    } else if found == 0 {
        println!(
            "\nnothing wrong in {} region(s) checked{hint}",
            fmt_int(checked as u64)
        );
    } else {
        println!(
            "\n{} finding(s) across {} region(s) checked{hint}",
            fmt_int(found as u64),
            fmt_int(checked as u64)
        );
    }
}

/// True when stderr is a terminal, so `\r` progress redraws make sense.
/// Piped or redirected, they would turn one status line into thousands.
fn progress_enabled() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

fn check_layer(world: &str, layer: Layer, sample: usize) -> Option<LayerReport> {
    let index = match index_regions(&layer.dir) {
        Ok(i) => i,
        Err(e) => {
            return Some(LayerReport {
                regions: 0,
                checked: 0,
                issues: vec![Issue {
                    kind: "unreachable",
                    detail: format!("{}: {e}", layer.dir.display()),
                    count: 1,
                    exact: true,
                    examples: Vec::new(),
                }],
                layer,
            });
        }
    };
    // Alternates are found by name, so a layer whose live regions have all been
    // moved into a backup dir still has to be looked at.
    let mut alternates = alternate_issues(&layer.dir);
    if index.entries.is_empty() && alternates.is_empty() {
        return None;
    }
    let empties = empty_regions(&index);
    let empty = Issue {
        kind: "empty file",
        detail: "0 bytes on disk — the game left a stub behind".to_string(),
        count: empties.len(),
        exact: true,
        examples: empties
            .iter()
            .take(3)
            .map(|p| p.display().to_string())
            .collect(),
    };
    let label = format!("checking {world} {}", layer.label());
    let paths = sample_regions(&index, sample);
    let probes = probe_all(&paths, &label);
    // Group by cause: one bad save version in a layer usually means a hundred
    // thousand of them, and listing each is useless.
    let mut groups: BTreeMap<(&'static str, String), (usize, Vec<String>)> = BTreeMap::new();
    for (path, probe) in paths.iter().zip(&probes) {
        let entry = if let Some(err) = &probe.error {
            let kind = if probe.unsupported {
                "unsupported"
            } else {
                "unreadable"
            };
            Some((kind, err.clone()))
        } else if probe.truncated {
            Some(("truncated", "stream ended mid-structure".to_string()))
        } else {
            None
        };
        if let Some((kind, detail)) = entry {
            let g = groups.entry((kind, detail)).or_insert((0, Vec::new()));
            g.0 += 1;
            if g.1.len() < 3 {
                g.1.push(path.display().to_string());
            }
        }
    }
    let mut issues: Vec<Issue> = Vec::new();
    if empty.count > 0 {
        issues.push(empty);
    }
    // A full pass decodes every region that could hold anything; the zero-byte
    // ones are already counted above rather than probed.
    let probed_everything = paths.len() + empties.len() == index.entries.len();
    issues.extend(
        groups
            .into_iter()
            .map(|((kind, detail), (count, examples))| Issue {
                kind,
                detail,
                count,
                exact: probed_everything,
                examples,
            }),
    );
    issues.append(&mut alternates);
    Some(LayerReport {
        layer,
        regions: index.entries.len() as u64,
        checked: paths.len(),
        issues,
    })
}

/// Region copies the live layer cannot see. `MapSaveLoad.backupFile` *moves*
/// the live file into `<version>_backup_<n>/` before rewriting it at a newer
/// save version, so a crash between the two leaves that copy as the only one
/// there is — 299 such regions on the reference archive. Syncthing conflict
/// files are the same story from the other direction: real data the game will
/// never open again.
fn alternate_issues(dir: &Path) -> Vec<Issue> {
    let Ok(alts) = scan_region_alternates(dir) else {
        return Vec::new();
    };
    let mut orphan_backups: BTreeSet<(i32, i32)> = BTreeSet::new();
    let mut backup_examples: Vec<String> = Vec::new();
    let mut conflicts = 0usize;
    let mut orphan_conflicts = 0usize;
    let mut conflict_examples: Vec<String> = Vec::new();
    for a in &alts {
        match a.source {
            AlternateSource::VersionBackup { .. } if !a.live => {
                if orphan_backups.insert((a.rx, a.rz)) && backup_examples.len() < 3 {
                    backup_examples.push(a.path.display().to_string());
                }
            }
            AlternateSource::SyncConflict { .. } => {
                conflicts += 1;
                if !a.live {
                    orphan_conflicts += 1;
                }
                if conflict_examples.len() < 3 {
                    conflict_examples.push(a.path.display().to_string());
                }
            }
            AlternateSource::VersionBackup { .. } => {}
        }
    }
    let mut out = Vec::new();
    if !orphan_backups.is_empty() {
        out.push(Issue {
            kind: "backup only",
            detail: "no live region at these coordinates — the pre-conversion \
                     backup is the only copy left"
                .to_string(),
            count: orphan_backups.len(),
            exact: true,
            examples: backup_examples,
        });
    }
    if conflicts > 0 {
        out.push(Issue {
            kind: "sync conflict",
            detail: format!(
                "Syncthing conflict copy, invisible to the game ({orphan_conflicts} with no live \
                 region beside it)"
            ),
            count: conflicts,
            exact: true,
            examples: conflict_examples,
        });
    }
    out
}

fn check_dbs(w: &xaerotools_server::WorldEntry) -> Vec<DbReport> {
    let Some(wm) = &w.world.world_map_path else {
        return Vec::new();
    };
    // Only "will it open" — XaeroPlusDrawing.db and friends legitimately hold
    // tables that are not chunk highlights, so table shape proves nothing.
    let mut out = Vec::new();
    for name in &w.world.databases {
        if let Err(e) = xaero_db::open_readonly(&wm.join(name)) {
            out.push(DbReport {
                db: name.clone(),
                error: e,
            });
        }
    }
    out
}

fn print_world(id: &str, reports: &[LayerReport], dbs: &[DbReport]) {
    println!("\n{id}");
    let mut clean = true;
    for r in reports {
        if r.issues.is_empty() {
            continue;
        }
        clean = false;
        println!(
            "  {}  ({} of {} regions checked)",
            r.layer.label(),
            fmt_int(r.checked as u64),
            fmt_int(r.regions)
        );
        for issue in &r.issues {
            let scope = if issue.exact {
                "exact".to_string()
            } else {
                format!(
                    "{:.0}% of sample",
                    100.0 * issue.count as f64 / r.checked.max(1) as f64
                )
            };
            println!(
                "    {:<14} {:>7} ({scope})  {}",
                issue.kind,
                fmt_int(issue.count as u64),
                issue.detail
            );
            for ex in &issue.examples {
                println!("        {ex}");
            }
        }
    }
    for d in dbs {
        clean = false;
        println!("  db {}: {}", d.db, d.error);
    }
    if clean {
        println!("  all clear");
    }
}

fn layer_json(r: &LayerReport) -> serde_json::Value {
    json!({
        "dim": r.layer.dim,
        "mw": r.layer.mw,
        "layer": r.layer.layer_name(),
        "path": r.layer.dir.display().to_string(),
        "regions": r.regions,
        "checked": r.checked,
        "issues": r.issues.iter().map(|i| json!({
            "kind": i.kind,
            "detail": i.detail,
            "count": i.count,
            "exact": i.exact,
            "examples": i.examples,
        })).collect::<Vec<_>>(),
    })
}
