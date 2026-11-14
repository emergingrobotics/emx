use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

pub const IN_MEMORY_MAX: u64 = 16 * MIB;
pub const BLOCK_CACHE_BYTES: u64 = 64 * MIB;
pub const SEARCH_MATCH_CAP: u64 = 64 * KIB;
pub const APPEND_TAIL_HASH_BYTES: u64 = 64 * KIB;
pub const SETTLE_WINDOW_MS: u64 = 200;
pub const POLL_INTERVAL_MS: u64 = 2000;
pub const JOURNAL_FSYNC_MS: u64 = 200;
pub const JOURNAL_SNAPSHOT_MAX: u64 = 64 * MIB;
pub const LARGE_SAVE_WARN: u64 = 512 * MIB;
pub const DAEMON_GRACE_MS: u64 = 3000;
/// Emacs 29 `undo-limit`, `undo-strong-limit` and `undo-outer-limit`.
pub const UNDO_LIMIT: u64 = 160_000;
pub const UNDO_STRONG_LIMIT: u64 = 240_000;
pub const UNDO_OUTER_LIMIT: u64 = 24_000_000;
/// Emacs `kill-ring-max`.
pub const KILL_RING_MAX: u64 = 120;
/// Emacs `auto-save-interval` and `auto-save-timeout`.
pub const AUTO_SAVE_INTERVAL_KEYS: u64 = 300;
pub const AUTO_SAVE_TIMEOUT_S: u64 = 30;
pub const TAB_WIDTH: u64 = 8;
pub const NEXT_SCREEN_CONTEXT_LINES: u64 = 2;
/// Emacs `amalgamating-undo-limit`.
pub const AMALGAMATION_LIMIT: u64 = 20;
pub const COLUMN_CHECKPOINT_INTERVAL: u64 = 4096;
pub const LONG_LINE_THRESHOLD: u64 = 4096;
pub const MAX_WIRE_MESSAGE: u64 = 16 * MIB;
pub const FRAME_QUEUE_DEPTH: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyboardEncoding {
    Kitty,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Key sequence (Emacs `kbd` syntax) to command name overrides.
    pub keys: BTreeMap<String, String>,
    pub in_memory_max: u64,
    pub block_cache_bytes: u64,
    pub settle_window_ms: u64,
    pub poll_interval_ms: u64,
    pub journal_fsync_ms: u64,
    pub journal_snapshot_max: u64,
    pub large_save_warn: u64,
    pub search_match_cap: u64,
    pub keyboard_encoding: KeyboardEncoding,
    pub daemon_grace_ms: u64,
    pub undo_limit: u64,
    pub undo_strong_limit: u64,
    pub undo_outer_limit: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keys: BTreeMap::new(),
            in_memory_max: IN_MEMORY_MAX,
            block_cache_bytes: BLOCK_CACHE_BYTES,
            settle_window_ms: SETTLE_WINDOW_MS,
            poll_interval_ms: POLL_INTERVAL_MS,
            journal_fsync_ms: JOURNAL_FSYNC_MS,
            journal_snapshot_max: JOURNAL_SNAPSHOT_MAX,
            large_save_warn: LARGE_SAVE_WARN,
            search_match_cap: SEARCH_MATCH_CAP,
            keyboard_encoding: KeyboardEncoding::Kitty,
            daemon_grace_ms: DAEMON_GRACE_MS,
            undo_limit: UNDO_LIMIT,
            undo_strong_limit: UNDO_STRONG_LIMIT,
            undo_outer_limit: UNDO_OUTER_LIMIT,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("invalid config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

impl Config {
    /// `None` means no config file: all defaults. An explicit path that does
    /// not exist is an error, never a silent fallback to defaults.
    pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
        let Some(path) = path else {
            return Ok(Config::default());
        };
        let content = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&content).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_str(content: &str) -> Result<Config, ConfigError> {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        fs::write(&path, content).expect("write config");
        Config::load(Some(&path))
    }

    #[test]
    fn no_path_gives_defaults() {
        let config = Config::load(None).expect("defaults");
        assert_eq!(config, Config::default());
        assert_eq!(config.undo_limit, 160_000);
        assert_eq!(config.undo_strong_limit, 240_000);
        assert_eq!(config.undo_outer_limit, 24_000_000);
        assert_eq!(config.in_memory_max, 16 * 1024 * 1024);
        assert_eq!(config.keyboard_encoding, KeyboardEncoding::Kitty);
    }

    #[test]
    fn partial_file_overrides_only_named_keys() {
        let config = load_str(
            "undo_limit = 10\nkeyboard_encoding = \"kitty\"\n[keys]\n\"C-x C-s\" = \"save-buffer\"\n",
        )
        .expect("valid config");
        assert_eq!(config.undo_limit, 10);
        assert_eq!(config.undo_strong_limit, UNDO_STRONG_LIMIT);
        assert_eq!(
            config.keys.get("C-x C-s").map(String::as_str),
            Some("save-buffer")
        );
    }

    #[test]
    fn unknown_key_is_an_error_naming_it() {
        let error = load_str("undo_limt = 10\n").expect_err("unknown key rejected");
        assert!(error.to_string().contains("undo_limt"), "{error}");
    }

    #[test]
    fn invalid_value_is_an_error_naming_the_key() {
        let error = load_str("undo_limit = \"big\"\n").expect_err("bad value rejected");
        assert!(error.to_string().contains("undo_limit"), "{error}");
        let error = load_str("keyboard_encoding = \"legacy\"\n").expect_err("only kitty");
        assert!(error.to_string().contains("keyboard_encoding"), "{error}");
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let error = Config::load(Some(&directory.path().join("absent.toml")))
            .expect_err("missing file rejected");
        assert!(matches!(error, ConfigError::Read { .. }));
    }
}
