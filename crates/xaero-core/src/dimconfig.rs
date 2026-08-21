//! Parsers for Xaero's plain-text config files that describe the on-disk data:
//! - world-map per-dimension `dimension_config.txt` (authoritative multiworld
//!   names and the real `dimensionTypeId`)
//! - world-map per-server `server_config.txt`
//! - minimap per-server `config.txt` (carries `dimensionType:` mappings)
//!
//! All parsers preserve unknown lines so merges never drop future keys.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DimensionConfig {
    /// `MWName:<mwFolderId>:<displayName>` entries (repeatable).
    pub multiworld_names: Vec<(String, String)>,
    pub cave_mode_type: Option<i32>,
    /// e.g. "minecraft:the_nether" — which vanilla dimension this behaves like.
    pub dimension_type_id: Option<String>,
    pub other_lines: Vec<String>,
}

pub fn parse_dimension_config(text: &str) -> DimensionConfig {
    let mut out = DimensionConfig::default();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("MWName:")
            && let Some((mw, name)) = rest.split_once(':')
        {
            // `^col^` is how the mod escapes a literal ':' in a display name
            // (MapDimension.loadConfig replaces it back on read).
            out.multiworld_names
                .push((mw.to_string(), name.replace("^col^", ":")));
            continue;
        }
        if let Some(rest) = line.strip_prefix("caveModeType:")
            && let Ok(v) = rest.parse()
        {
            out.cave_mode_type = Some(v);
            continue;
        }
        if let Some(rest) = line.strip_prefix("dimensionTypeId:") {
            out.dimension_type_id = Some(rest.to_string());
            continue;
        }
        out.other_lines.push(line.to_string());
    }
    out
}

impl DimensionConfig {
    pub fn multiworld_display_name<'a>(&'a self, mw_folder: &'a str) -> &'a str {
        self.multiworld_names
            .iter()
            .find(|(mw, _)| mw == mw_folder)
            .map(|(_, name)| name.as_str())
            .unwrap_or(mw_folder)
    }

    /// `caveModeType` as the mod names it (GuiCaveModeOptions): 0 off,
    /// 1 layered (`caves/<topY>>4>`), 2 full (one `caves/<MIN_VALUE>` layer).
    pub fn cave_mode_name(&self) -> Option<&'static str> {
        match self.cave_mode_type? {
            0 => Some("Off"),
            1 => Some("Layered"),
            2 => Some("Full"),
            _ => None,
        }
    }
}

/// Generic `key:value` config (used for `server_config.txt`). Order-preserving.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeyValueConfig {
    pub entries: Vec<(String, String)>,
}

pub fn parse_key_value_config(text: &str) -> KeyValueConfig {
    let mut out = KeyValueConfig::default();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        match line.split_once(':') {
            Some((k, v)) => out.entries.push((k.to_string(), v.to_string())),
            None => out.entries.push((line.to_string(), String::new())),
        }
    }
    out
}

impl KeyValueConfig {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Minimap `config.txt`: general key/values plus `dimensionType:<levelId>:<dimTypeId>`
/// mappings. Note the escaping there differs from folder names: only `:` -> `$`
/// (slashes are kept as-is).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MinimapConfig {
    pub config: KeyValueConfig,
    /// (level id, dimension type id), both with `$` restored to `:`.
    pub dimension_types: Vec<(String, String)>,
}

impl MinimapConfig {
    /// The vanilla dimension type the game reported for a level id. The only
    /// source of that mapping when a world-map `dimension_config.txt` is
    /// missing or has no `dimensionTypeId` line.
    pub fn dimension_type_of(&self, level_id: &str) -> Option<&str> {
        self.dimension_types
            .iter()
            .find(|(level, _)| level == level_id)
            .map(|(_, ty)| ty.as_str())
    }
}

pub fn parse_minimap_config(text: &str) -> MinimapConfig {
    let mut out = MinimapConfig::default();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("dimensionType:")
            && let Some((level, ty)) = rest.split_once(':')
        {
            out.dimension_types
                .push((level.replace('$', ":"), ty.replace('$', ":")));
            continue;
        }
        match line.split_once(':') {
            Some((k, v)) => out.config.entries.push((k.to_string(), v.to_string())),
            None => out.config.entries.push((line.to_string(), String::new())),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_config_real() {
        let text =
            "MWName:mw$default:Default\ncaveModeType:1\ndimensionTypeId:minecraft:the_nether\n";
        let c = parse_dimension_config(text);
        assert_eq!(
            c.multiworld_names,
            vec![("mw$default".into(), "Default".into())]
        );
        assert_eq!(c.cave_mode_type, Some(1));
        assert_eq!(c.dimension_type_id.as_deref(), Some("minecraft:the_nether"));
        assert_eq!(c.multiworld_display_name("mw$default"), "Default");
        assert_eq!(c.multiworld_display_name("mw$123"), "mw$123");
        assert_eq!(c.cave_mode_name(), Some("Layered"));
    }

    #[test]
    fn dimension_config_legacy_multiworld_and_escaped_name() {
        let text =
            "confirmedMultiworld:mw$default\nMWName:mw0,1,0:Map^col^2\nMWName:mw$default:Map 1\n";
        let c = parse_dimension_config(text);
        assert_eq!(c.multiworld_display_name("mw0,1,0"), "Map:2");
        assert_eq!(c.multiworld_display_name("mw$default"), "Map 1");
        assert_eq!(c.other_lines, vec!["confirmedMultiworld:mw$default"]);
    }

    #[test]
    fn minimap_config_real() {
        let text = "usingMultiworldDetection:false\n//dimension types (DO NOT EDIT)\ndimensionType:minecraft$worlds/2b2t/2b2t_1_nether:minecraft$the_nether\ndimensionType:minecraft$overworld:minecraft$overworld\n//server waypoints\n";
        let c = parse_minimap_config(text);
        assert_eq!(c.config.get("usingMultiworldDetection"), Some("false"));
        assert_eq!(
            c.dimension_types[0],
            (
                "minecraft:worlds/2b2t/2b2t_1_nether".to_string(),
                "minecraft:the_nether".to_string()
            )
        );
        assert_eq!(
            c.dimension_type_of("minecraft:worlds/2b2t/2b2t_1_nether"),
            Some("minecraft:the_nether")
        );
        assert_eq!(c.dimension_type_of("minecraft:nope"), None);
    }
}
