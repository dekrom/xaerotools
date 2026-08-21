//! Parser/formatter for Xaero's Minimap waypoint files
//! (`minimap/<worldId>/dim%<d>/mw$<id>_1.txt`).
//!
//! Line format (self-describing header written by the mod):
//! `waypoint:name:initials:x:y:z:color:disabled:type:set:rotate_on_tp:tp_yaw:visibility_type:destination`
//!
//! Verified against the mod's `WaypointIO` bytecode:
//! - literal `:` inside name/initials is stored as `§§` (restored on read)
//! - an absent Y is stored as `~`
//! - `sets:` lines declare waypoint set names
//! - color is an index into the 20-entry `WaypointColor` enum

#[derive(Debug, Clone, PartialEq)]
pub struct Waypoint {
    pub name: String,
    pub initials: String,
    pub x: i32,
    pub y: Option<i32>,
    pub z: i32,
    pub color: u8,
    pub disabled: bool,
    /// "type" column: 0 = normal, 1 = death, 2 = old death.
    pub purpose: i32,
    pub set: String,
    pub rotate_on_tp: bool,
    pub tp_yaw: i32,
    pub visibility_type: i32,
    pub destination: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WaypointFile {
    /// Declared waypoint set names from `sets:` lines (excluding the default).
    pub sets: Vec<String>,
    pub waypoints: Vec<Waypoint>,
    /// Non-comment lines we don't understand, preserved verbatim so a merge
    /// never drops data written by a newer mod version.
    pub other_lines: Vec<String>,
}

pub const DEFAULT_SET: &str = "gui.xaero_default";

/// Waypoint color palette (WaypointColor enum order): the 16 Minecraft text
/// colors followed by magenta, light blue, lime, pink.
pub const WAYPOINT_COLORS: [[u8; 3]; 20] = [
    [0x00, 0x00, 0x00], // black
    [0x00, 0x00, 0xAA], // dark_blue
    [0x00, 0xAA, 0x00], // dark_green
    [0x00, 0xAA, 0xAA], // dark_aqua
    [0xAA, 0x00, 0x00], // dark_red
    [0xAA, 0x00, 0xAA], // dark_purple
    [0xFF, 0xAA, 0x00], // gold
    [0xAA, 0xAA, 0xAA], // gray
    [0x55, 0x55, 0x55], // dark_gray
    [0x55, 0x55, 0xFF], // blue
    [0x55, 0xFF, 0x55], // green
    [0x55, 0xFF, 0xFF], // aqua
    [0xFF, 0x55, 0x55], // red
    [0xFF, 0x55, 0xFF], // purple
    [0xFF, 0xFF, 0x55], // yellow
    [0xFF, 0xFF, 0xFF], // white
    [0xC7, 0x4E, 0xBD], // magenta
    [0x3A, 0xB3, 0xDA], // light_blue
    [0x80, 0xC7, 0x1F], // lime
    [0xF3, 0x8B, 0xAA], // pink
];

pub fn waypoint_color_rgb(index: u8) -> [u8; 3] {
    WAYPOINT_COLORS[(index as usize) % WAYPOINT_COLORS.len()]
}

const COLON_ESCAPE: &str = "§§";

fn unescape(field: &str) -> String {
    field.replace(COLON_ESCAPE, ":")
}

fn escape(field: &str) -> String {
    field.replace(':', COLON_ESCAPE)
}

pub fn parse_waypoints_file(text: &str) -> WaypointFile {
    let mut out = WaypointFile::default();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("sets:") {
            out.sets
                .extend(rest.split(':').filter(|s| !s.is_empty()).map(String::from));
        } else if line.starts_with("waypoint:") {
            match parse_waypoint_line(line) {
                Some(w) => out.waypoints.push(w),
                None => out.other_lines.push(line.to_string()),
            }
        } else {
            out.other_lines.push(line.to_string());
        }
    }
    out
}

pub fn parse_waypoint_line(line: &str) -> Option<Waypoint> {
    let t: Vec<&str> = line.split(':').collect();
    if t.len() < 10 || t[0] != "waypoint" {
        return None;
    }
    let get = |i: usize| t.get(i).copied().unwrap_or("");
    Some(Waypoint {
        name: unescape(t[1]),
        initials: unescape(t[2]),
        x: t[3].parse().ok()?,
        y: if t[4] == "~" { None } else { t[4].parse().ok() },
        z: t[5].parse().ok()?,
        color: t[6].parse().unwrap_or(0),
        disabled: t[7] == "true",
        purpose: t[8].parse().unwrap_or(0),
        set: t[9].to_string(),
        rotate_on_tp: get(10) == "true",
        tp_yaw: get(11).parse().unwrap_or(0),
        visibility_type: get(12).parse().unwrap_or(0),
        destination: get(13) == "true",
    })
}

/// Serializes a waypoint exactly the way the mod's writer does, so merged
/// files remain loadable in-game.
pub fn format_waypoint_line(w: &Waypoint) -> String {
    format!(
        "waypoint:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
        escape(&w.name),
        escape(&w.initials),
        w.x,
        w.y.map(|v| v.to_string()).unwrap_or_else(|| "~".into()),
        w.z,
        w.color,
        w.disabled,
        w.purpose,
        w.set,
        w.rotate_on_tp,
        w.tp_yaw,
        w.visibility_type,
        w.destination
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_line() {
        let line = "waypoint:StashFinder - 🪧 8:🪧:-24:64:20999928:6:false:0:gui.xaero_default:false:0:0:false";
        let w = parse_waypoint_line(line).unwrap();
        assert_eq!(w.name, "StashFinder - 🪧 8");
        assert_eq!(w.initials, "🪧");
        assert_eq!((w.x, w.y, w.z), (-24, Some(64), 20999928));
        assert_eq!(w.color, 6);
        assert!(!w.disabled && !w.destination);
        assert_eq!(w.set, DEFAULT_SET);
        assert_eq!(format_waypoint_line(&w), line);
    }

    #[test]
    fn colon_escape_round_trip() {
        let w = Waypoint {
            name: "base: main".into(),
            initials: "b:m".into(),
            x: 1,
            y: None,
            z: 2,
            color: 19,
            disabled: true,
            purpose: 0,
            set: DEFAULT_SET.into(),
            rotate_on_tp: false,
            tp_yaw: 0,
            visibility_type: 1,
            destination: false,
        };
        let line = format_waypoint_line(&w);
        assert!(line.contains("base§§ main"));
        assert!(line.contains(":~:"));
        assert_eq!(parse_waypoint_line(&line).unwrap(), w);
    }

    #[test]
    fn file_with_sets_and_unknown_lines() {
        let text = "#\n#waypoint:name:initials:...\n#\nsets:alpha:beta\nwaypoint:A:a:0:64:0:0:false:0:gui.xaero_default:false:0:0:false\nfuturestuff:xyz\n";
        let f = parse_waypoints_file(text);
        assert_eq!(f.sets, vec!["alpha", "beta"]);
        assert_eq!(f.waypoints.len(), 1);
        assert_eq!(f.other_lines, vec!["futurestuff:xyz"]);
    }
}
