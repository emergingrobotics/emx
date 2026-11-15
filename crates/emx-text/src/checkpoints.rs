//! Column checkpoints: a per-line cache mapping between byte offsets and
//! display columns, so a very long line can be navigated without rescanning
//! it from the line start each time.
//!
//! A line's display column at a byte offset is the sum of the display widths
//! of the units before it (grapheme clusters, tabs to the next tab stop,
//! control and invalid bytes as escape notation). Computing it from the line
//! start is O(line length); for a 100 MB single line that is far too much to
//! do on every cursor placement or frame. Instead, as a long line is walked,
//! this cache records a checkpoint `(byte_offset, display_column)` at a
//! grapheme boundary roughly every `interval` bytes. A later query seeks to
//! the nearest checkpoint below its target and scans forward at most one
//! interval, so the per-query cost is bounded by the interval rather than by
//! the line length.

use std::num::NonZeroUsize;

use lru::LruCache;
use parking_lot::Mutex;

use crate::display::{display_units, unit_width};
use crate::source::ReadError;
use crate::text::Text;

/// Checkpoints are placed roughly this many bytes apart, so a query scans at
/// most this many bytes beyond the nearest checkpoint.
pub const COLUMN_CHECKPOINT_INTERVAL: u64 = 4096;
/// A line at least this long is cached with checkpoints; a shorter line is
/// scanned directly, since one scan of it already costs at most this much.
pub const LONG_LINE_THRESHOLD: u64 = 4096;
/// How many lines' checkpoints are kept before the least recently used line
/// is dropped.
const CACHED_LINES: usize = 1024;

/// One recorded point on a line: `column` display columns lie between the
/// line start and `offset`, and `offset` is a grapheme boundary.
#[derive(Debug, Clone, Copy)]
struct Checkpoint {
    offset: u64,
    column: u64,
}

/// The checkpoints known for one line, in ascending offset order starting
/// with `(line_start, 0)`. Once `complete` is set the last checkpoint is the
/// line's end (its newline, or the buffer end).
#[derive(Debug, Clone)]
struct LineCheckpoints {
    points: Vec<Checkpoint>,
    complete: bool,
}

impl LineCheckpoints {
    fn new(line_start: u64) -> LineCheckpoints {
        LineCheckpoints {
            points: vec![Checkpoint {
                offset: line_start,
                column: 0,
            }],
            complete: false,
        }
    }

    fn last(&self) -> Checkpoint {
        *self
            .points
            .last()
            .expect("a line always has its line-start checkpoint")
    }

    /// The last checkpoint whose offset is at or before `pos`.
    fn below_offset(&self, pos: u64) -> Checkpoint {
        let index = self.points.partition_point(|point| point.offset <= pos);
        self.points[index - 1]
    }

    /// The last checkpoint whose column is at or before `col`, with the offset
    /// of the next checkpoint above it (its own offset when it is the last).
    fn span_at_column(&self, col: u64) -> (Checkpoint, u64) {
        let index = self.points.partition_point(|point| point.column <= col);
        let base = self.points[index - 1];
        let upper = self
            .points
            .get(index)
            .map_or(base.offset, |next| next.offset);
        (base, upper)
    }
}

/// What a bounded probe at a line start found.
enum LineProbe {
    /// The line's end lies within the probe window.
    Short { line_end: u64 },
    /// No newline within a threshold of the line start: the line is long and
    /// uses checkpoints.
    Long,
}

/// A cache of column checkpoints for the long lines of one buffer version.
/// The cache is advisory: edits that shift offsets invalidate it, so callers
/// clear it when the text changes.
#[derive(Debug)]
pub struct ColumnCheckpoints {
    lines: Mutex<LruCache<u64, LineCheckpoints>>,
    tab_width: usize,
    interval: u64,
    long_line_threshold: u64,
}

impl ColumnCheckpoints {
    pub fn new(tab_width: usize, interval: u64, long_line_threshold: u64) -> ColumnCheckpoints {
        assert!(tab_width > 0, "tab width is at least one column");
        assert!(
            interval > 0,
            "a checkpoint interval spans at least one byte"
        );
        let capacity = NonZeroUsize::new(CACHED_LINES).expect("the cached-line count is nonzero");
        ColumnCheckpoints {
            lines: Mutex::new(LruCache::new(capacity)),
            tab_width,
            interval,
            long_line_threshold,
        }
    }

    /// Discards every cached line, after an edit shifted the offsets the
    /// checkpoints were keyed by.
    pub fn clear(&self) {
        self.lines.lock().clear();
    }

    /// The display column `pos` sits at on the line starting at `line_start`.
    /// Reads at most one interval of bytes beyond the nearest checkpoint.
    pub fn column_of(&self, text: &Text, line_start: u64, pos: u64) -> Result<u64, ReadError> {
        self.column_of_with(text, line_start, pos, false)
    }

    /// Like `column_of`, but never reads the file: it fails with `NotCached`
    /// rather than block on I/O, for the rendering path.
    pub fn column_of_cached(
        &self,
        text: &Text,
        line_start: u64,
        pos: u64,
    ) -> Result<u64, ReadError> {
        self.column_of_with(text, line_start, pos, true)
    }

    /// The first byte offset on the line starting at `line_start` whose
    /// display column is at least `col`, or the line's end when the line is
    /// shorter. Reads at most one interval of bytes beyond the nearest
    /// checkpoint.
    pub fn pos_at_column(&self, text: &Text, line_start: u64, col: u64) -> Result<u64, ReadError> {
        self.pos_at_column_with(text, line_start, col, false)
    }

    /// Like `pos_at_column`, but never reads the file, for the rendering path.
    pub fn pos_at_column_cached(
        &self,
        text: &Text,
        line_start: u64,
        col: u64,
    ) -> Result<u64, ReadError> {
        self.pos_at_column_with(text, line_start, col, true)
    }

    fn column_of_with(
        &self,
        text: &Text,
        line_start: u64,
        pos: u64,
        cached: bool,
    ) -> Result<u64, ReadError> {
        if let LineProbe::Short { .. } = self.probe(text, line_start, cached)? {
            return self.scan_column(text, line_start, 0, pos, cached);
        }
        let start = {
            let mut lines = self.lines.lock();
            let line = lines.get_or_insert_mut(line_start, || LineCheckpoints::new(line_start));
            self.extend_to_offset(text, line, pos, cached)?;
            line.below_offset(pos)
        };
        self.scan_column(text, start.offset, start.column, pos, cached)
    }

    fn pos_at_column_with(
        &self,
        text: &Text,
        line_start: u64,
        col: u64,
        cached: bool,
    ) -> Result<u64, ReadError> {
        if let LineProbe::Short { line_end } = self.probe(text, line_start, cached)? {
            return self.scan_pos_in_range(text, line_start, 0, line_end, col, cached);
        }
        self.pos_at_column_long(text, line_start, col, cached)
    }

    /// `pos_at_column` for a line already known to be long: the checkpoint
    /// path, without probing again.
    fn pos_at_column_long(
        &self,
        text: &Text,
        line_start: u64,
        col: u64,
        cached: bool,
    ) -> Result<u64, ReadError> {
        let (base, upper) = {
            let mut lines = self.lines.lock();
            let line = lines.get_or_insert_mut(line_start, || LineCheckpoints::new(line_start));
            self.extend_to_column(text, line, col, cached)?;
            line.span_at_column(col)
        };
        self.scan_pos_in_range(text, base.offset, base.column, upper, col, cached)
    }

    /// For the rendering seek: the byte at display column `col` on the line
    /// starting at `line_start`, or `None` when the line is short enough to be
    /// laid out from its start directly. Never reads the file (fails with
    /// `NotCached` instead), and reads at most one probe window plus one
    /// interval beyond the nearest checkpoint.
    pub fn seek_column_cached(
        &self,
        text: &Text,
        line_start: u64,
        col: u64,
    ) -> Result<Option<u64>, ReadError> {
        match self.probe(text, line_start, true)? {
            LineProbe::Short { .. } => Ok(None),
            LineProbe::Long => Ok(Some(self.pos_at_column_long(text, line_start, col, true)?)),
        }
    }

    /// Reads at most one threshold-sized window at `line_start` to tell a
    /// short line (whose end it finds) from a long one (which uses
    /// checkpoints). Never reads the whole of a long line.
    fn probe(&self, text: &Text, line_start: u64, cached: bool) -> Result<LineProbe, ReadError> {
        let length = text.len();
        let end = length.min(line_start + self.long_line_threshold);
        let mut bytes = Vec::new();
        read(text, line_start..end, &mut bytes, cached)?;
        if let Some(found) = memchr::memchr(b'\n', &bytes) {
            return Ok(LineProbe::Short {
                line_end: line_start + found as u64,
            });
        }
        if end == length {
            return Ok(LineProbe::Short { line_end: length });
        }
        Ok(LineProbe::Long)
    }

    /// The display column at `pos`, scanning from `from` (a grapheme
    /// boundary) whose column is `base_column`. The running column starts at
    /// `base_column` so tab stops are counted from the line start, not from
    /// `from`.
    fn scan_column(
        &self,
        text: &Text,
        from: u64,
        base_column: u64,
        pos: u64,
        cached: bool,
    ) -> Result<u64, ReadError> {
        if pos <= from {
            return Ok(base_column);
        }
        let mut bytes = Vec::new();
        read(text, from..pos, &mut bytes, cached)?;
        let mut column = base_column;
        for (_, unit) in display_units(&bytes) {
            column += unit_width(&unit, column_as_usize(column), self.tab_width) as u64;
        }
        Ok(column)
    }

    /// Within `[from, upper)` (both grapheme boundaries), the first offset
    /// whose column is at least `goal`, starting the running column at
    /// `base_column`. Returns `upper` when the span does not reach `goal`.
    fn scan_pos_in_range(
        &self,
        text: &Text,
        from: u64,
        base_column: u64,
        upper: u64,
        goal: u64,
        cached: bool,
    ) -> Result<u64, ReadError> {
        let mut bytes = Vec::new();
        read(text, from..upper, &mut bytes, cached)?;
        let mut column = base_column;
        for (offset, unit) in display_units(&bytes) {
            if column >= goal {
                return Ok(from + offset as u64);
            }
            column += unit_width(&unit, column_as_usize(column), self.tab_width) as u64;
        }
        Ok(upper)
    }

    /// Extends `line`'s checkpoints until one lies at or past `pos`, or the
    /// line is complete.
    fn extend_to_offset(
        &self,
        text: &Text,
        line: &mut LineCheckpoints,
        pos: u64,
        cached: bool,
    ) -> Result<(), ReadError> {
        while !line.complete && line.last().offset < pos {
            self.extend_once(text, line, cached)?;
        }
        Ok(())
    }

    /// Extends `line`'s checkpoints until one has a column at or past `col`,
    /// or the line is complete.
    fn extend_to_column(
        &self,
        text: &Text,
        line: &mut LineCheckpoints,
        col: u64,
        cached: bool,
    ) -> Result<(), ReadError> {
        while !line.complete && line.last().column < col {
            self.extend_once(text, line, cached)?;
        }
        Ok(())
    }

    /// Adds one checkpoint, at the first grapheme boundary at or past an
    /// interval of bytes beyond the last one, or the line end when that comes
    /// first. The read extends past the interval so the boundary there is
    /// confirmed by a following unit; a cluster longer than the read grows it,
    /// so progress is always made.
    fn extend_once(
        &self,
        text: &Text,
        line: &mut LineCheckpoints,
        cached: bool,
    ) -> Result<(), ReadError> {
        let base = line.last();
        let length = text.len();
        // Read past the interval so the boundary near it has a following unit
        // to confirm it (and is never a truncated multi-byte tail).
        let mut read_len = self.interval.saturating_mul(2);
        let mut bytes = Vec::new();
        loop {
            let end = length.min(base.offset + read_len);
            bytes.clear();
            read(text, base.offset..end, &mut bytes, cached)?;
            let newline = memchr::memchr(b'\n', &bytes);
            let segment = &bytes[..newline.unwrap_or(bytes.len())];
            let reached_end = newline.is_some() || end == length;
            let units: Vec<(usize, _)> = display_units(segment).collect();
            // Walk units, summing columns, until a boundary at or past the
            // interval is reached. A boundary is confirmed either because the
            // line ended (every unit is whole) or because a later unit follows
            // it (so the unit starting there is a complete character, not a
            // truncated tail that would merge leftward with more bytes).
            let mut column = base.column;
            for (index, (offset, unit)) in units.iter().enumerate() {
                let confirmed = reached_end || index + 1 < units.len();
                if *offset as u64 >= self.interval && confirmed {
                    line.points.push(Checkpoint {
                        offset: base.offset + *offset as u64,
                        column,
                    });
                    return Ok(());
                }
                column += unit_width(unit, column_as_usize(column), self.tab_width) as u64;
            }
            if reached_end {
                line.points.push(Checkpoint {
                    offset: base.offset + segment.len() as u64,
                    column,
                });
                line.complete = true;
                return Ok(());
            }
            read_len = read_len.saturating_mul(2);
        }
    }
}

fn column_as_usize(column: u64) -> usize {
    usize::try_from(column).expect("a display column fits in usize on a 64-bit target")
}

fn read(
    text: &Text,
    range: std::ops::Range<u64>,
    out: &mut Vec<u8>,
    cached: bool,
) -> Result<(), ReadError> {
    if cached {
        text.read_cached(range, out)
    } else {
        text.read(range, out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use proptest::prelude::*;

    use super::*;
    use crate::source::Sources;

    const PROPTEST_CASES: u32 = 64;
    /// How many query positions to sample across a generated line, spread so
    /// that some fall inside intervals and some on checkpoint boundaries.
    const SAMPLES: usize = 160;

    fn text_of(bytes: &[u8]) -> Text {
        Text::from_original(Sources::memory(Arc::from(bytes)))
    }

    /// One scan of the first line's units: each boundary offset with the
    /// display column reached there, ending with the line's end offset and
    /// total column width.
    fn scan_line(bytes: &[u8], tab_width: usize) -> Vec<(u64, u64)> {
        let line_end = memchr::memchr(b'\n', bytes).unwrap_or(bytes.len());
        let mut points = Vec::new();
        let mut column: u64 = 0;
        for (offset, unit) in display_units(&bytes[..line_end]) {
            points.push((offset as u64, column));
            column += unit_width(&unit, column_as_usize(column), tab_width) as u64;
        }
        points.push((line_end as u64, column));
        points
    }

    /// Picks up to `SAMPLES` indices spread across `len` items, always
    /// including the first and last.
    fn sample_indices(len: usize) -> Vec<usize> {
        if len <= SAMPLES {
            return (0..len).collect();
        }
        let mut indices: Vec<usize> = (0..SAMPLES)
            .map(|i| i * (len - 1) / (SAMPLES - 1))
            .collect();
        indices.dedup();
        indices
    }

    fn line_strategy() -> impl Strategy<Value = Vec<u8>> {
        // Wide characters, combining marks, tabs, control and ASCII bytes,
        // repeated into a line crossing several checkpoint intervals.
        let piece = prop_oneof![
            Just("a".as_bytes().to_vec()),
            Just("\u{4e2d}".as_bytes().to_vec()),
            Just("e\u{301}".as_bytes().to_vec()),
            Just("\t".as_bytes().to_vec()),
            Just("\u{1f600}".as_bytes().to_vec()),
            Just(vec![0x01]),
            Just(vec![0xff]),
            Just("  ".as_bytes().to_vec()),
        ];
        // At least 5000 one-byte-or-larger pieces, so the line always exceeds
        // the long-line threshold and crosses several checkpoint intervals.
        prop::collection::vec(piece, 5000..8000).prop_map(|pieces| pieces.concat())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(PROPTEST_CASES))]

        #[test]
        fn checkpoints_match_full_scan(line in line_strategy(), tab_width in 1usize..=16) {
            let text = text_of(&line);
            let checkpoints = ColumnCheckpoints::new(
                tab_width,
                COLUMN_CHECKPOINT_INTERVAL,
                LONG_LINE_THRESHOLD,
            );
            prop_assert!(line.len() as u64 >= LONG_LINE_THRESHOLD, "line crosses intervals");
            // One scan gives the ground truth: every boundary's column.
            let points = scan_line(&line, tab_width);
            let (line_end, max_column) = *points.last().expect("the line end point");

            for &index in &sample_indices(points.len()) {
                let (offset, expected) = points[index];
                let actual = checkpoints.column_of(&text, 0, offset).expect("column_of reads memory");
                prop_assert_eq!(actual, expected, "column_of at offset {}", offset);
            }

            // For each sampled goal column, the expected offset is the first
            // boundary whose column is at least the goal, or the line end.
            let columns: Vec<u64> = points.iter().map(|point| point.1).collect();
            let goals: Vec<u64> = sample_indices(points.len())
                .into_iter()
                .flat_map(|index| {
                    let column = points[index].1;
                    [column.saturating_sub(1), column, column + 1]
                })
                .chain([max_column + 7])
                .collect();
            for goal in goals {
                let index = columns.partition_point(|column| *column < goal);
                let expected = points.get(index).map_or(line_end, |point| point.0);
                let actual = checkpoints
                    .pos_at_column(&text, 0, goal)
                    .expect("pos_at_column reads memory");
                prop_assert_eq!(actual, expected, "pos_at_column at goal {}", goal);
            }
        }
    }

    #[test]
    fn short_line_is_scanned_without_caching() {
        let text = text_of(b"short line\nsecond");
        let checkpoints =
            ColumnCheckpoints::new(8, COLUMN_CHECKPOINT_INTERVAL, LONG_LINE_THRESHOLD);
        assert_eq!(checkpoints.column_of(&text, 0, 5).expect("read"), 5);
        assert_eq!(checkpoints.pos_at_column(&text, 0, 5).expect("read"), 5);
        // The goal past the line end clamps to the newline position.
        assert_eq!(checkpoints.pos_at_column(&text, 0, 99).expect("read"), 10);
        assert!(
            checkpoints.lines.lock().is_empty(),
            "short lines are not cached"
        );
    }
}
