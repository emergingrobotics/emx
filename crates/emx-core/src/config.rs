use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use emx_text::BLOCK_SIZE;
use emx_types::KbdError;
use serde::Deserialize;

use crate::keymap::Keymap;

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
/// Emacs `mark-ring-max`.
pub const MARK_RING_MAX: u64 = 16;
/// Emacs `indent-tabs-mode`: indentation commands may use tabs.
pub const INDENT_TABS_MODE: bool = true;
/// Emacs `auto-save-interval` and `auto-save-timeout`.
pub const AUTO_SAVE_INTERVAL_KEYS: u64 = 300;
pub const AUTO_SAVE_TIMEOUT_S: u64 = 30;
pub const TAB_WIDTH: u64 = 8;
/// Widest `tab_width` accepted from config.
pub const TAB_WIDTH_MAX: u64 = 16;
pub const NEXT_SCREEN_CONTEXT_LINES: u64 = 2;
/// Emacs `amalgamating-undo-limit`.
pub const AMALGAMATION_LIMIT: u64 = 20;
pub const COLUMN_CHECKPOINT_INTERVAL: u64 = 4096;
pub const LONG_LINE_THRESHOLD: u64 = 4096;
/// Emacs `echo-keystrokes` (1 second): how long an unfinished key sequence
/// waits before it is echoed. Zero disables echoing.
pub const ECHO_KEYSTROKES_MS: u64 = 1000;
/// `emx events` reports a client's cursor at most once per this interval.
pub const CURSOR_EVENT_INTERVAL_MS: u64 = 50;
/// At most this many completion candidates are listed above a prompt
/// (vertico's `vertico-count`).
pub const COMPLETION_ROWS: u64 = 10;
/// Widest `completion_rows` accepted from config; a terminal is never this
/// tall.
pub const COMPLETION_ROWS_MAX: u64 = 1000;
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
    pub append_tail_hash_bytes: u64,
    pub kill_ring_max: u64,
    pub mark_ring_max: u64,
    pub indent_tabs_mode: bool,
    pub auto_save_interval_keys: u64,
    pub auto_save_timeout_s: u64,
    pub tab_width: u64,
    /// Zero is valid: Emacs allows paging with no overlap.
    pub next_screen_context_lines: u64,
    pub amalgamation_limit: u64,
    pub column_checkpoint_interval: u64,
    pub long_line_threshold: u64,
    /// Zero is valid: Emacs `echo-keystrokes` 0 turns echoing off.
    pub echo_keystrokes_ms: u64,
    pub cursor_event_interval_ms: u64,
    pub completion_rows: u64,
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
            append_tail_hash_bytes: APPEND_TAIL_HASH_BYTES,
            kill_ring_max: KILL_RING_MAX,
            mark_ring_max: MARK_RING_MAX,
            indent_tabs_mode: INDENT_TABS_MODE,
            auto_save_interval_keys: AUTO_SAVE_INTERVAL_KEYS,
            auto_save_timeout_s: AUTO_SAVE_TIMEOUT_S,
            tab_width: TAB_WIDTH,
            next_screen_context_lines: NEXT_SCREEN_CONTEXT_LINES,
            amalgamation_limit: AMALGAMATION_LIMIT,
            column_checkpoint_interval: COLUMN_CHECKPOINT_INTERVAL,
            long_line_threshold: LONG_LINE_THRESHOLD,
            echo_keystrokes_ms: ECHO_KEYSTROKES_MS,
            cursor_event_interval_ms: CURSOR_EVENT_INTERVAL_MS,
            completion_rows: COMPLETION_ROWS,
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
    #[error("invalid config {path}: {key} {reason}")]
    Invalid {
        path: PathBuf,
        key: &'static str,
        reason: String,
    },
    #[error("cannot determine the working directory: {source}")]
    WorkingDirectory { source: io::Error },
    #[error("invalid key sequence {keys:?} in [keys]: {source}")]
    InvalidKey { keys: String, source: KbdError },
    #[error("unknown command {command:?} bound to {keys:?} in [keys]")]
    UnknownCommand { keys: String, command: String },
    /// Emacs `define-key` wording.
    #[error("Key sequence {keys} starts with non-prefix key {prefix}")]
    NonPrefixKey { keys: String, prefix: String },
}

/// A rejected setting: the key and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSetting {
    pub key: &'static str,
    pub reason: String,
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
        let config: Config = toml::from_str(&content).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate().map_err(|invalid| ConfigError::Invalid {
            path: path.to_path_buf(),
            key: invalid.key,
            reason: invalid.reason,
        })?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), InvalidSetting> {
        let nonzero = [
            ("in_memory_max", self.in_memory_max),
            ("block_cache_bytes", self.block_cache_bytes),
            ("search_match_cap", self.search_match_cap),
            ("append_tail_hash_bytes", self.append_tail_hash_bytes),
            ("settle_window_ms", self.settle_window_ms),
            ("poll_interval_ms", self.poll_interval_ms),
            ("journal_fsync_ms", self.journal_fsync_ms),
            ("journal_snapshot_max", self.journal_snapshot_max),
            ("large_save_warn", self.large_save_warn),
            ("daemon_grace_ms", self.daemon_grace_ms),
            ("undo_limit", self.undo_limit),
            ("undo_strong_limit", self.undo_strong_limit),
            ("undo_outer_limit", self.undo_outer_limit),
            ("kill_ring_max", self.kill_ring_max),
            ("mark_ring_max", self.mark_ring_max),
            ("auto_save_interval_keys", self.auto_save_interval_keys),
            ("auto_save_timeout_s", self.auto_save_timeout_s),
            ("amalgamation_limit", self.amalgamation_limit),
            (
                "column_checkpoint_interval",
                self.column_checkpoint_interval,
            ),
            ("long_line_threshold", self.long_line_threshold),
            ("cursor_event_interval_ms", self.cursor_event_interval_ms),
        ];
        for (key, value) in nonzero {
            if value == 0 {
                return Err(InvalidSetting {
                    key,
                    reason: "must be greater than 0".to_string(),
                });
            }
        }
        // A cache smaller than one block cannot hold even the block it just
        // read, so every large-file open would fail. Reject it at startup
        // rather than let it pass here and break the first large open.
        if self.block_cache_bytes < BLOCK_SIZE as u64 {
            return Err(InvalidSetting {
                key: "block_cache_bytes",
                reason: format!("must be at least one block ({BLOCK_SIZE})"),
            });
        }
        if !(1..=TAB_WIDTH_MAX).contains(&self.tab_width) {
            return Err(InvalidSetting {
                key: "tab_width",
                reason: format!("must be between 1 and {TAB_WIDTH_MAX}"),
            });
        }
        if !(1..=COMPLETION_ROWS_MAX).contains(&self.completion_rows) {
            return Err(InvalidSetting {
                key: "completion_rows",
                reason: format!("must be between 1 and {COMPLETION_ROWS_MAX}"),
            });
        }
        if self.undo_strong_limit < self.undo_limit {
            return Err(InvalidSetting {
                key: "undo_strong_limit",
                reason: format!("must be at least undo_limit ({})", self.undo_limit),
            });
        }
        if self.undo_outer_limit < self.undo_strong_limit {
            return Err(InvalidSetting {
                key: "undo_outer_limit",
                reason: format!(
                    "must be at least undo_strong_limit ({})",
                    self.undo_strong_limit
                ),
            });
        }
        Keymap::emacs_default()
            .apply_overrides(&self.keys)
            .map_err(|error| InvalidSetting {
                key: "keys",
                reason: error.to_string(),
            })?;
        Ok(())
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
            "undo_limit = 10\nkeyboard_encoding = \"kitty\"\n[keys]\n\"C-c f\" = \"forward-char\"\n",
        )
        .expect("valid config");
        assert_eq!(config.undo_limit, 10);
        assert_eq!(config.undo_strong_limit, UNDO_STRONG_LIMIT);
        assert_eq!(
            config.keys.get("C-c f").map(String::as_str),
            Some("forward-char")
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

    #[test]
    fn every_group_is_overridable() {
        let config = load_str(
            "in_memory_max = 1\nblock_cache_bytes = 65536\nsearch_match_cap = 3\n\
             append_tail_hash_bytes = 4\nsettle_window_ms = 5\npoll_interval_ms = 6\n\
             journal_fsync_ms = 7\njournal_snapshot_max = 8\nlarge_save_warn = 9\n\
             daemon_grace_ms = 10\nundo_limit = 11\nundo_strong_limit = 12\n\
             undo_outer_limit = 13\nkill_ring_max = 14\nauto_save_interval_keys = 15\n\
             auto_save_timeout_s = 16\ntab_width = 4\nnext_screen_context_lines = 0\n\
             amalgamation_limit = 17\ncolumn_checkpoint_interval = 18\n\
             long_line_threshold = 19\necho_keystrokes_ms = 0\nmark_ring_max = 20\n\
             indent_tabs_mode = false\ncursor_event_interval_ms = 21\ncompletion_rows = 22\n",
        )
        .expect("valid config");
        let expected = Config {
            keys: BTreeMap::new(),
            in_memory_max: 1,
            block_cache_bytes: 65536,
            search_match_cap: 3,
            append_tail_hash_bytes: 4,
            settle_window_ms: 5,
            poll_interval_ms: 6,
            journal_fsync_ms: 7,
            journal_snapshot_max: 8,
            large_save_warn: 9,
            keyboard_encoding: KeyboardEncoding::Kitty,
            daemon_grace_ms: 10,
            undo_limit: 11,
            undo_strong_limit: 12,
            undo_outer_limit: 13,
            kill_ring_max: 14,
            mark_ring_max: 20,
            indent_tabs_mode: false,
            auto_save_interval_keys: 15,
            auto_save_timeout_s: 16,
            tab_width: 4,
            next_screen_context_lines: 0,
            amalgamation_limit: 17,
            column_checkpoint_interval: 18,
            long_line_threshold: 19,
            echo_keystrokes_ms: 0,
            cursor_event_interval_ms: 21,
            completion_rows: 22,
        };
        assert_eq!(config, expected);
    }

    #[test]
    fn defaults_cover_the_new_fields() {
        let config = Config::default();
        assert_eq!(config.kill_ring_max, 120);
        assert_eq!(config.completion_rows, 10);
        assert_eq!(config.tab_width, 8);
        assert_eq!(config.next_screen_context_lines, 2);
        assert_eq!(config.amalgamation_limit, 20);
        assert_eq!(config.auto_save_interval_keys, 300);
        assert_eq!(config.auto_save_timeout_s, 30);
        assert_eq!(config.column_checkpoint_interval, 4096);
        assert_eq!(config.long_line_threshold, 4096);
        assert_eq!(config.append_tail_hash_bytes, 64 * 1024);
        assert_eq!(config.echo_keystrokes_ms, 1000);
        assert_eq!(config.cursor_event_interval_ms, 50);
        assert!(config.validate().is_ok());
    }

    fn invalid_key(content: &str) -> &'static str {
        match load_str(content).expect_err("config rejected") {
            ConfigError::Invalid { key, .. } => key,
            other => panic!("expected Invalid, got {other}"),
        }
    }

    #[test]
    fn zero_is_rejected_for_every_interval_size_and_limit() {
        for key in [
            "in_memory_max",
            "block_cache_bytes",
            "search_match_cap",
            "append_tail_hash_bytes",
            "settle_window_ms",
            "poll_interval_ms",
            "journal_fsync_ms",
            "journal_snapshot_max",
            "large_save_warn",
            "daemon_grace_ms",
            "undo_limit",
            "undo_strong_limit",
            "undo_outer_limit",
            "kill_ring_max",
            "auto_save_interval_keys",
            "auto_save_timeout_s",
            "amalgamation_limit",
            "column_checkpoint_interval",
            "long_line_threshold",
        ] {
            assert_eq!(invalid_key(&format!("{key} = 0\n")), key);
        }
        let error = load_str("poll_interval_ms = 0\n").expect_err("rejected");
        assert!(error.to_string().contains("poll_interval_ms"), "{error}");
    }

    #[test]
    fn block_cache_bytes_below_one_block_is_rejected() {
        // A positive value under one block passes the nonzero check but would
        // break every large-file open, so validate() must reject it by name.
        let key = invalid_key(&format!("block_cache_bytes = {}\n", BLOCK_SIZE - 1));
        assert_eq!(key, "block_cache_bytes");
        let error = load_str(&format!("block_cache_bytes = {}\n", BLOCK_SIZE - 1))
            .expect_err("sub-block cache rejected");
        assert!(error.to_string().contains("block_cache_bytes"), "{error}");
        assert!(
            load_str(&format!("block_cache_bytes = {BLOCK_SIZE}\n")).is_ok(),
            "exactly one block is valid"
        );
    }

    #[test]
    fn undo_limits_must_be_ordered() {
        assert_eq!(invalid_key("undo_limit = 300000\n"), "undo_strong_limit");
        assert_eq!(
            invalid_key("undo_strong_limit = 30000000\n"),
            "undo_outer_limit"
        );
        assert!(load_str("undo_limit = 5\nundo_strong_limit = 5\nundo_outer_limit = 5\n").is_ok());
    }

    #[test]
    fn key_overrides_are_validated_at_load() {
        let error = load_str("[keys]\n\"C-c t\" = \"no-such-command\"\n")
            .expect_err("unknown command rejected");
        let message = error.to_string();
        assert!(
            message.contains("keys") && message.contains("no-such-command"),
            "{message}"
        );
        assert!(load_str("[keys]\n\"C-c t\" = \"forward-char\"\n").is_ok());
    }

    #[test]
    fn tab_width_must_be_1_to_16() {
        assert_eq!(invalid_key("tab_width = 0\n"), "tab_width");
        assert_eq!(invalid_key("tab_width = 17\n"), "tab_width");
        assert_eq!(invalid_key("completion_rows = 0\n"), "completion_rows");
        assert_eq!(invalid_key("completion_rows = 1001\n"), "completion_rows");
        assert_eq!(load_str("tab_width = 16\n").expect("valid").tab_width, 16);
        assert_eq!(load_str("tab_width = 1\n").expect("valid").tab_width, 1);
    }
}
