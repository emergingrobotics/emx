use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const CONTROL_RANGE_END: u8 = 0x20;
const DELETE: u8 = 0x7f;
const C1_START: u32 = 0x80;
const C1_END: u32 = 0x9f;
const MAX_GRAPHEME_WIDTH: usize = 2;
const CONTROL_WIDTH: usize = 2;
// Octal escape such as `\377`.
const ESCAPE_WIDTH: usize = 4;

/// One cell-producing unit of file bytes. The renderer never emits raw
/// control or invalid bytes: `Control` draws as `^X`, and `InvalidByte` and
/// `C1` draw as `\ooo` octal of the byte or code point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayUnit {
    Grapheme {
        len: usize,
        width: u8,
    },
    Tab,
    Newline,
    Control(u8),
    InvalidByte(u8),
    /// A validly encoded C1 code point (U+0080..U+009F), holding its low
    /// byte. Raw, U+009B is CSI on some terminals.
    C1(u8),
}

pub fn display_units(bytes: &[u8]) -> impl Iterator<Item = (usize, DisplayUnit)> + '_ {
    Units {
        bytes,
        position: 0,
        run_start: 0,
        run: "",
    }
}

struct Units<'a> {
    bytes: &'a [u8],
    position: usize,
    run_start: usize,
    // Longest valid UTF-8 prefix found at `run_start`, cached so a long valid
    // line is validated once rather than once per grapheme.
    run: &'a str,
}

impl<'a> Units<'a> {
    fn ensure_run(&mut self) {
        if self.position >= self.run_start && self.position < self.run_start + self.run.len() {
            return;
        }
        let validated = match std::str::from_utf8(&self.bytes[self.position..]) {
            Ok(text) => text,
            Err(error) => {
                std::str::from_utf8(&self.bytes[self.position..self.position + error.valid_up_to()])
                    .expect("valid_up_to marks a valid prefix")
            }
        };
        self.run_start = self.position;
        self.run = validated;
    }
}

impl Iterator for Units<'_> {
    type Item = (usize, DisplayUnit);

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.position;
        let byte = *self.bytes.get(start)?;
        let (length, unit) = match byte {
            b'\n' => (1, DisplayUnit::Newline),
            b'\t' => (1, DisplayUnit::Tab),
            0..CONTROL_RANGE_END | DELETE => (1, DisplayUnit::Control(byte)),
            _ => {
                self.ensure_run();
                let remaining = &self.run[start - self.run_start..];
                match remaining.chars().next() {
                    None => (1, DisplayUnit::InvalidByte(byte)),
                    Some(character) if (C1_START..=C1_END).contains(&u32::from(character)) => {
                        let low_byte =
                            u8::try_from(u32::from(character) & 0xff).expect("masked to one byte");
                        (character.len_utf8(), DisplayUnit::C1(low_byte))
                    }
                    Some(_) => {
                        let cluster = remaining
                            .graphemes(true)
                            .next()
                            .expect("non-empty text has a grapheme");
                        let width = cluster.width().clamp(1, MAX_GRAPHEME_WIDTH);
                        (
                            cluster.len(),
                            DisplayUnit::Grapheme {
                                len: cluster.len(),
                                width: u8::try_from(width).expect("clamped to at most 2"),
                            },
                        )
                    }
                }
            }
        };
        self.position += length;
        Some((start, unit))
    }
}

pub fn unit_width(unit: &DisplayUnit, column: usize, tab_width: usize) -> usize {
    match unit {
        DisplayUnit::Tab => tab_width - (column % tab_width),
        DisplayUnit::Control(_) => CONTROL_WIDTH,
        DisplayUnit::InvalidByte(_) | DisplayUnit::C1(_) => ESCAPE_WIDTH,
        DisplayUnit::Newline => 0,
        DisplayUnit::Grapheme { width, .. } => usize::from(*width),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(bytes: &[u8]) -> Vec<(usize, DisplayUnit)> {
        display_units(bytes).collect()
    }

    fn grapheme(len: usize, width: u8) -> DisplayUnit {
        DisplayUnit::Grapheme { len, width }
    }

    #[test]
    fn display_control_and_invalid_bytes() {
        let result = units(b"a\x1b\xffb");
        assert_eq!(
            result,
            vec![
                (0, grapheme(1, 1)),
                (1, DisplayUnit::Control(0x1b)),
                (2, DisplayUnit::InvalidByte(0xff)),
                (3, grapheme(1, 1)),
            ]
        );
        assert_eq!(unit_width(&result[1].1, 1, 8), 2);
        assert_eq!(unit_width(&result[2].1, 2, 8), 4);
    }

    #[test]
    fn display_wide_and_combining() {
        assert_eq!(units("e\u{301}".as_bytes()), vec![(0, grapheme(3, 1))]);
        assert_eq!(units("中".as_bytes()), vec![(0, grapheme(3, 2))]);
    }

    #[test]
    fn tab_advances_to_stop() {
        assert_eq!(unit_width(&DisplayUnit::Tab, 3, 8), 5);
        assert_eq!(unit_width(&DisplayUnit::Tab, 8, 8), 8);
        assert_eq!(unit_width(&DisplayUnit::Tab, 0, 4), 4);
    }

    #[test]
    fn delete_is_control_and_cr_is_not_newline() {
        assert_eq!(
            units(b"\x7f\r\n"),
            vec![
                (0, DisplayUnit::Control(0x7f)),
                (1, DisplayUnit::Control(0x0d)),
                (2, DisplayUnit::Newline),
            ]
        );
        assert_eq!(unit_width(&DisplayUnit::Newline, 0, 8), 0);
    }

    #[test]
    fn c1_code_points_are_escaped_not_raw() {
        let result = units("a\u{9b}b".as_bytes());
        assert_eq!(
            result,
            vec![
                (0, grapheme(1, 1)),
                (1, DisplayUnit::C1(0x9b)),
                (3, grapheme(1, 1)),
            ]
        );
        assert_eq!(unit_width(&DisplayUnit::C1(0x9b), 0, 8), 4);
    }

    #[test]
    fn c1_after_base_char_is_not_clustered() {
        let result = units("e\u{85}".as_bytes());
        assert_eq!(
            result,
            vec![(0, grapheme(1, 1)), (1, DisplayUnit::C1(0x85))]
        );
    }

    #[test]
    fn lone_combining_mark_and_zwj_have_width_one() {
        assert_eq!(units("\u{301}".as_bytes()), vec![(0, grapheme(2, 1))]);
        assert_eq!(units("\u{200d}".as_bytes()), vec![(0, grapheme(3, 1))]);
    }

    #[test]
    fn truncated_sequence_at_end_is_invalid_per_byte() {
        let euro = "€".as_bytes();
        let result = units(&euro[..2]);
        assert_eq!(
            result,
            vec![
                (0, DisplayUnit::InvalidByte(euro[0])),
                (1, DisplayUnit::InvalidByte(euro[1])),
            ]
        );
    }

    #[test]
    fn invalid_byte_between_valid_text_resumes() {
        let result = units("é\u{ff}x".as_bytes());
        assert_eq!(result.len(), 3);
        let result = units(b"\xc3\x28z");
        assert_eq!(
            result,
            vec![
                (0, DisplayUnit::InvalidByte(0xc3)),
                (1, grapheme(1, 1)),
                (2, grapheme(1, 1)),
            ]
        );
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(units(b"").is_empty());
    }

    #[test]
    fn units_cover_input_exactly() {
        let input = "a\tb\u{301}中\x01\u{9f}\n".as_bytes();
        let mut expected_offset = 0;
        for (offset, unit) in display_units(input) {
            assert_eq!(offset, expected_offset);
            expected_offset += match unit {
                DisplayUnit::Grapheme { len, .. } => len,
                DisplayUnit::C1(_) => 2,
                _ => 1,
            };
        }
        assert_eq!(expected_offset, input.len());
    }
}
