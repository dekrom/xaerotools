use std::fs;
use tempfile::TempDir;

/// Build a minimal world-map tree. Each region = (world_id, rx, rz, bytes),
/// placed at world-map/<world>/null/0/<rx>_<rz>.zip (dim "null", mw "0").
pub fn world_map_tree(regions: &[(&str, i32, i32, &[u8])]) -> TempDir {
    let directory = TempDir::new().expect("tempdir");
    for (world, rx, rz, bytes) in regions {
        let region_directory = directory.path().join(format!("world-map/{world}/null/0"));
        fs::create_dir_all(&region_directory).unwrap();
        fs::write(region_directory.join(format!("{rx}_{rz}.zip")), bytes).unwrap();
    }
    directory
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builds_the_expected_layout() {
        let directory = world_map_tree(&[("srv", -10, -1, b"zzz")]);
        let p = directory.path().join("world-map/srv/null/0/-10_-1.zip");
        assert!(p.is_file(), "expected region file at {}", p.display());
        assert_eq!(std::fs::read(p).unwrap(), b"zzz");
    }
}
