use std::collections::{BTreeMap, HashMap};

use emx_types::{Key, KeyCode, Mods, format_key, parse_kbd};

use crate::command::{self, PrefixArg};
use crate::config::ConfigError;

const NO_MODS: Mods = Mods {
    ctrl: false,
    meta: false,
    shift: false,
    super_: false,
};
const ESCAPE: Key = Key {
    code: KeyCode::Escape,
    mods: NO_MODS,
};
const CONTROL_U: Key = Key {
    code: KeyCode::Char('u'),
    mods: Mods {
        ctrl: true,
        ..NO_MODS
    },
};

const SELF_INSERT: &str = "self-insert-command";
const DIGIT_ARGUMENT: &str = "digit-argument";
const NEGATIVE_ARGUMENT: &str = "negative-argument";

const DEFAULT_BINDINGS: [(&str, &str); 61] = [
    ("C-f", "forward-char"),
    ("<right>", "forward-char"),
    ("C-b", "backward-char"),
    ("<left>", "backward-char"),
    ("C-n", "next-line"),
    ("<down>", "next-line"),
    ("C-p", "previous-line"),
    ("<up>", "previous-line"),
    ("C-a", "move-beginning-of-line"),
    ("<home>", "move-beginning-of-line"),
    ("C-e", "move-end-of-line"),
    ("<end>", "move-end-of-line"),
    ("M-f", "forward-word"),
    ("M-b", "backward-word"),
    ("M-<", "beginning-of-buffer"),
    ("C-<home>", "beginning-of-buffer"),
    ("M->", "end-of-buffer"),
    ("C-<end>", "end-of-buffer"),
    ("RET", "newline"),
    ("C-d", "delete-char"),
    ("DEL", "delete-backward-char"),
    ("C-k", "kill-line"),
    ("M-d", "kill-word"),
    ("C-w", "kill-region"),
    ("M-w", "kill-ring-save"),
    ("C-y", "yank"),
    ("M-y", "yank-pop"),
    ("C-SPC", "set-mark-command"),
    ("C-@", "set-mark-command"),
    ("C-x C-x", "exchange-point-and-mark"),
    ("C-/", "undo"),
    ("C-_", "undo"),
    ("C-x u", "undo"),
    ("C-?", "undo-redo"),
    ("C-M-_", "undo-redo"),
    ("C-u", "universal-argument"),
    ("C-g", "keyboard-quit"),
    ("C-v", "scroll-up-command"),
    ("<next>", "scroll-up-command"),
    ("M-v", "scroll-down-command"),
    ("<prior>", "scroll-down-command"),
    ("C-l", "recenter-top-bottom"),
    ("C-x C-f", "find-file"),
    ("C-x C-s", "save-buffer"),
    ("C-x C-w", "write-file"),
    ("C-x s", "save-some-buffers"),
    ("C-x k", "kill-buffer"),
    ("C-x C-c", "save-buffers-kill-terminal"),
    ("C-x #", "server-edit"),
    ("C-x C-q", "read-only-mode"),
    ("C-x 2", "split-window-below"),
    ("C-x 3", "split-window-right"),
    ("C-x 0", "delete-window"),
    ("C-x 1", "delete-other-windows"),
    ("C-x o", "other-window"),
    ("C-x b", "switch-to-buffer"),
    ("C-x C-b", "list-buffers"),
    ("M-x", "execute-extended-command"),
    ("M--", NEGATIVE_ARGUMENT),
    ("C--", NEGATIVE_ARGUMENT),
    ("C-M--", NEGATIVE_ARGUMENT),
];
const BUFFER_MENU_BINDINGS: [(&str, &str); 1] = [("RET", "Buffer-menu-this-window")];
/// `ctl-x-map` and `mode-specific-map`; their bindings arrive with later
/// commands, but the prefixes exist so `C-c z` reads as one sequence.
const DEFAULT_PREFIXES: [&str; 2] = ["C-x", "C-c"];
const DIGIT_ARGUMENT_MODIFIERS: [&str; 3] = ["M-", "C-", "C-M-"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    Command(&'static str),
    Prefix(Keymap),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keymap {
    bindings: HashMap<Key, Binding>,
    /// The command for every unmodified printable character without an
    /// explicit binding. Emacs covers them with a char-table range in the
    /// global map, which a trie cannot enumerate.
    printable: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    Command(&'static str),
    Prefix,
    Undefined,
}

impl Keymap {
    pub fn new() -> Keymap {
        Keymap::default()
    }

    pub fn emacs_default() -> Keymap {
        let mut keymap = Keymap {
            bindings: HashMap::new(),
            printable: Some(SELF_INSERT),
        };
        // Prefixes first: binding one later would replace its keymap.
        for sequence in DEFAULT_PREFIXES {
            keymap.bind_default(sequence, Binding::Prefix(Keymap::new()));
        }
        for (sequence, command) in DEFAULT_BINDINGS {
            keymap.bind_default(sequence, Binding::Command(command));
        }
        for modifiers in DIGIT_ARGUMENT_MODIFIERS {
            for digit in '0'..='9' {
                keymap.bind_default(
                    &format!("{modifiers}{digit}"),
                    Binding::Command(DIGIT_ARGUMENT),
                );
            }
        }
        keymap
    }

    /// Emacs `Buffer-menu-mode-map`, looked up before the global map in a
    /// Buffer Menu.
    pub fn buffer_menu() -> Keymap {
        let mut keymap = Keymap::new();
        for (sequence, command) in BUFFER_MENU_BINDINGS {
            keymap.bind_default(sequence, Binding::Command(command));
        }
        keymap
    }

    fn bind_default(&mut self, sequence: &str, binding: Binding) {
        let keys = parse_kbd(sequence).expect("default bindings are valid kbd strings");
        self.bind(&keys, binding)
            .expect("default bindings do not pass through command keys");
    }

    /// Rebinds key sequences from the `[keys]` config table. Unknown command
    /// names and unparsable sequences are errors, never skipped.
    pub fn apply_overrides(&mut self, keys: &BTreeMap<String, String>) -> Result<(), ConfigError> {
        for (sequence, command_name) in keys {
            let parsed = parse_kbd(sequence).map_err(|source| ConfigError::InvalidKey {
                keys: sequence.clone(),
                source,
            })?;
            let spec =
                command::lookup(command_name).ok_or_else(|| ConfigError::UnknownCommand {
                    keys: sequence.clone(),
                    command: command_name.clone(),
                })?;
            self.bind(&parsed, Binding::Command(spec.name))?;
        }
        Ok(())
    }

    /// Emacs `define-key`: missing prefixes are created, and binding below a
    /// key that runs a command is an error.
    pub fn bind(&mut self, keys: &[Key], binding: Binding) -> Result<(), ConfigError> {
        let folded = fold_escapes(keys);
        let Some((last, prefixes)) = folded.split_last() else {
            return Err(ConfigError::InvalidKey {
                keys: String::new(),
                source: emx_types::KbdError::Empty,
            });
        };
        let mut keymap = self;
        for (index, key) in prefixes.iter().enumerate() {
            if !matches!(keymap.bindings.get(key), Some(Binding::Prefix(_))) {
                if keymap.binds_command(key) {
                    return Err(ConfigError::NonPrefixKey {
                        keys: describe_keys(&folded),
                        prefix: describe_keys(&folded[..=index]),
                    });
                }
                keymap.bindings.insert(*key, Binding::Prefix(Keymap::new()));
            }
            let Some(Binding::Prefix(inner)) = keymap.bindings.get_mut(key) else {
                unreachable!("a prefix keymap was ensured above");
            };
            keymap = inner;
        }
        keymap.bindings.insert(*last, binding);
        Ok(())
    }

    fn binds_command(&self, key: &Key) -> bool {
        match self.bindings.get(key) {
            Some(Binding::Command(_)) => true,
            Some(Binding::Prefix(_)) => false,
            None => is_printable(key) && self.printable.is_some(),
        }
    }

    /// Looks up a canonical key sequence. ESC followed by a character key is
    /// the same as the Meta-modified key, as in Emacs's `esc-map`.
    pub fn lookup(&self, keys: &[Key]) -> Lookup {
        let folded = fold_escapes(keys);
        let mut keymap = self;
        for (index, key) in folded.iter().enumerate() {
            let last = index + 1 == folded.len();
            match keymap.bindings.get(key) {
                Some(Binding::Prefix(inner)) => {
                    if last {
                        return Lookup::Prefix;
                    }
                    keymap = inner;
                }
                Some(Binding::Command(name)) => {
                    return if last {
                        Lookup::Command(name)
                    } else {
                        Lookup::Undefined
                    };
                }
                None => {
                    if let Some(name) = keymap.printable.filter(|_| is_printable(key)) {
                        return if last {
                            Lookup::Command(name)
                        } else {
                            Lookup::Undefined
                        };
                    }
                    if last && *key == ESCAPE && keymap.has_meta_bindings() {
                        return Lookup::Prefix;
                    }
                    return Lookup::Undefined;
                }
            }
        }
        Lookup::Undefined
    }

    fn has_meta_bindings(&self) -> bool {
        self.bindings.keys().any(|key| key.mods.meta)
    }
}

fn is_printable(key: &Key) -> bool {
    key.mods == NO_MODS && matches!(key.code, KeyCode::Char(character) if !character.is_control())
}

/// Keys Emacs represents as integers (characters) rather than symbols; only
/// these combine with a preceding ESC into a Meta key.
fn is_character_event(key: &Key) -> bool {
    matches!(
        key.code,
        KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Escape
    )
}

fn with_meta(key: &Key) -> Key {
    Key {
        code: key.code,
        mods: Mods {
            meta: true,
            ..key.mods
        },
    }
}

fn fold_escapes(keys: &[Key]) -> Vec<Key> {
    let mut folded = Vec::with_capacity(keys.len());
    let mut index = 0;
    while index < keys.len() {
        let key = keys[index];
        match keys.get(index + 1) {
            Some(next) if key == ESCAPE && is_character_event(next) && !next.mods.meta => {
                folded.push(with_meta(next));
                index += 2;
            }
            _ => {
                folded.push(key);
                index += 1;
            }
        }
    }
    folded
}

/// Emacs `key-description`: ESC followed by a character key is shown as the
/// Meta key (`ESC z` is `M-z`), but ESC before ESC, a function key or an
/// already Meta key stays `ESC`.
pub fn describe_keys(keys: &[Key]) -> String {
    let mut parts = Vec::with_capacity(keys.len());
    let mut add_meta = false;
    for key in keys {
        if add_meta {
            if *key == ESCAPE || !is_character_event(key) || key.mods.meta {
                parts.push(format_key(&ESCAPE));
                if *key == ESCAPE {
                    continue;
                }
                parts.push(format_key(key));
            } else {
                parts.push(format_key(&with_meta(key)));
            }
            add_meta = false;
        } else if *key == ESCAPE {
            add_meta = true;
        } else {
            parts.push(format_key(key));
        }
    }
    if add_meta {
        parts.push(format_key(&ESCAPE));
    }
    parts.join(" ")
}

/// Emacs `universal-argument-map`, active for the key after a prefix
/// argument command. `-` after digits is not bound here, so it falls through
/// to the global map and inserts itself.
pub(crate) fn universal_argument_binding(key: Key, prefix_arg: &PrefixArg) -> Option<&'static str> {
    if key == CONTROL_U {
        return Some("universal-argument-more");
    }
    if key.mods != NO_MODS {
        return None;
    }
    match key.code {
        KeyCode::Char(character) if character.is_ascii_digit() => Some(DIGIT_ARGUMENT),
        KeyCode::Char('-') if !matches!(prefix_arg, PrefixArg::Numeric(_)) => {
            Some(NEGATIVE_ARGUMENT)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(sequence: &str) -> Vec<Key> {
        parse_kbd(sequence).expect("valid test kbd")
    }

    #[test]
    fn default_lookups() {
        let keymap = Keymap::emacs_default();
        assert_eq!(keymap.lookup(&keys("C-f")), Lookup::Command("forward-char"));
        assert_eq!(keymap.lookup(&keys("a")), Lookup::Command(SELF_INSERT));
        assert_eq!(keymap.lookup(&keys("A")), Lookup::Command(SELF_INSERT));
        assert_eq!(keymap.lookup(&keys("C-x")), Lookup::Prefix);
        assert_eq!(keymap.lookup(&keys("C-c z")), Lookup::Undefined);
        assert_eq!(keymap.lookup(&keys("M-7")), Lookup::Command(DIGIT_ARGUMENT));
        assert_eq!(
            keymap.lookup(&keys("C-M-0")),
            Lookup::Command(DIGIT_ARGUMENT)
        );
        assert_eq!(keymap.lookup(&keys("ESC")), Lookup::Prefix);
        assert_eq!(
            keymap.lookup(&keys("ESC 7")),
            Lookup::Command(DIGIT_ARGUMENT)
        );
        assert_eq!(keymap.lookup(&keys("M-a")), Lookup::Undefined);
        assert_eq!(keymap.lookup(&keys("C-x ESC")), Lookup::Undefined);
    }

    #[test]
    fn override_may_replace_a_prefix_with_a_command() {
        let mut keymap = Keymap::emacs_default();
        let overrides = BTreeMap::from([("C-x".to_string(), "forward-char".to_string())]);
        keymap.apply_overrides(&overrides).expect("allowed");
        assert_eq!(keymap.lookup(&keys("C-x")), Lookup::Command("forward-char"));
    }

    #[test]
    fn override_creates_missing_prefixes() {
        let mut keymap = Keymap::emacs_default();
        let overrides = BTreeMap::from([("<f9> x".to_string(), "forward-char".to_string())]);
        keymap.apply_overrides(&overrides).expect("allowed");
        assert_eq!(keymap.lookup(&keys("<f9>")), Lookup::Prefix);
        assert_eq!(
            keymap.lookup(&keys("<f9> x")),
            Lookup::Command("forward-char")
        );
    }

    #[test]
    fn printable_default_is_not_a_prefix() {
        let mut keymap = Keymap::emacs_default();
        let overrides = BTreeMap::from([("a b".to_string(), "forward-char".to_string())]);
        let error = keymap
            .apply_overrides(&overrides)
            .expect_err("a runs a command");
        assert_eq!(
            error.to_string(),
            "Key sequence a b starts with non-prefix key a"
        );
    }

    #[test]
    fn describe_keys_matches_emacs_key_description() {
        // (key-description (kbd "...")) in Emacs 29.3.
        for (sequence, description) in [
            ("ESC z", "M-z"),
            ("ESC C-c", "C-M-c"),
            ("C-x ESC ESC", "C-x ESC ESC"),
            ("ESC M-x", "ESC M-x"),
            ("ESC <f1>", "ESC <f1>"),
            ("ESC", "ESC"),
            ("C-c z", "C-c z"),
        ] {
            assert_eq!(describe_keys(&keys(sequence)), description, "{sequence}");
        }
    }

    #[test]
    fn universal_argument_map_filters_minus_after_digits() {
        let minus = keys("-")[0];
        assert_eq!(
            universal_argument_binding(minus, &PrefixArg::Raw(4)),
            Some(NEGATIVE_ARGUMENT)
        );
        assert_eq!(
            universal_argument_binding(minus, &PrefixArg::Numeric(3)),
            None
        );
        assert_eq!(
            universal_argument_binding(keys("C-u")[0], &PrefixArg::Numeric(3)),
            Some("universal-argument-more")
        );
        assert_eq!(
            universal_argument_binding(keys("M-5")[0], &PrefixArg::Raw(4)),
            None
        );
    }
}
