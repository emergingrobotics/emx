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

impl Key {
    // Shift on a plain letter is folded into the character; with Ctrl the
    // shift stays because Emacs distinguishes C-S-a from C-a.
    fn normalized(code: KeyCode, mut mods: Mods) -> Key {
        let mut code = code;
        if let KeyCode::Char(character) = code {
            if mods.ctrl {
                if let Some(lower) = single_case_mapping(character.to_lowercase(), character) {
                    code = KeyCode::Char(lower);
                    mods.shift = true;
                }
            } else if mods.shift
                && let Some(upper) = single_case_mapping(character.to_uppercase(), character)
            {
                code = KeyCode::Char(upper);
                mods.shift = false;
            }
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
