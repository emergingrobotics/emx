//! Emacs `show-paren-mode` (on by default since Emacs 28): the paren
//! before point when it closes, else the one after point when it opens,
//! and the paren it matches. Reads only cached text, never blocking
//! redisplay, and skips parens in comments and strings by their faces.

use emx_text::Text;
use emx_types::Face;

use crate::faces::FaceSpans;

const PAIRS: [(u8, u8); 3] = [(b'(', b')'), (b'[', b']'), (b'{', b'}')];

/// A paren next to point and what it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParenMatch {
    pub here: u64,
    /// The other paren; `None` when there is none within reach.
    pub there: Option<u64>,
    /// The two parens are of a kind.
    pub matched: bool,
}

fn is_open(byte: u8) -> bool {
    PAIRS.iter().any(|(open, _)| *open == byte)
}

fn is_close(byte: u8) -> bool {
    PAIRS.iter().any(|(_, close)| *close == byte)
}

fn pair(open: u8, close: u8) -> bool {
    PAIRS.contains(&(open, close))
}

/// Code, not a comment or string, as the highlighting says.
fn in_code(faces: &FaceSpans, pos: u64) -> bool {
    !matches!(faces.face_at(pos), Some(Face::Comment | Face::String))
}

pub(crate) fn show_paren(
    text: &Text,
    faces: &FaceSpans,
    point: u64,
    max_distance: u64,
) -> Option<ParenMatch> {
    let byte_at = |pos: u64| -> Option<u8> {
        let mut byte = Vec::new();
        text.read_cached(pos..pos + 1, &mut byte).ok()?;
        byte.first().copied()
    };
    if point > 0
        && let Some(close) = byte_at(point - 1)
        && is_close(close)
        && in_code(faces, point - 1)
    {
        let here = point - 1;
        let from = here.saturating_sub(max_distance);
        let mut bytes = Vec::new();
        text.read_cached(from..here, &mut bytes).ok()?;
        let mut depth = 0usize;
        for (offset, byte) in bytes.iter().enumerate().rev() {
            let pos = from + offset as u64;
            if !in_code(faces, pos) {
                continue;
            }
            if is_close(*byte) {
                depth += 1;
            } else if is_open(*byte) {
                if depth == 0 {
                    return Some(ParenMatch {
                        here,
                        there: Some(pos),
                        matched: pair(*byte, close),
                    });
                }
                depth -= 1;
            }
        }
        return Some(ParenMatch {
            here,
            there: None,
            matched: false,
        });
    }
    if point < text.len()
        && let Some(open) = byte_at(point)
        && is_open(open)
        && in_code(faces, point)
    {
        let to = point
            .saturating_add(1)
            .saturating_add(max_distance)
            .min(text.len());
        let mut bytes = Vec::new();
        text.read_cached(point + 1..to, &mut bytes).ok()?;
        let mut depth = 0usize;
        for (offset, byte) in bytes.iter().enumerate() {
            let pos = point + 1 + offset as u64;
            if !in_code(faces, pos) {
                continue;
            }
            if is_open(*byte) {
                depth += 1;
            } else if is_close(*byte) {
                if depth == 0 {
                    return Some(ParenMatch {
                        here: point,
                        there: Some(pos),
                        matched: pair(open, *byte),
                    });
                }
                depth -= 1;
            }
        }
        return Some(ParenMatch {
            here: point,
            there: None,
            matched: false,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use emx_text::Sources;

    use super::*;

    fn text(source: &str) -> Text {
        Text::from_original(Sources::memory(Arc::from(source.as_bytes())))
    }

    fn at(source: &str, point: u64) -> Option<ParenMatch> {
        show_paren(&text(source), &FaceSpans::new(), point, 1000)
    }

    #[test]
    fn a_closing_paren_before_point_comes_first() {
        let source = "f(a[1]) (x)";
        assert_eq!(
            at(source, 7),
            Some(ParenMatch {
                here: 6,
                there: Some(1),
                matched: true
            })
        );
        assert_eq!(
            at(source, 1),
            Some(ParenMatch {
                here: 1,
                there: Some(6),
                matched: true
            })
        );
        assert_eq!(at(source, 3).map(|found| found.there), Some(Some(5)));
        assert_eq!(at(source, 0), None);
    }

    #[test]
    fn mismatched_and_unbalanced_parens() {
        assert_eq!(
            at("(]", 2),
            Some(ParenMatch {
                here: 1,
                there: Some(0),
                matched: false
            })
        );
        assert_eq!(
            at("x)", 2),
            Some(ParenMatch {
                here: 1,
                there: None,
                matched: false
            })
        );
    }

    #[test]
    fn parens_in_comments_and_strings_are_skipped() {
        let source = "(\")\")";
        let mut faces = FaceSpans::new();
        faces.replace_range(0..5, vec![(1..4, Face::String)]);
        assert_eq!(
            show_paren(&text(source), &faces, 0, 1000).map(|found| found.there),
            Some(Some(4))
        );
        assert_eq!(
            show_paren(&text(source), &faces, 3, 1000),
            None,
            "inside a string"
        );
    }
}
