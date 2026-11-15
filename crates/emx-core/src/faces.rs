//! Faces a buffer's text is drawn in, as byte ranges: what Emacs keeps as
//! `face` text properties. They move with edits the way markers do, so a
//! highlight computed for older text stays on the same characters until a
//! newer one replaces it.

use std::ops::Range;

use emx_types::Face;

/// Sorted, non-overlapping, non-empty ranges, each with its face.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FaceSpans {
    spans: Vec<(Range<u64>, Face)>,
}

impl FaceSpans {
    pub fn new() -> FaceSpans {
        FaceSpans::default()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn clear(&mut self) {
        self.spans.clear();
    }

    pub fn spans(&self) -> &[(Range<u64>, Face)] {
        &self.spans
    }

    /// The face of the byte at `pos`, if a span covers it.
    pub fn face_at(&self, pos: u64) -> Option<Face> {
        let index = self.spans.partition_point(|(range, _)| range.end <= pos);
        self.spans
            .get(index)
            .filter(|(range, _)| range.start <= pos)
            .map(|(_, face)| *face)
    }

    /// Spans overlapping `within`, clipped to it.
    pub fn overlapping(&self, within: Range<u64>) -> impl Iterator<Item = (Range<u64>, Face)> + '_ {
        let first = self
            .spans
            .partition_point(|(range, _)| range.end <= within.start);
        self.spans[first..]
            .iter()
            .take_while(move |(range, _)| range.start < within.end)
            .map(move |(range, face)| {
                (
                    range.start.max(within.start)..range.end.min(within.end),
                    *face,
                )
            })
    }

    /// Replaces whatever covers `range` with `spans`, which must lie inside
    /// it, be sorted and not overlap. Spans reaching across `range`'s ends
    /// are cut at them.
    pub fn replace_range(&mut self, range: Range<u64>, spans: Vec<(Range<u64>, Face)>) {
        debug_assert!(
            spans
                .iter()
                .all(|(span, _)| range.start <= span.start && span.end <= range.end),
            "replacement spans lie inside the replaced range"
        );
        debug_assert!(
            spans
                .windows(2)
                .all(|pair| pair[0].0.end <= pair[1].0.start),
            "replacement spans are sorted and disjoint"
        );
        let mut result = Vec::with_capacity(self.spans.len() + spans.len());
        let mut replacement = Some(spans);
        for (span, face) in self.spans.drain(..) {
            if span.end <= range.start {
                result.push((span, face));
                continue;
            }
            if span.start < range.start {
                result.push((span.start..range.start, face));
            }
            if span.start >= range.end {
                if let Some(spans) = replacement.take() {
                    result.extend(spans);
                }
                result.push((span, face));
                continue;
            }
            if let Some(spans) = replacement.take() {
                result.extend(spans);
            }
            if span.end > range.end {
                result.push((range.end..span.end, face));
            }
        }
        if let Some(spans) = replacement {
            result.extend(spans);
        }
        result.retain(|(span, _)| !span.is_empty());
        self.spans = result;
    }

    /// `length` bytes were inserted at `at`. Text inserted inside a span
    /// takes its face; text inserted where one starts or ends does not.
    pub fn adjust_for_insert(&mut self, at: u64, length: u64) {
        for (span, _) in &mut self.spans {
            if at <= span.start {
                span.start += length;
                span.end += length;
            } else if at < span.end {
                span.end += length;
            }
        }
    }

    pub fn adjust_for_delete(&mut self, deleted: &Range<u64>) {
        let length = deleted.end - deleted.start;
        let moved = |pos: u64| {
            if pos <= deleted.start {
                pos
            } else if pos >= deleted.end {
                pos - length
            } else {
                deleted.start
            }
        };
        for (span, _) in &mut self.spans {
            *span = moved(span.start)..moved(span.end);
        }
        self.spans.retain(|(span, _)| !span.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(list: &[(u64, u64, Face)]) -> FaceSpans {
        FaceSpans {
            spans: list
                .iter()
                .map(|(start, end, face)| (*start..*end, *face))
                .collect(),
        }
    }

    const K: Face = Face::Region;
    const S: Face = Face::Error;

    #[test]
    fn face_at_finds_the_covering_span() {
        let faces = spans(&[(2, 4, K), (6, 7, S)]);
        assert_eq!(faces.face_at(1), None);
        assert_eq!(faces.face_at(2), Some(K));
        assert_eq!(faces.face_at(3), Some(K));
        assert_eq!(faces.face_at(4), None);
        assert_eq!(faces.face_at(6), Some(S));
        assert_eq!(faces.face_at(7), None);
    }

    #[test]
    fn insertion_shifts_or_extends() {
        let mut faces = spans(&[(2, 4, K), (6, 7, S)]);
        faces.adjust_for_insert(3, 2);
        assert_eq!(faces, spans(&[(2, 6, K), (8, 9, S)]));
        faces.adjust_for_insert(2, 1);
        assert_eq!(faces, spans(&[(3, 7, K), (9, 10, S)]));
        faces.adjust_for_insert(7, 1);
        assert_eq!(faces, spans(&[(3, 7, K), (10, 11, S)]));
    }

    #[test]
    fn deletion_shrinks_and_drops() {
        let mut faces = spans(&[(2, 4, K), (6, 8, S)]);
        faces.adjust_for_delete(&(3..7));
        assert_eq!(faces, spans(&[(2, 3, K), (3, 4, S)]));
        faces.adjust_for_delete(&(2..4));
        assert!(faces.is_empty());
    }

    #[test]
    fn replace_range_cuts_and_inserts() {
        let mut faces = spans(&[(0, 10, K), (12, 14, S), (20, 22, K)]);
        faces.replace_range(5..15, vec![(6..8, S)]);
        assert_eq!(faces, spans(&[(0, 5, K), (6, 8, S), (20, 22, K)]));
        faces.replace_range(30..40, vec![(31..32, K)]);
        assert_eq!(
            faces,
            spans(&[(0, 5, K), (6, 8, S), (20, 22, K), (31, 32, K)])
        );
        faces.replace_range(0..100, Vec::new());
        assert!(faces.is_empty());
        faces.replace_range(0..1, vec![(0..1, S)]);
        assert_eq!(faces, spans(&[(0, 1, S)]));
    }

    #[test]
    fn overlapping_clips_to_the_range() {
        let faces = spans(&[(0, 4, K), (6, 9, S)]);
        assert_eq!(
            faces.overlapping(2..7).collect::<Vec<_>>(),
            vec![(2..4, K), (6..7, S)]
        );
        assert_eq!(faces.overlapping(4..6).count(), 0);
    }
}
