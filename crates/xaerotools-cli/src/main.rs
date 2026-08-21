//! xaerotools CLI. `print_help` below is the list of subcommands; the three
//! read-only archivist ones live in their own modules:
//!   render — stitch a block-coordinate box of one layer into a PNG
//!   stats  — make an archive describe itself
//!   doctor — report what cannot be read, or is only left as a copy
//! `render-region` stays here as the dev aid it always was: one region, one PNG.

mod archive;
mod doctor;
mod render;
mod stats;

use std::path::PathBuf;

use xaero_core::render::{ColorTable, LightMode, RenderOpts};

pub(crate) static COLORTABLE: &[u8] = include_bytes!("../../../assets/colortable.bin");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("render-region") => render_region_cmd(&args[1..]),
        Some("render") => render::render_cmd(&args[1..]),
        Some("stats") => stats::stats_cmd(&args[1..]),
        Some("doctor") => doctor::doctor_cmd(&args[1..]),
        Some("serve") => serve_cmd(&args[1..]),
        Some("merge") => merge_cmd(&args[1..]),
        Some("db-merge") => db_merge_cmd(&args[1..]),
        Some("waypoints") => waypoints_cmd(&args[1..]),
        Some("tokens") => tokens_cmd(&args[1..]),
        None => serve_cmd(&[]),
        _ => {
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    eprintln!("XaeroTools — view, merge and protect your Xaero's World Map data");
    eprintln!();
    eprintln!("USAGE:");
    eprintln!(
        "  xaerotools                    open your maps in the browser (auto-detects .minecraft)"
    );
    eprintln!("  xaerotools serve [--root PATH]... [--port N] [--open]");
    eprintln!("                   [--lan --password PW] [--vault PATH] [--atlas-dir PATH]");
    eprintln!("                   [--config PATH] [--ingest-dir PATH] [--ingest-no-caves]");
    eprintln!("                   [--live-poll]");
    eprintln!("  xaerotools render --bbox x1,z1,x2,z2 | --all  -o out.png");
    eprintln!("                   [--root PATH]... [--world W] [--dim D] [--mw MW]");
    eprintln!("                   [--cave N | --layer surface|cave:N]");
    eprintln!("                   [--zoom 0..-9 | --scale PX] [--max-px N]");
    eprintln!("  xaerotools stats  [--root PATH]... [--world W] [--sample N | --full]");
    eprintln!("                   [--no-dbs] [--json]");
    eprintln!("  xaerotools doctor [--root PATH]... [--world W] [--sample N | --full] [--json]");
    eprintln!("  xaerotools merge <A> <B> -o OUT [--apply] [--prefer mtime|a|b]");
    eprintln!("                   [--server NAME]... [--alias X=Y]... [--yes] [--json]");
    eprintln!("  xaerotools db-merge <BASE.db> <SRC.db>... [-o OUT.db] [--apply] [--json]");
    eprintln!("  xaerotools waypoints sync   [--root PATH]... [--vault PATH]");
    eprintln!("  xaerotools waypoints list   [--world W] [--archived-only] [--vault PATH]");
    eprintln!("  xaerotools waypoints export --world W -o DIR [--include-archived] [--vault PATH]");
    eprintln!("  xaerotools tokens generate <player> [--config PATH]");
    eprintln!("  xaerotools tokens list|revoke <player> [--config PATH]");
    eprintln!("  xaerotools render-region <region.zip> -o out.png [--cave] [--debug-missing]");
    eprintln!();
    eprintln!("The waypoint vault backs up every waypoint it ever sees (all accounts,");
    eprintln!("all instances) — deleting one in game never removes it from the vault.");
    eprintln!("Sync runs automatically whenever the viewer starts.");
    eprintln!();
    eprintln!("Tokens authenticate POST /ingest/v1/position (live player markers) and");
    eprintln!("POST /ingest/v1/region (map upload: per-player backup + merged group map);");
    eprintln!("a running server picks up generate/revoke within a second.");
    eprintln!();
    eprintln!("render exports a block-coordinate box as one PNG (--bbox is snapped");
    eprintln!("outward to whole 512-block regions); stats and doctor are read-only");
    eprintln!("surveys that sample regions unless --full is given. doctor reports");
    eprintln!("what it cannot read, and regions left only in a backup or conflict");
    eprintln!("copy; findings are not errors, so it exits 0 unless it could not run.");
}

// ------------------------------------------------------------ ingest tokens --

fn tokens_cmd(args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let rest = if args.is_empty() { args } else { &args[1..] };
    let mut config_path: Option<PathBuf> = None;
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--config" => {
                i += 1;
                config_path = Some(PathBuf::from(&rest[i]));
            }
            other if !other.starts_with('-') => names.push(other.to_string()),
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    use xaerotools_server::config;
    let path = config_path.unwrap_or_else(config::default_config_path);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // Mutations lock + re-load so they never clobber a concurrent write by a
    // running server (roots added via the web UI).
    match sub {
        "generate" => {
            let Some(player) = names.first() else {
                eprintln!("usage: xaerotools tokens generate <player> [--config PATH]");
                std::process::exit(2);
            };
            let token = config::with_file_lock(&path, || {
                let mut cfg = config::load(&path)?;
                let token = cfg.set_token(player, now);
                config::save(&path, &cfg)?;
                Ok::<String, String>(token)
            })
            .unwrap_or_else(|e| {
                eprintln!("config: {e}");
                std::process::exit(1);
            });
            println!(
                "token for {player} (shown once — store it in the client config, never in shell args):"
            );
            println!("{token}");
            println!();
            println!(
                "use it as:  Authorization: Bearer <token>  on POST /ingest/v1/{{position,region}}"
            );
        }
        "list" => {
            let cfg = config::load(&path).unwrap_or_else(|e| {
                eprintln!("config: {e}");
                std::process::exit(1);
            });
            if cfg.tokens.is_empty() {
                println!("no tokens — create one with: xaerotools tokens generate <player>");
            }
            for t in &cfg.tokens {
                let age_days = now.saturating_sub(t.created_ms) / 86_400_000;
                println!(
                    "{}  {}…  created {}",
                    t.player,
                    &t.token[..8.min(t.token.len())],
                    if age_days == 0 {
                        "today".to_string()
                    } else {
                        format!("{age_days}d ago")
                    }
                );
            }
        }
        "revoke" => {
            let Some(player) = names.first() else {
                eprintln!("usage: xaerotools tokens revoke <player> [--config PATH]");
                std::process::exit(2);
            };
            let revoked = config::with_file_lock(&path, || {
                let mut cfg = config::load(&path)?;
                let revoked = cfg.revoke_token(player);
                if revoked {
                    config::save(&path, &cfg)?;
                }
                Ok::<bool, String>(revoked)
            })
            .unwrap_or_else(|e| {
                eprintln!("config: {e}");
                std::process::exit(1);
            });
            if !revoked {
                eprintln!("no token for {player}");
                std::process::exit(1);
            }
            println!("revoked {player}'s token (a running server notices within ~1s)");
        }
        _ => {
            eprintln!("usage: xaerotools tokens <generate|list|revoke> (see xaerotools help)");
            std::process::exit(2);
        }
    }
}

// --------------------------------------------------------- waypoints vault --

fn waypoints_cmd(args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    let rest = if args.is_empty() { args } else { &args[1..] };
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut vault_path: Option<PathBuf> = None;
    let mut world: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut archived_only = false;
    let mut include_archived = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--root" => {
                i += 1;
                roots.push(PathBuf::from(&rest[i]));
            }
            "--vault" => {
                i += 1;
                vault_path = Some(PathBuf::from(&rest[i]));
            }
            "--world" => {
                i += 1;
                world = Some(rest[i].clone());
            }
            "-o" => {
                i += 1;
                out = Some(PathBuf::from(&rest[i]));
            }
            "--archived-only" => archived_only = true,
            "--include-archived" => include_archived = true,
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let vault_path = vault_path.unwrap_or_else(xaero_db::vault::default_vault_path);
    let mut vault = xaero_db::vault::Vault::open(&vault_path).unwrap_or_else(|e| {
        eprintln!("vault: {e}");
        std::process::exit(1);
    });

    match sub {
        "sync" => {
            if roots.is_empty() {
                roots = xaero_scan::default_root_candidates();
            }
            if roots.is_empty() {
                eprintln!("no Xaero data found — pass --root <path>");
                std::process::exit(1);
            }
            let worlds = xaerotools_server::discover_worlds(&roots);
            let batches = xaerotools_server::collect_vault_batches(&worlds);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            match vault.sync(&batches, now) {
                Ok(r) => println!(
                    "synced {} live waypoints from {} worlds: {} new, {} revived, {} newly archived\nvault {} now holds {} waypoints ({} archived)",
                    r.seen,
                    worlds.len(),
                    r.added,
                    r.revived,
                    r.newly_archived,
                    vault_path.display(),
                    r.total,
                    r.archived_total
                ),
                Err(e) => {
                    eprintln!("sync failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        "list" => match world {
            None => {
                for (world, total, archived) in vault.worlds().unwrap_or_default() {
                    println!("{world}: {total} waypoints ({archived} archived)");
                }
            }
            Some(w) => {
                for wp in vault
                    .waypoints_for_world(&w, archived_only)
                    .unwrap_or_default()
                {
                    println!(
                        "{}{} [{}] {}, {}, {}",
                        wp.name,
                        if wp.present { "" } else { " (archived)" },
                        wp.dim_key,
                        wp.x,
                        wp.y.map(|v| v.to_string()).unwrap_or_else(|| "~".into()),
                        wp.z
                    );
                }
            }
        },
        "export" => {
            let (Some(world), Some(out)) = (world, out) else {
                eprintln!("export needs --world <id> and -o <dir>");
                std::process::exit(2);
            };
            // Collect the (dim, mw_file) groups present for this world.
            let all = vault.waypoints_for_world(&world, false).unwrap_or_default();
            let mut groups: Vec<(String, String)> = all
                .iter()
                .map(|w| (w.dim_key.clone(), w.mw_file.clone()))
                .collect();
            groups.sort();
            groups.dedup();
            if groups.is_empty() {
                eprintln!(
                    "vault has no waypoints for world {world} — run `xaerotools waypoints sync` first"
                );
                std::process::exit(1);
            }
            for (dim_key, mw_file) in groups {
                let dim = match dim_key.as_str() {
                    "minecraft:overworld" => xaero_core::naming::Dimension::Overworld,
                    "minecraft:the_nether" => xaero_core::naming::Dimension::Nether,
                    "minecraft:the_end" => xaero_core::naming::Dimension::End,
                    other => xaero_core::naming::Dimension::Custom(other.to_string()),
                };
                let text = vault
                    .export_file(&world, &dim_key, &mw_file, include_archived)
                    .unwrap_or_else(|e| {
                        eprintln!("export failed: {e}");
                        std::process::exit(1);
                    });
                let dir = out
                    .join("minimap")
                    .join(&world)
                    .join(dim.to_minimap_folder());
                std::fs::create_dir_all(&dir).expect("mkdir export dir");
                let file = dir.join(&mw_file);
                std::fs::write(&file, text).expect("write export");
                println!("wrote {}", file.display());
            }
            println!("\nTo restore in game: copy the exported `minimap/{world}/...` folders into");
            println!(".minecraft/xaero/minimap/ (game closed), replacing or renaming as needed.");
        }
        _ => {
            eprintln!("usage: xaerotools waypoints <sync|list|export> (see xaerotools help)");
            std::process::exit(2);
        }
    }
}

fn merge_cmd(args: &[String]) {
    let mut positional: Vec<PathBuf> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut opts = xaero_merge::MergeOptions::default();
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                out = Some(PathBuf::from(&args[i]));
            }
            "--apply" => opts.apply = true,
            "--yes" => opts.auto_alias = true,
            "--json" => json = true,
            "--server" => {
                i += 1;
                opts.servers.push(args[i].clone());
            }
            "--alias" => {
                i += 1;
                let (x, y) = args[i]
                    .split_once('=')
                    .unwrap_or_else(|| panic!("--alias must be A-id=B-id"));
                opts.aliases.push((x.to_string(), y.to_string()));
            }
            "--prefer" => {
                i += 1;
                opts.prefer = match args[i].as_str() {
                    "a" => xaero_merge::Prefer::A,
                    "b" => xaero_merge::Prefer::B,
                    _ => xaero_merge::Prefer::Mtime,
                };
            }
            other if !other.starts_with('-') => positional.push(PathBuf::from(other)),
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    if positional.len() != 2 {
        eprintln!("merge needs exactly two source roots (A B) and -o OUT");
        std::process::exit(2);
    }
    let out = out.unwrap_or_else(|| {
        eprintln!("merge needs -o OUT (a fresh output directory)");
        std::process::exit(2);
    });
    if out.exists()
        && out
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(true)
    {
        eprintln!("output {} already exists and is not empty", out.display());
        std::process::exit(1);
    }
    let report = match xaero_merge::merge_to_output(&positional[0], &positional[1], &out, &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("merge failed: {e}");
            std::process::exit(1);
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
        return;
    }
    print_merge_report(&report, &out);
}

fn print_merge_report(r: &xaero_merge::MergeReport, out: &std::path::Path) {
    if !r.suggested_aliases.is_empty() {
        println!("Suggested world pairings (re-run with --yes or --alias to accept):");
        for (a, b) in &r.suggested_aliases {
            println!("  --alias \"{a}={b}\"");
        }
        println!();
    }
    for (a, b) in &r.world_pairs {
        println!("merging world: {a}  +  {b}");
    }
    for w in &r.only_worlds {
        println!("copying whole world: {w}");
    }
    println!();
    println!(
        "{:<28} {:>8} {:>8} {:>9}",
        "unit", "A only", "B only", "conflicts"
    );
    for u in &r.units {
        let layer = match u.cave {
            None => String::new(),
            Some(n) => format!(" cave:{n}"),
        };
        println!(
            "{:<28} {:>8} {:>8} {:>9}",
            format!("{}/{}{layer}", u.dim, u.mw),
            u.only_a,
            u.only_b,
            u.conflicts
        );
        for e in &u.merge_errors {
            println!("    ! {e}");
        }
    }
    println!("total region files out: {}", r.total_regions_out());
    for db in &r.dbs {
        for t in &db.tables {
            println!(
                "db {} [{}]: {} + {} rows, {} overlap -> {}",
                std::path::Path::new(&db.dest)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
                t.table,
                t.dest_rows_before,
                t.source_rows,
                t.overlap,
                t.dest_rows_after
            );
        }
    }
    println!("waypoint files merged: {}", r.waypoint_files_merged);
    if r.applied {
        println!("\nDONE — merged data written to {}", out.display());
        println!(
            "Open it with: xaerotools serve --root \"{}\"",
            out.display()
        );
    } else {
        println!(
            "\nDRY RUN — nothing written. Re-run with --apply to merge into {}",
            out.display()
        );
    }
}

fn db_merge_cmd(args: &[String]) {
    let mut positional: Vec<PathBuf> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut apply = false;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                out = Some(PathBuf::from(&args[i]));
            }
            "--apply" => apply = true,
            "--json" => json = true,
            other if !other.starts_with('-') => positional.push(PathBuf::from(other)),
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    if positional.len() < 2 {
        eprintln!("db-merge needs a BASE.db and at least one SRC.db");
        std::process::exit(2);
    }
    let base = positional[0].clone();
    let dest = match &out {
        Some(o) => {
            if apply {
                if o.exists() {
                    eprintln!("output {} already exists", o.display());
                    std::process::exit(1);
                }
                std::fs::copy(&base, o).unwrap_or_else(|e| {
                    eprintln!("copy base: {e}");
                    std::process::exit(1);
                });
            }
            if apply { o.clone() } else { base.clone() }
        }
        None => base.clone(),
    };
    if apply && out.is_none() {
        eprintln!(
            "note: merging IN PLACE into {} (use -o OUT.db to keep it untouched)",
            dest.display()
        );
    }
    let sources: Vec<&std::path::Path> = positional[1..].iter().map(|p| p.as_path()).collect();
    match xaero_db::merge::merge_into(&dest, &sources, apply) {
        Ok(report) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                for t in &report.tables {
                    println!(
                        "[{}] {} + {} rows, {} overlap -> {}",
                        t.table, t.dest_rows_before, t.source_rows, t.overlap, t.dest_rows_after
                    );
                }
                if apply {
                    println!("DONE -> {}", dest.display());
                } else {
                    println!("DRY RUN — nothing written. Re-run with --apply.");
                }
            }
        }
        Err(e) => {
            eprintln!("db-merge failed: {e}");
            std::process::exit(1);
        }
    }
}

fn serve_cmd(args: &[String]) {
    let mut config = xaerotools_server::ServerConfig::default();
    let mut open = false;
    let mut lan = false;
    let mut port: u16 = 45746;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--root" => {
                i += 1;
                config.roots.push(PathBuf::from(&args[i]));
            }
            "--port" => {
                i += 1;
                port = args[i].parse().expect("--port must be a number");
            }
            "--lan" => lan = true,
            "--password" => {
                i += 1;
                config.password = Some(args[i].clone());
            }
            "--vault" => {
                i += 1;
                config.vault_path = Some(PathBuf::from(&args[i]));
            }
            "--atlas-dir" => {
                i += 1;
                config.atlas_dir = Some(PathBuf::from(&args[i]));
            }
            "--config" => {
                i += 1;
                config.config_path = Some(PathBuf::from(&args[i]));
            }
            "--ingest-dir" => {
                i += 1;
                config.ingest_dir = Some(PathBuf::from(&args[i]));
            }
            "--ingest-no-caves" => config.ingest_no_caves = true,
            "--live-poll" => config.live_poll = true,
            "--open" => open = true,
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    if lan && config.password.is_none() {
        eprintln!(
            "--lan requires --password <pw> (everyone on your network could see your map otherwise)"
        );
        std::process::exit(2);
    }
    if lan {
        eprintln!(
            "LAN mode: password required. Plain HTTP — for access beyond your LAN prefer a VPN like Tailscale."
        );
    }
    let host: std::net::IpAddr = if lan {
        [0, 0, 0, 0].into()
    } else {
        [127, 0, 0, 1].into()
    };
    // If the chosen port is taken (another instance, another app), walk
    // forward a little instead of failing — double-click must Just Work.
    // Exhausting the range has to say so: falling through to the raw bind
    // error just tells the user "Address already in use" about a port they
    // never picked.
    let last = port.saturating_add(19);
    let chosen = (port..=last)
        .find(|&candidate| std::net::TcpListener::bind((host, candidate)).is_ok())
        .unwrap_or_else(|| {
            eprintln!("ports {port}..{last} are all in use — pass --port N");
            std::process::exit(2);
        });
    if chosen != port {
        eprintln!("port {port} is busy — using {chosen} instead");
    }
    config.bind = (host, chosen).into();
    // Roots persisted in the config file (added via the web UI) count too.
    let config_path = config
        .config_path
        .clone()
        .unwrap_or_else(xaerotools_server::config::default_config_path);
    let persisted_roots = xaerotools_server::config::load(&config_path)
        .map(|c| c.roots)
        .unwrap_or_default();
    if config.roots.is_empty() && persisted_roots.is_empty() {
        config.roots = xaero_scan::default_root_candidates();
        if config.roots.is_empty() {
            eprintln!(
                "no Xaero data found automatically — pass --root <path to .minecraft or xaero folder>"
            );
            std::process::exit(1);
        }
        for r in &config.roots {
            eprintln!("auto-detected root: {}", r.display());
        }
    }
    let mut preview_roots = config.roots.clone();
    for r in persisted_roots {
        if !preview_roots.contains(&r) {
            preview_roots.push(r);
        }
    }
    let preview = xaerotools_server::discover_worlds(&preview_roots);
    eprintln!("{} world(s) discovered:", preview.len());
    for w in &preview {
        let regions: usize = 0;
        let _ = regions;
        eprintln!(
            "  {} ({} dims, {} DBs{})",
            w.world.id,
            w.world.dims.len(),
            w.world.databases.len(),
            if w.world.waypoint_files.is_empty() {
                ""
            } else {
                ", waypoints"
            }
        );
    }
    let url = format!("http://{}", config.bind);
    eprintln!("XaeroTools listening on {url}");
    if open {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(xaerotools_server::run(config)) {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}

fn render_region_cmd(args: &[String]) {
    let mut input: Option<PathBuf> = None;
    let mut output = PathBuf::from("region.png");
    let mut opts = RenderOpts::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                output = PathBuf::from(&args[i]);
            }
            "--cave" => opts.light_mode = LightMode::Multiply,
            "--debug-missing" => opts.debug_missing = true,
            other => input = Some(PathBuf::from(other)),
        }
        i += 1;
    }
    let input = input.expect("need a region file");
    let ct = ColorTable::parse(COLORTABLE).expect("embedded color table");
    let bytes = std::fs::read(&input).expect("read region");
    let stream = xaero_core::read_region_container(&bytes).expect("container");
    let dec = xaero_core::decode_region(&stream).expect("decode");
    eprintln!(
        "{}: v{} chunks={} truncated={} palette={} biomes={}",
        input.display(),
        dec.version,
        dec.region.chunks.len(),
        dec.truncated,
        dec.palettes.states.len(),
        dec.palettes.biome_names.len()
    );
    if std::env::var_os("XT_DEBUG").is_some() {
        let mut counts: std::collections::HashMap<&str, usize> = Default::default();
        for (_, chunk) in &dec.region.chunks {
            for tile in chunk.tiles.iter().flatten() {
                for px in &tile.pixels {
                    let name = match px.state {
                        None => "minecraft:grass_block",
                        Some(i) => dec
                            .palettes
                            .state_names
                            .get(i as usize)
                            .map(|s| s.as_str())
                            .unwrap_or("<dangling>"),
                    };
                    *counts.entry(name).or_default() += 1;
                }
            }
        }
        let mut top: Vec<_> = counts.into_iter().collect();
        top.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        for (name, n) in top.iter().take(12) {
            let c = ct.block(name);
            eprintln!(
                "  {n:>8} {name}  rgba={:?} tint={:?} missing={}",
                c.rgba, c.tint, c.missing
            );
        }
    }
    let rgba = xaero_core::render::render_region(&dec, &ct, &opts);
    write_png(&output, &rgba, 512, 512);
    eprintln!("wrote {}", output.display());
}

fn write_png(path: &PathBuf, rgba: &[u8], w: u32, h: u32) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().expect("png header");
    writer.write_image_data(rgba).expect("png data");
}
