//! xaero-colorgen — dev-time generator of `assets/colortable.bin` (XCT1).
//!
//! Downloads (or accepts) an official Minecraft client jar, derives one
//! average RGBA per block from its top-face texture, classifies biome tinting,
//! samples the grass/foliage colormaps per biome, and writes a compact binary
//! artifact consumed by xaero-core's renderer. Never shipped to users; the
//! artifact contains only derived per-block averages.
//!
//! Usage:
//!   xaero-colorgen --mc-version 1.21.8 --out ../../assets/colortable.bin
//!   xaero-colorgen --jar /path/to/client.jar --out colortable.bin

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

fn main() {
    let mut mc_version = String::from("1.21.8");
    let mut jar_path: Option<PathBuf> = None;
    let mut out_path = PathBuf::from("colortable.bin");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--mc-version" => mc_version = args.next().expect("--mc-version needs a value"),
            "--jar" => jar_path = Some(PathBuf::from(args.next().expect("--jar needs a value"))),
            "--out" => out_path = PathBuf::from(args.next().expect("--out needs a value")),
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }

    let jar_bytes = match jar_path {
        Some(p) => std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())),
        None => fetch_client_jar(&mc_version),
    };
    let mut jar = zip::ZipArchive::new(std::io::Cursor::new(jar_bytes)).expect("open client jar");

    let table = build_table(&mut jar, &mc_version);
    let bin = serialize(&table);
    std::fs::write(&out_path, &bin).unwrap_or_else(|e| panic!("write {}: {e}", out_path.display()));
    println!(
        "wrote {} ({} blocks, {} aliases, {} biomes, {} bytes) for MC {}",
        out_path.display(),
        table.blocks.len(),
        table.aliases.len(),
        table.biomes.len(),
        bin.len(),
        table.mc_version
    );
    if !table.missing.is_empty() {
        println!(
            "{} blocks had no resolvable texture (baked gray, flagged):",
            table.missing.len()
        );
        for name in table.missing.iter().take(15) {
            println!("  {name}");
        }
        if table.missing.len() > 15 {
            println!("  ... and {} more", table.missing.len() - 15);
        }
    }
}

// ---------------------------------------------------------------- download --

fn http_get(url: &str) -> Vec<u8> {
    let mut res = ureq::get(url)
        .call()
        .unwrap_or_else(|e| panic!("GET {url}: {e}"));
    let mut buf = Vec::new();
    res.body_mut()
        .as_reader()
        .read_to_end(&mut buf)
        .unwrap_or_else(|e| panic!("GET {url} body: {e}"));
    buf
}

fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .expect("no HOME");
            Path::new(&home).join(".cache")
        });
    base.join("xaerotools").join("client-jars")
}

fn fetch_client_jar(version: &str) -> Vec<u8> {
    let cache = cache_dir();
    let cached = cache.join(format!("{version}.jar"));
    if let Ok(bytes) = std::fs::read(&cached) {
        eprintln!("using cached {}", cached.display());
        return bytes;
    }
    eprintln!("fetching version manifest…");
    let manifest: Value = serde_json::from_slice(&http_get(MANIFEST_URL)).unwrap();
    let ver = manifest["versions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == version)
        .unwrap_or_else(|| panic!("version {version} not in manifest"));
    let ver_json: Value = serde_json::from_slice(&http_get(ver["url"].as_str().unwrap())).unwrap();
    let dl = &ver_json["downloads"]["client"];
    let url = dl["url"].as_str().unwrap();
    let expected_sha1 = dl["sha1"].as_str().unwrap();
    eprintln!("downloading client jar for {version}…");
    let bytes = http_get(url);
    let actual = sha1_hex(&bytes);
    assert_eq!(actual, expected_sha1, "client jar sha1 mismatch");
    let _ = std::fs::create_dir_all(&cache);
    let _ = std::fs::write(&cached, &bytes);
    bytes
}

fn sha1_hex(data: &[u8]) -> String {
    let mut h = sha1_smol::Sha1::new();
    h.update(data);
    h.digest().to_string()
}

// ------------------------------------------------------------------- model --

#[derive(Clone, Copy, PartialEq)]
enum Tint {
    None = 0,
    Grass = 1,
    Foliage = 2,
    DryFoliage = 3,
    Water = 4,
}

struct BlockEntry {
    rgba: [u8; 4],
    tint: Tint,
    missing: bool,
}

struct BiomeEntry {
    grass: [u8; 3],
    foliage: [u8; 3],
    water: [u8; 3],
    dry_foliage: [u8; 3],
}

struct Table {
    mc_version: String,
    blocks: BTreeMap<String, BlockEntry>,
    aliases: Vec<(String, String)>,
    biomes: BTreeMap<String, BiomeEntry>,
    missing: Vec<String>,
}

type Jar<'a> = zip::ZipArchive<std::io::Cursor<Vec<u8>>>;

fn jar_read(jar: &mut Jar, path: &str) -> Option<Vec<u8>> {
    let mut f = jar.by_name(path).ok()?;
    let mut buf = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn jar_json(jar: &mut Jar, path: &str) -> Option<Value> {
    serde_json::from_slice(&jar_read(jar, path)?).ok()
}

fn build_table(jar: &mut Jar, mc_version: &str) -> Table {
    // Enumerate blockstates and biome definitions first (needs the name list).
    let mut blockstate_names: Vec<String> = Vec::new();
    let mut biome_names: Vec<String> = Vec::new();
    for i in 0..jar.len() {
        let name = jar
            .by_index_raw(i)
            .map(|f| f.name().to_string())
            .unwrap_or_default();
        if let Some(rest) = name.strip_prefix("assets/minecraft/blockstates/") {
            if let Some(base) = rest.strip_suffix(".json") {
                blockstate_names.push(base.to_string());
            }
        } else if let Some(rest) = name.strip_prefix("data/minecraft/worldgen/biome/") {
            if let Some(base) = rest.strip_suffix(".json") {
                biome_names.push(base.to_string());
            }
        }
    }
    blockstate_names.sort();
    biome_names.sort();
    assert!(!blockstate_names.is_empty(), "no blockstates in jar");
    assert!(
        !biome_names.is_empty(),
        "no worldgen biomes in jar (need a client jar for 1.19+)"
    );

    let mut model_cache: HashMap<String, Option<Value>> = HashMap::new();
    let mut texture_cache: HashMap<String, Option<[u8; 4]>> = HashMap::new();
    let mut blocks = BTreeMap::new();
    let mut missing = Vec::new();

    for base in &blockstate_names {
        let id = format!("minecraft:{base}");
        if matches!(
            base.as_str(),
            "air"
                | "cave_air"
                | "void_air"
                | "moving_piston"
                | "light"
                | "barrier"
                | "structure_void"
        ) {
            blocks.insert(
                id,
                BlockEntry {
                    rgba: [0, 0, 0, 0],
                    tint: Tint::None,
                    missing: false,
                },
            );
            continue;
        }
        let tint = classify_tint(base);
        let avg = block_average(jar, base, &mut model_cache, &mut texture_cache);
        let (rgba, is_missing) = match avg {
            Some(mut c) => {
                if let Some(fixed) = fixed_tint(base) {
                    c = multiply(c, fixed);
                }
                (c, false)
            }
            None => {
                missing.push(id.clone());
                ([0x7F, 0x7F, 0x7F, 0xFF], true)
            }
        };
        blocks.insert(
            id,
            BlockEntry {
                rgba,
                tint,
                missing: is_missing,
            },
        );
    }

    // Biomes.
    let grass_map = load_colormap(jar, "grass");
    let foliage_map = load_colormap(jar, "foliage");
    let dry_map = load_colormap_opt(jar, "dry_foliage");
    let mut biomes = BTreeMap::new();
    for base in &biome_names {
        let id = format!("minecraft:{base}");
        let Some(v) = jar_json(jar, &format!("data/minecraft/worldgen/biome/{base}.json")) else {
            continue;
        };
        let temp = v["temperature"].as_f64().unwrap_or(0.8) as f32;
        let downfall = v["downfall"].as_f64().unwrap_or(0.4) as f32;
        let fx = &v["effects"];
        let sample = |map: &Option<Vec<[u8; 3]>>| -> [u8; 3] {
            map.as_ref()
                .map(|m| sample_colormap(m, temp, downfall))
                .unwrap_or([0x7F, 0xB2, 0x38])
        };
        let mut grass = fx["grass_color"]
            .as_i64()
            .map(rgb_int)
            .unwrap_or_else(|| sample(&grass_map));
        let foliage = fx["foliage_color"]
            .as_i64()
            .map(rgb_int)
            .unwrap_or_else(|| sample(&foliage_map));
        let dry_foliage = fx["dry_foliage_color"]
            .as_i64()
            .map(rgb_int)
            .unwrap_or_else(|| sample(&dry_map));
        match fx["grass_color_modifier"].as_str() {
            Some("swamp") => grass = [0x6A, 0x70, 0x39],
            Some("dark_forest") => {
                let packed =
                    ((grass[0] as u32) << 16 | (grass[1] as u32) << 8 | grass[2] as u32) & 0xFEFEFE;
                let mixed = (packed + 0x28340A) >> 1;
                grass = rgb_int(mixed as i64);
            }
            _ => {}
        }
        let water = fx["water_color"]
            .as_i64()
            .map(rgb_int)
            .unwrap_or([0x3F, 0x76, 0xE4]);
        biomes.insert(
            id,
            BiomeEntry {
                grass,
                foliage,
                water,
                dry_foliage,
            },
        );
    }

    let aliases = vec![
        // Renames within the supported 1.18 -> 1.21.x window (old -> current).
        (
            "minecraft:grass".to_string(),
            "minecraft:short_grass".to_string(),
        ),
    ];

    Table {
        mc_version: mc_version.to_string(),
        blocks,
        aliases,
        biomes,
        missing,
    }
}

fn rgb_int(v: i64) -> [u8; 3] {
    let v = v as u32;
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

fn multiply(c: [u8; 4], t: [u8; 3]) -> [u8; 4] {
    [
        ((c[0] as u16 * t[0] as u16) / 255) as u8,
        ((c[1] as u16 * t[1] as u16) / 255) as u8,
        ((c[2] as u16 * t[2] as u16) / 255) as u8,
        c[3],
    ]
}

/// Blocks whose tint is a biome-independent constant: bake it into the color.
fn fixed_tint(base: &str) -> Option<[u8; 3]> {
    match base {
        "spruce_leaves" => Some([0x61, 0x99, 0x61]),
        "birch_leaves" => Some([0x80, 0xA7, 0x55]),
        "mangrove_leaves" => Some([0x92, 0xC6, 0x48]),
        "lily_pad" => Some([0x20, 0x80, 0x30]),
        _ => None,
    }
}

fn classify_tint(base: &str) -> Tint {
    match base {
        "grass_block" | "short_grass" | "grass" | "tall_grass" | "fern" | "large_fern"
        | "sugar_cane" | "bush" => Tint::Grass,
        "oak_leaves" | "jungle_leaves" | "acacia_leaves" | "dark_oak_leaves" | "vine" => {
            Tint::Foliage
        }
        "leaf_litter" => Tint::DryFoliage,
        "water" | "bubble_column" => Tint::Water,
        _ => Tint::None,
    }
}

// -------------------------------------------------------- model resolution --

fn block_average(
    jar: &mut Jar,
    base: &str,
    model_cache: &mut HashMap<String, Option<Value>>,
    texture_cache: &mut HashMap<String, Option<[u8; 4]>>,
) -> Option<[u8; 4]> {
    // Special cases with no usable blockstate model.
    if base == "water" {
        return texture_average(jar, "minecraft:block/water_still", texture_cache);
    }
    if base == "lava" {
        return texture_average(jar, "minecraft:block/lava_still", texture_cache);
    }

    let bs = jar_json(jar, &format!("assets/minecraft/blockstates/{base}.json"))?;
    let model_id = pick_model(&bs)?;
    let textures = resolve_textures(jar, &model_id, model_cache)?;
    let tex_id = pick_face_texture(&textures)?;
    texture_average(jar, &tex_id, texture_cache)
        // Fallback: a texture directly named after the block.
        .or_else(|| texture_average(jar, &format!("minecraft:block/{base}"), texture_cache))
}

fn pick_model(bs: &Value) -> Option<String> {
    let first_of = |v: &Value| -> Option<String> {
        let obj = if v.is_array() { v.get(0)? } else { v };
        obj["model"].as_str().map(String::from)
    };
    if let Some(variants) = bs["variants"].as_object() {
        if let Some(v) = variants.get("") {
            return first_of(v);
        }
        let mut keys: Vec<_> = variants.keys().collect();
        keys.sort();
        return first_of(variants.get(*keys.first()?)?);
    }
    if let Some(parts) = bs["multipart"].as_array() {
        for part in parts {
            if let Some(m) = first_of(&part["apply"]) {
                return Some(m);
            }
        }
    }
    None
}

/// Walks the parent chain merging `textures` (child wins), then resolves
/// `#ref` indirections.
fn resolve_textures(
    jar: &mut Jar,
    model_id: &str,
    cache: &mut HashMap<String, Option<Value>>,
) -> Option<BTreeMap<String, String>> {
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    let mut current = Some(model_id.to_string());
    let mut hops = 0;
    while let Some(id) = current {
        if hops > 16 {
            break;
        }
        hops += 1;
        let path = model_path(&id);
        let model = cache
            .entry(path.clone())
            .or_insert_with(|| jar_json_by_path(jar, &path))
            .clone()?;
        if let Some(tex) = model["textures"].as_object() {
            for (k, v) in tex {
                if let Some(s) = v.as_str() {
                    merged.entry(k.clone()).or_insert_with(|| s.to_string());
                }
            }
        }
        current = model["parent"].as_str().map(String::from);
    }
    // Resolve #refs.
    let resolved: BTreeMap<String, String> = merged
        .iter()
        .map(|(k, v)| {
            let mut val = v.clone();
            let mut hops = 0;
            while let Some(r) = val.strip_prefix('#') {
                if hops > 16 {
                    break;
                }
                hops += 1;
                match merged.get(r) {
                    Some(next) => val = next.clone(),
                    None => break,
                }
            }
            (k.clone(), val)
        })
        .collect();
    Some(resolved)
}

fn jar_json_by_path(jar: &mut Jar, path: &str) -> Option<Value> {
    jar_json(jar, path)
}

fn model_path(model_id: &str) -> String {
    let id = model_id.strip_prefix("minecraft:").unwrap_or(model_id);
    format!("assets/minecraft/models/{id}.json")
}

fn pick_face_texture(textures: &BTreeMap<String, String>) -> Option<String> {
    for key in [
        "up", "top", "end", "all", "texture", "cross", "side", "particle",
    ] {
        if let Some(v) = textures.get(key) {
            if !v.starts_with('#') {
                return Some(v.clone());
            }
        }
    }
    textures.values().find(|v| !v.starts_with('#')).cloned()
}

// ------------------------------------------------------------------ pixels --

fn texture_average(
    jar: &mut Jar,
    tex_id: &str,
    cache: &mut HashMap<String, Option<[u8; 4]>>,
) -> Option<[u8; 4]> {
    if let Some(hit) = cache.get(tex_id) {
        return *hit;
    }
    let id = tex_id.strip_prefix("minecraft:").unwrap_or(tex_id);
    let path = format!("assets/minecraft/textures/{id}.png");
    let result = jar_read(jar, &path).and_then(|png| average_png(&png));
    cache.insert(tex_id.to_string(), result);
    result
}

/// Average RGBA over visible pixels of the first animation frame.
fn average_png(data: &[u8]) -> Option<[u8; 4]> {
    let (pixels, w, h) = decode_png_rgba(data)?;
    let frame_h = if h > w && h % w == 0 { w } else { h }; // vertical animation strips
    let (mut r, mut g, mut b, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for y in 0..frame_h {
        for x in 0..w {
            let p = &pixels[(y * w + x) * 4..(y * w + x) * 4 + 4];
            a += p[3] as u64;
            if p[3] > 0 {
                r += p[0] as u64 * p[3] as u64;
                g += p[1] as u64 * p[3] as u64;
                b += p[2] as u64 * p[3] as u64;
                n += p[3] as u64;
            }
        }
    }
    if n == 0 {
        return Some([0, 0, 0, 0]);
    }
    let total = (frame_h * w) as u64;
    Some([
        (r / n) as u8,
        (g / n) as u8,
        (b / n) as u8,
        (a / total) as u8,
    ])
}

fn decode_png_rgba(data: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let w = info.width as usize;
    let h = info.height as usize;
    let mut out = vec![0u8; w * h * 4];
    match info.color_type {
        png::ColorType::Rgba => out.copy_from_slice(&buf[..w * h * 4]),
        png::ColorType::Rgb => {
            for i in 0..w * h {
                out[i * 4..i * 4 + 3].copy_from_slice(&buf[i * 3..i * 3 + 3]);
                out[i * 4 + 3] = 255;
            }
        }
        png::ColorType::Grayscale => {
            for i in 0..w * h {
                let v = buf[i];
                out[i * 4..i * 4 + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for i in 0..w * h {
                let v = buf[i * 2];
                out[i * 4..i * 4 + 4].copy_from_slice(&[v, v, v, buf[i * 2 + 1]]);
            }
        }
        png::ColorType::Indexed => return None, // EXPAND should have handled it
    }
    Some((out, w, h))
}

fn load_colormap(jar: &mut Jar, name: &str) -> Option<Vec<[u8; 3]>> {
    let data = jar_read(
        jar,
        &format!("assets/minecraft/textures/colormap/{name}.png"),
    )?;
    let (pixels, w, h) = decode_png_rgba(&data)?;
    if w != 256 || h != 256 {
        return None;
    }
    Some(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| [p[0], p[1], p[2]])
            .collect(),
    )
}

fn load_colormap_opt(jar: &mut Jar, name: &str) -> Option<Vec<[u8; 3]>> {
    load_colormap(jar, name)
}

fn sample_colormap(map: &[[u8; 3]], temperature: f32, downfall: f32) -> [u8; 3] {
    let t = temperature.clamp(0.0, 1.0);
    let d = downfall.clamp(0.0, 1.0) * t;
    let x = ((1.0 - t) * 255.0) as usize;
    let y = ((1.0 - d) * 255.0) as usize;
    map[y.min(255) * 256 + x.min(255)]
}

// --------------------------------------------------------------- serialize --

/// XCT1 layout (big-endian):
///   "XCT1" u16 fmt=1
///   u16 len + mc_version utf8
///   u32 block_count { u16 len + name, rgba[4], tint u8, flags u8 }
///   u32 alias_count { u16 len + from, u16 len + to }
///   u32 biome_count { u16 len + name, grass[3], foliage[3], water[3], dry[3] }
fn serialize(t: &Table) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 * 1024);
    out.extend_from_slice(b"XCT1");
    out.extend_from_slice(&1u16.to_be_bytes());
    put_str(&mut out, &t.mc_version);
    out.extend_from_slice(&(t.blocks.len() as u32).to_be_bytes());
    for (name, e) in &t.blocks {
        put_str(&mut out, name);
        out.extend_from_slice(&e.rgba);
        out.push(e.tint as u8);
        out.push(if e.missing { 1 } else { 0 });
    }
    out.extend_from_slice(&(t.aliases.len() as u32).to_be_bytes());
    for (from, to) in &t.aliases {
        put_str(&mut out, from);
        put_str(&mut out, to);
    }
    out.extend_from_slice(&(t.biomes.len() as u32).to_be_bytes());
    for (name, b) in &t.biomes {
        put_str(&mut out, name);
        out.extend_from_slice(&b.grass);
        out.extend_from_slice(&b.foliage);
        out.extend_from_slice(&b.water);
        out.extend_from_slice(&b.dry_foliage);
    }
    out
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}
