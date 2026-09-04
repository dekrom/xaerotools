//! Persistent server config: runtime-added map roots and ingest tokens.
//!
//! Lives in the app data dir (next to the vault by default), never inside a
//! scanned root. JSON so no extra deps; saved atomically (tmp + rename).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct FileConfig {
    /// Map roots added at runtime (CLI --root flags are not persisted here).
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    /// Ingest bearer tokens, one per player account.
    #[serde(default)]
    pub tokens: Vec<TokenEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TokenEntry {
    pub player: String,
    /// 64 hex chars (32 random bytes). Never logged, never in URLs.
    pub token: String,
    #[serde(rename = "createdMs", default)]
    pub created_ms: u64,
}

/// `~/.local/share/xaerotools/config.json` (platform equivalent) — the same
/// directory the default vault lives in.
pub fn default_config_path() -> PathBuf {
    xaero_db::vault::default_vault_path()
        .parent()
        .map(|d| d.join("config.json"))
        .unwrap_or_else(|| PathBuf::from("config.json"))
}

/// Missing file = default config; a corrupt file is an error (refuse to
/// silently clobber tokens on the next save).
pub fn load(path: &Path) -> Result<FileConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            restrict_permissions(path); // holds tokens: no wider than 0600
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn save(path: &Path, config: &FileConfig) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, &text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Saves arbitrary JSON text next to the config, atomically and 0600.
///
/// Used for sidecars that are not the config itself (the stored Atlas POI
/// list), so they inherit the same "never world-readable, never half-written"
/// handling without being crammed into `FileConfig`.
pub fn save_sidecar(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Creates the file 0600 from the first byte — it holds bearer tokens, and a
/// write-then-chmod would expose them for a window on multi-user machines.
#[cfg(unix)]
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path); // stale tmp from a crashed save
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)
}

/// Serializes cross-process load-modify-save cycles on the config (the
/// server's roots API vs the `tokens` CLI) via an advisory lock on a sibling
/// file. Best effort: if locking fails the closure still runs.
pub fn with_file_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    let lock_path = path.with_extension("json.lock");
    let guard = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .and_then(|file| {
            file.lock()?;
            Ok(file)
        })
        .ok();
    let out = f();
    drop(guard);
    out
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

/// File mtime in unix ms (0 when missing) — used to hot-reload tokens.
/// Change stamp for the config file: mtime plus length, so an edit that lands
/// within one mtime tick (coarse filesystems round to 1-2 s) is still seen.
pub fn file_stamp(path: &Path) -> (u64, u64) {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    (mtime_ms(path), len)
}

pub fn mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn generate_token() -> String {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).expect("os rng");
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

impl FileConfig {
    /// Adds (or replaces — one token per player) and returns the new token.
    pub fn set_token(&mut self, player: &str, now_ms: u64) -> String {
        let token = generate_token();
        self.tokens.retain(|t| t.player != player);
        self.tokens.push(TokenEntry {
            player: player.to_string(),
            token: token.clone(),
            created_ms: now_ms,
        });
        token
    }

    /// Removes the player's token; false if there was none.
    pub fn revoke_token(&mut self, player: &str) -> bool {
        let before = self.tokens.len();
        self.tokens.retain(|t| t.player != player);
        self.tokens.len() != before
    }

    /// Constant-time-ish bearer lookup: compares against every entry so the
    /// timing doesn't reveal which prefix matched. Returns the player.
    pub fn player_for_token(&self, presented: &str) -> Option<&str> {
        let mut found: Option<&str> = None;
        for entry in &self.tokens {
            if ct_eq(entry.token.as_bytes(), presented.as_bytes()) {
                found = Some(&entry.player);
            }
        }
        found
    }
}

pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_token_lifecycle() {
        let dir = std::env::temp_dir().join(format!("xt-config-test-{}", std::process::id()));
        let path = dir.join("config.json");
        let mut cfg = FileConfig::default();
        cfg.roots.push(PathBuf::from("/some/root"));
        let tok1 = cfg.set_token("Alice", 1);
        assert_eq!(tok1.len(), 64);
        assert_eq!(cfg.player_for_token(&tok1), Some("Alice"));
        assert_eq!(cfg.player_for_token("nope"), None);

        // Regenerating replaces, never accumulates.
        let tok2 = cfg.set_token("Alice", 2);
        assert_ne!(tok1, tok2);
        assert_eq!(cfg.player_for_token(&tok1), None);
        assert_eq!(cfg.tokens.len(), 1);

        save(&path, &cfg).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.roots, cfg.roots);
        assert_eq!(loaded.player_for_token(&tok2), Some("Alice"));

        let mut loaded = loaded;
        assert!(loaded.revoke_token("Alice"));
        assert!(!loaded.revoke_token("Alice"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_default_corrupt_file_is_error() {
        assert!(load(Path::new("/nonexistent/xt/config.json"))
            .unwrap()
            .tokens
            .is_empty());
        let dir = std::env::temp_dir().join(format!("xt-config-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("config.json");
        std::fs::write(&bad, "{not json").unwrap();
        assert!(load(&bad).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
