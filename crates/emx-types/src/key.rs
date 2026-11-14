use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

const MAX_FUNCTION_KEY: u8 = 35;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub meta: bool,
    pub shift: bool,
    pub super_: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Delete,
    Insert,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub mods: Mods,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KbdError {
    #[error("empty key sequence")]
    Empty,
    #[error("modifier prefix without a key in {0:?}")]
    MissingKey(String),
    #[error("unsupported modifier {0:?}- in {1:?}")]
    UnsupportedModifier(char, String),
    #[error("unknown key name <{0}>")]
    UnknownKeyName(String),
    #[error("function key out of range 1..={MAX_FUNCTION_KEY} in <{0}>")]
    FunctionKeyRange(String),
    #[error("unrecognized key {0:?}")]
    Unrecognized(String),
    #[error("expected exactly one key, found {0}")]
    NotOneKey(usize),
}

const NAMED_KEYS: [(&str, KeyCode); 5] = [
    ("RET", KeyCode::Enter),
    ("SPC", KeyCode::Char(' ')),
    ("TAB", KeyCode::Tab),
    ("DEL", KeyCode::Backspace),
    ("ESC", KeyCode::Escape),
];

const ANGLE_KEYS: [(&str, KeyCode); 10] = [
    ("delete", KeyCode::Delete),
    ("insert", KeyCode::Insert),
    ("up", KeyCode::Up),
    ("down", KeyCode::Down),
    ("left", KeyCode::Left),
    ("right", KeyCode::Right),
    ("home", KeyCode::Home),
    ("end", KeyCode::End),
    ("prior", KeyCode::PageUp),
    ("next", KeyCode::PageDown),
];

const DELETE_CHARACTER: char = '\u{7f}';
const FIRST_PRINTABLE: u32 = 0x20;
/// XOR with a C0 control character gives the character typed with Ctrl
/// (`^J` for 10), as Emacs prints them.
const CONTROL_TO_CARET: u32 = 0x40;

/// Control characters Emacs reads as the same event as a named key.
const CONTROL_ALIASES: [(char, KeyCode); 3] = [
    ('i', KeyCode::Tab),
    ('m', KeyCode::Enter),
    ('[', KeyCode::Escape),
];

impl Key {
    /// The form Emacs sees: C-i is TAB, C-m is RET, C-[ is ESC, and shift on
    /// a letter without Ctrl is folded into the character. With Ctrl the
    /// shift stays, because Emacs distinguishes C-S-a from C-a. C-? stays
    /// distinct from DEL.
    pub fn canonical(self) -> Key {
        Key::normalized(self.code, self.mods)
    }

    fn normalized(code: KeyCode, mut mods: Mods) -> Key {
        let mut code = code;
        // A control character is the key that types it: Emacs reads byte
        // 10 as C-j and 127 as DEL. No terminal sends them as characters,
        // but a client can.
        if let KeyCode::Char(character) = code {
            if character == DELETE_CHARACTER {
                code = KeyCode::Backspace;
            } else if (character as u32) < FIRST_PRINTABLE {
                let letter = char::from_u32(character as u32 ^ CONTROL_TO_CARET)
                    .expect("C0 controls map to ASCII")
                    .to_ascii_lowercase();
                code = KeyCode::Char(letter);
                mods.ctrl = true;
            }
        }
        if let KeyCode::Char(character) = code {
            if mods.ctrl {
                if let Some(lower) = single_case_mapping(character.to_lowercase(), character) {
                    code = KeyCode::Char(lower);
                    mods.shift = true;
                }
            } else if mods.shift {
                if let Some(upper) = single_case_mapping(character.to_uppercase(), character) {
                    code = KeyCode::Char(upper);
                    mods.shift = false;
                } else if single_case_mapping(character.to_lowercase(), character).is_some() {
                    mods.shift = false;
                }
            }
        }
        if let KeyCode::Char(character) = code
            && mods.ctrl
            && !mods.shift
            && let Some((_, alias)) = CONTROL_ALIASES
                .iter()
                .find(|(control, _)| *control == character)
        {
            code = *alias;
            mods.ctrl = false;
        }
        Key { code, mods }
    }
}

fn single_case_mapping(mut mapped: impl Iterator<Item = char>, original: char) -> Option<char> {
    let first = mapped.next()?;
    if mapped.next().is_some() || first == original {
        return None;
    }
    Some(first)
}

/// Returns canonical keys (see `Key::canonical`).
pub fn parse_kbd(s: &str) -> Result<Vec<Key>, KbdError> {
    let keys = s
        .split_whitespace()
        .map(parse_token)
        .collect::<Result<Vec<_>, _>>()?;
    if keys.is_empty() {
        return Err(KbdError::Empty);
    }
    Ok(keys)
}

fn parse_token(token: &str) -> Result<Key, KbdError> {
    let mut mods = Mods::default();
    let mut rest = token;
    loop {
        let mut characters = rest.chars();
        let (Some(prefix), Some('-')) = (characters.next(), characters.next()) else {
            break;
        };
        if !"ACHMSs".contains(prefix) {
            break;
        }
        let remainder = characters.as_str();
        if remainder.is_empty() {
            return Err(KbdError::MissingKey(token.to_string()));
        }
        match prefix {
            'C' => mods.ctrl = true,
            'M' => mods.meta = true,
            'S' => mods.shift = true,
            's' => mods.super_ = true,
            other => return Err(KbdError::UnsupportedModifier(other, token.to_string())),
        }
        rest = remainder;
    }
    Ok(Key::normalized(parse_base(rest)?, mods))
}

fn parse_base(base: &str) -> Result<KeyCode, KbdError> {
    if let Some(inner) = base
        .strip_prefix('<')
        .and_then(|stripped| stripped.strip_suffix('>'))
        .filter(|inner| !inner.is_empty())
    {
        return parse_angle(inner);
    }
    if let Some((_, code)) = NAMED_KEYS.iter().find(|(name, _)| *name == base) {
        return Ok(*code);
    }
    let mut characters = base.chars();
    match (characters.next(), characters.next()) {
        (Some(character), None) if !character.is_control() => Ok(KeyCode::Char(character)),
        _ => Err(KbdError::Unrecognized(base.to_string())),
    }
}

fn parse_angle(name: &str) -> Result<KeyCode, KbdError> {
    if let Some((_, code)) = ANGLE_KEYS.iter().find(|(candidate, _)| *candidate == name) {
        return Ok(*code);
    }
    if let Some(digits) = name.strip_prefix('f')
        && !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return match digits.parse::<u8>() {
            Ok(number) if (1..=MAX_FUNCTION_KEY).contains(&number) => Ok(KeyCode::F(number)),
            _ => Err(KbdError::FunctionKeyRange(name.to_string())),
        };
    }
    Err(KbdError::UnknownKeyName(name.to_string()))
}

pub fn format_key(k: &Key) -> String {
    let mut out = String::new();
    if k.mods.ctrl {
        out.push_str("C-");
    }
    if k.mods.meta {
        out.push_str("M-");
    }
    if k.mods.shift {
        out.push_str("S-");
    }
    if k.mods.super_ {
        out.push_str("s-");
    }
    match k.code {
        KeyCode::Char(' ') => out.push_str("SPC"),
        KeyCode::Char(character) => out.push(character),
        KeyCode::Enter => out.push_str("RET"),
        KeyCode::Tab => out.push_str("TAB"),
        KeyCode::Backspace => out.push_str("DEL"),
        KeyCode::Escape => out.push_str("ESC"),
        KeyCode::F(number) => out.push_str(&format!("<f{number}>")),
        named => {
            let name = ANGLE_KEYS
                .iter()
                .find(|(_, code)| *code == named)
                .map(|(name, _)| *name)
                .expect("every non-Char, non-F, non-RET-style KeyCode has an angle name");
            out.push('<');
            out.push_str(name);
            out.push('>');
        }
    }
    out
}

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&format_key(self))
    }
}

impl Serialize for Key {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let mut keys = parse_kbd(&text).map_err(serde::de::Error::custom)?;
        if keys.len() != 1 {
            return Err(serde::de::Error::custom(KbdError::NotOneKey(keys.len())));
        }
        Ok(keys.remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kbd_roundtrip() {
        for s in [
            "C-x C-f",
            "M-%",
            "C-M-s",
            "RET",
            "SPC",
            "TAB",
            "DEL",
            "<f8>",
            "C-SPC",
            "C-/",
            "C-_",
            "M-<",
            "C-x r t",
            "<up>",
            "S-<left>",
            "ESC",
            "C-S-a",
            "M-A",
            "<prior>",
            "<next>",
            "<delete>",
            "<insert>",
            "<home>",
            "<end>",
            "C-M-S-s-a",
            "C--",
            "-",
            "C-<f12>",
        ] {
            let keys = parse_kbd(s).unwrap();
            assert_eq!(keys.iter().map(format_key).collect::<Vec<_>>().join(" "), s);
        }
    }

    #[test]
    fn shift_letter_normalizes() {
        assert_eq!(parse_kbd("S-a").unwrap(), parse_kbd("A").unwrap());
        assert_eq!(parse_kbd("M-S-a").unwrap(), parse_kbd("M-A").unwrap());
    }

    #[test]
    fn ctrl_shift_letter_keeps_shift() {
        let keys = parse_kbd("C-S-a").unwrap();
        assert!(keys[0].mods.ctrl && keys[0].mods.shift);
        assert_ne!(keys, parse_kbd("C-a").unwrap());
        assert_eq!(keys, parse_kbd("C-A").unwrap());
    }

    #[test]
    fn modifier_order_is_canonical() {
        let keys = parse_kbd("M-C-s").unwrap();
        assert_eq!(format_key(&keys[0]), "C-M-s");
    }

    #[test]
    fn canonical_aliases_match_emacs() {
        assert_eq!(parse_kbd("C-i").unwrap(), parse_kbd("TAB").unwrap());
        assert_eq!(parse_kbd("C-m").unwrap(), parse_kbd("RET").unwrap());
        assert_eq!(parse_kbd("C-[").unwrap(), parse_kbd("ESC").unwrap());
        assert_eq!(parse_kbd("C-M-i").unwrap(), parse_kbd("M-TAB").unwrap());
        assert_eq!(parse_kbd("S-A").unwrap(), parse_kbd("A").unwrap());
        assert_eq!(parse_kbd("M-S-A").unwrap(), parse_kbd("M-A").unwrap());
        assert_ne!(parse_kbd("C-?").unwrap(), parse_kbd("DEL").unwrap());
        assert_eq!(format_key(&parse_kbd("C-?").unwrap()[0]), "C-?");
        assert_ne!(parse_kbd("C-S-i").unwrap(), parse_kbd("TAB").unwrap());
    }

    #[test]
    fn canonical_folds_raw_keys() {
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        let shift = Mods {
            shift: true,
            ..Mods::default()
        };
        let raw = |code, mods| Key { code, mods };
        assert_eq!(
            raw(KeyCode::Char('i'), ctrl).canonical(),
            raw(KeyCode::Tab, Mods::default())
        );
        assert_eq!(
            raw(KeyCode::Char('m'), ctrl).canonical(),
            raw(KeyCode::Enter, Mods::default())
        );
        assert_eq!(
            raw(KeyCode::Char('['), ctrl).canonical(),
            raw(KeyCode::Escape, Mods::default())
        );
        assert_eq!(
            raw(KeyCode::Char('A'), shift).canonical(),
            raw(KeyCode::Char('A'), Mods::default())
        );
        assert_eq!(
            raw(KeyCode::Char('a'), shift).canonical(),
            raw(KeyCode::Char('A'), Mods::default())
        );
        let left = raw(KeyCode::Left, shift);
        assert_eq!(left.canonical(), left);
    }

    #[test]
    fn control_characters_are_their_ctrl_keys() {
        let raw = |character| Key {
            code: KeyCode::Char(character),
            mods: Mods::default(),
        };
        let kbd = |text| parse_kbd(text).unwrap().remove(0);
        assert_eq!(raw('\n').canonical(), kbd("C-j"));
        assert_eq!(raw('\u{1}').canonical(), kbd("C-a"));
        assert_eq!(raw('\r').canonical(), kbd("RET"));
        assert_eq!(raw('\t').canonical(), kbd("TAB"));
        assert_eq!(raw('\u{1b}').canonical(), kbd("ESC"));
        assert_eq!(raw('\u{0}').canonical(), kbd("C-@"));
        assert_eq!(raw('\u{1f}').canonical(), kbd("C-_"));
        assert_eq!(raw('\u{7f}').canonical(), kbd("DEL"));
        let meta = Key {
            code: KeyCode::Char('\n'),
            mods: Mods {
                meta: true,
                ..Mods::default()
            },
        };
        assert_eq!(meta.canonical(), kbd("C-M-j"));
    }

    #[test]
    fn bad_kbd_errors() {
        assert!(parse_kbd("C-").is_err());
        assert!(parse_kbd("<nosuchkey>").is_err());
        assert!(parse_kbd("").is_err());
        assert!(parse_kbd("foo").is_err());
        assert!(parse_kbd("H-a").is_err());
        assert!(parse_kbd("<f0>").is_err());
        assert!(parse_kbd("<f999>").is_err());
    }

    #[test]
    fn key_serde_is_kbd_string() {
        let key = parse_kbd("C-x").unwrap().remove(0);
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, "\"C-x\"");
        assert_eq!(serde_json::from_str::<Key>(&json).unwrap(), key);
        assert!(serde_json::from_str::<Key>("\"C-x C-f\"").is_err());
        assert!(serde_json::from_str::<Key>("\"C-\"").is_err());
    }
}
