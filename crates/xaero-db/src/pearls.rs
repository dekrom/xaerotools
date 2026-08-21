//! `xaeroplus-pearls.json` — stasis-chamber ender pearls tracked by the mod.
//!
//! Written next to the world's DBs by XaeroPlus's `Pearls` module as
//! `Map<ownerUuid, Map<pearlUuid, Pearl>>` (Gson, pretty-printed). Only the
//! local player's own pearls are tracked, and only once they have lived >= 20
//! ticks and gone near-stationary — i.e. stasis chambers. x/y/z are BLOCK
//! coordinates. In game they surface as a waypoint set named "Pearl".
//!
//! It is not SQLite, but it lives inside the world folder alongside the DBs,
//! so it is read here rather than in the region scanner.

use std::collections::BTreeMap;
use std::path::Path;

/// File name inside a world folder.
pub const PEARLS_FILE: &str = "xaeroplus-pearls.json";

/// One tracked pearl, as stored (`Pearls.Pearl`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Pearl {
    /// The thrown-entity UUID; also the key it is stored under.
    pub uuid: String,
    /// Dimension resource key, e.g. "minecraft:the_nether".
    #[serde(rename = "dimensionKey")]
    pub dimension_key: String,
    /// BLOCK coordinates.
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

/// A pearl together with the owner it is filed under.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OwnedPearl {
    /// UUID of the player who threw it.
    pub owner: String,
    #[serde(flatten)]
    pub pearl: Pearl,
}

/// Parses the file's contents. Unknown owners/pearls are kept as-is; a `{}`
/// file (the common case — the real 2b2t archive's is exactly that) yields an
/// empty list, which is a valid answer and not an error.
pub fn parse(json: &str) -> Result<Vec<OwnedPearl>, String> {
    let raw: BTreeMap<String, BTreeMap<String, Pearl>> =
        serde_json::from_str(json).map_err(|e| format!("parse {PEARLS_FILE}: {e}"))?;
    let mut out = Vec::new();
    for (owner, pearls) in raw {
        for (_key, pearl) in pearls {
            out.push(OwnedPearl {
                owner: owner.clone(),
                pearl,
            });
        }
    }
    Ok(out)
}

/// Reads `<world_dir>/xaeroplus-pearls.json`. A missing file is not an error —
/// most worlds have never had the module enabled.
pub fn read_world(world_dir: &Path) -> Result<Vec<OwnedPearl>, String> {
    let path = world_dir.join(PEARLS_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_file() {
        assert!(parse("{}").unwrap().is_empty());
    }

    #[test]
    fn parses_owner_keyed_map() {
        let json = r#"{
          "0dbb6d5c-1111-4444-8888-000000000001": {
            "5f2e0a10-2222-4444-8888-000000000002": {
              "uuid": "5f2e0a10-2222-4444-8888-000000000002",
              "dimensionKey": "minecraft:the_nether",
              "x": -812,
              "y": 122,
              "z": 3401
            }
          }
        }"#;
        let pearls = parse(json).unwrap();
        assert_eq!(pearls.len(), 1);
        assert_eq!(pearls[0].owner, "0dbb6d5c-1111-4444-8888-000000000001");
        assert_eq!(pearls[0].pearl.dimension_key, "minecraft:the_nether");
        assert_eq!(
            (pearls[0].pearl.x, pearls[0].pearl.y, pearls[0].pearl.z),
            (-812, 122, 3401)
        );
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("xt-pearls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read_world(&dir).unwrap().is_empty());
        std::fs::write(dir.join(PEARLS_FILE), "{}").unwrap();
        assert!(read_world(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
