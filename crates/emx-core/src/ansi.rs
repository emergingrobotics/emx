//! `ansi-color-mode` (DESIGN section 7: "an optional interpret ANSI colors
//! view for logs renders SGR color codes only"): SGR sequences
//! (`ESC [ ... m`) are hidden and the text after them is drawn in their
//! colors. Other escape sequences stay visible, escaped. The buffer's
//! bytes are untouched.

use std::ops::Range;

use emx_types::Face;

const ESCAPE: u8 = 0x1b;
const CSI_OPEN: u8 = b'[';
const SGR_FINAL: u8 = b'm';
/// SGR sequences longer than this are not believed to be colors.
const SGR_MAX_LEN: usize = 64;
const DEFAULT_COLOR: u8 = 16;

/// The byte ranges of the SGR sequences in `bytes`.
pub(crate) fn sgr_sequences(bytes: &[u8]) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] != ESCAPE || bytes[index + 1] != CSI_OPEN {
            index += 1;
            continue;
        }
        let parameters = bytes[index + 2..]
            .iter()
            .take(SGR_MAX_LEN)
            .position(|byte| !(byte.is_ascii_digit() || *byte == b';'));
        match parameters {
            Some(length) if bytes[index + 2 + length] == SGR_FINAL => {
                let end = index + 3 + length;
                found.push(index..end);
                index = end;
            }
            _ => index += 1,
        }
    }
    found
}

/// The colors SGR sequences give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct State {
    foreground: u8,
    background: u8,
    bold: bool,
}

const PLAIN: State = State {
    foreground: DEFAULT_COLOR,
    background: DEFAULT_COLOR,
    bold: false,
};

impl State {
    fn apply(&mut self, parameters: &[u8]) {
        let numbers: Vec<u32> = parameters
            .split(|byte| *byte == b';')
            .map(|field| {
                std::str::from_utf8(field)
                    .ok()
                    .and_then(|text| text.parse().ok())
                    .unwrap_or(0)
            })
            .collect();
        let mut index = 0;
        while index < numbers.len() {
            match numbers[index] {
                0 => *self = PLAIN,
                1 => self.bold = true,
                22 => self.bold = false,
                code @ 30..=37 => self.foreground = (code - 30) as u8,
                39 => self.foreground = DEFAULT_COLOR,
                code @ 40..=47 => self.background = (code - 40) as u8,
                49 => self.background = DEFAULT_COLOR,
                code @ 90..=97 => self.foreground = (code - 90 + 8) as u8,
                code @ 100..=107 => self.background = (code - 100 + 8) as u8,
                // 256-color and true-color colors are skipped, with their
                // arguments.
                38 | 48 => {
                    index += match numbers.get(index + 1) {
                        Some(5) => 2,
                        Some(2) => 4,
                        _ => 0,
                    };
                }
                _ => {}
            }
            index += 1;
        }
    }

    fn face(self) -> Option<Face> {
        (self != PLAIN).then_some(Face::Ansi {
            foreground: self.foreground,
            background: self.background,
            bold: self.bold,
        })
    }
}

/// The colors of the text in `bytes` (which start at buffer position
/// `base`, with no color in effect), as face spans; the sequences
/// themselves get none.
pub(crate) fn ansi_faces(bytes: &[u8], base: u64) -> Vec<(Range<u64>, Face)> {
    let mut spans: Vec<(Range<u64>, Face)> = Vec::new();
    let mut state = PLAIN;
    let mut at = 0;
    let paint = |range: Range<usize>, state: State, spans: &mut Vec<(Range<u64>, Face)>| {
        if let (false, Some(face)) = (range.is_empty(), state.face()) {
            spans.push((base + range.start as u64..base + range.end as u64, face));
        }
    };
    for sequence in sgr_sequences(bytes) {
        paint(at..sequence.start, state, &mut spans);
        state.apply(&bytes[sequence.start + 2..sequence.end - 1]);
        at = sequence.end;
    }
    paint(at..bytes.len(), state, &mut spans);
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ansi(foreground: u8, bold: bool) -> Face {
        Face::Ansi {
            foreground,
            background: DEFAULT_COLOR,
            bold,
        }
    }

    #[test]
    fn sgr_sequences_are_found_and_other_sequences_left() {
        let bytes = b"a\x1b[31mred\x1b[0m \x1b[2Jx\x1b[1;32mg";
        assert_eq!(sgr_sequences(bytes), vec![1..6, 9..13, 19..26]);
        assert!(sgr_sequences(b"\x1b[").is_empty());
        assert!(sgr_sequences(b"\x1b[31").is_empty());
    }

    #[test]
    fn colors_follow_the_sequences() {
        let bytes = b"a\x1b[31mred\x1b[0m b\x1b[1;92mok\x1b[39mz";
        assert_eq!(
            ansi_faces(bytes, 100),
            vec![
                (106..109, ansi(1, false)),
                (122..124, ansi(10, true)),
                (129..130, ansi(DEFAULT_COLOR, true)),
            ]
        );
        assert_eq!(
            ansi_faces(b"\x1b[38;5;196mx\x1b[38;2;1;2;3;4my", 0),
            vec![],
            "extended colors are skipped with their arguments"
        );
    }
}
