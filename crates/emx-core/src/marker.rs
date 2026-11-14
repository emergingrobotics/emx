use std::ops::Range;

slotmap::new_key_type! {
    pub struct MarkerId;
}

/// Emacs marker insertion type: `StayBefore` is insertion-type nil (text
/// inserted at the marker goes after it), `AdvanceAfter` is insertion-type t.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertionType {
    StayBefore,
    AdvanceAfter,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Marker {
    pub(crate) pos: u64,
    pub(crate) insertion_type: InsertionType,
    /// False for window point and start, which stand in for Emacs PT and
    /// window-start: undo must not record adjustments for them.
    pub(crate) record_in_undo: bool,
}

impl Marker {
    pub(crate) fn adjust_for_insert(&mut self, at: u64, len: u64) {
        let advances =
            self.pos > at || (self.pos == at && self.insertion_type == InsertionType::AdvanceAfter);
        if advances {
            self.pos += len;
        }
    }

    pub(crate) fn adjust_for_delete(&mut self, range: &Range<u64>) {
        if self.pos > range.end {
            self.pos -= range.end - range.start;
        } else if self.pos > range.start {
            self.pos = range.start;
        }
    }

    /// The offset Emacs `record_marker_adjustments` records for a marker
    /// inside a deletion: reinserting the text leaves a StayBefore marker at
    /// the start and an AdvanceAfter marker at the end, and undo subtracts
    /// this offset to put the marker back where it was.
    pub(crate) fn deletion_adjustment(&self, range: &Range<u64>) -> Option<i64> {
        if !self.record_in_undo || self.pos < range.start || self.pos > range.end {
            return None;
        }
        let adjustment = match self.insertion_type {
            InsertionType::StayBefore => range.start as i64 - self.pos as i64,
            InsertionType::AdvanceAfter => range.end as i64 - self.pos as i64,
        };
        (adjustment != 0).then_some(adjustment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(pos: u64, insertion_type: InsertionType) -> Marker {
        Marker {
            pos,
            insertion_type,
            record_in_undo: true,
        }
    }

    #[test]
    fn insertion_at_marker_moves_only_advance_after() {
        let mut stay = marker(5, InsertionType::StayBefore);
        let mut advance = marker(5, InsertionType::AdvanceAfter);
        stay.adjust_for_insert(5, 3);
        advance.adjust_for_insert(5, 3);
        assert_eq!((stay.pos, advance.pos), (5, 8));
        stay.adjust_for_insert(6, 3);
        assert_eq!(stay.pos, 5);
    }

    #[test]
    fn deletion_moves_inside_to_start_and_after_left() {
        let mut inside = marker(4, InsertionType::StayBefore);
        let mut at_end = marker(6, InsertionType::AdvanceAfter);
        let mut after = marker(9, InsertionType::StayBefore);
        let mut before = marker(2, InsertionType::StayBefore);
        for m in [&mut inside, &mut at_end, &mut after, &mut before] {
            m.adjust_for_delete(&(3..6));
        }
        assert_eq!(
            (inside.pos, at_end.pos, after.pos, before.pos),
            (3, 3, 6, 2)
        );
    }

    #[test]
    fn deletion_adjustment_matches_emacs_record_marker_adjustments() {
        // Emacs records (MARKER . -1) for a nil-type marker one past FROM and
        // (MARKER . 1) for a t-type marker one before TO.
        assert_eq!(
            marker(4, InsertionType::StayBefore).deletion_adjustment(&(3..6)),
            Some(-1)
        );
        assert_eq!(
            marker(5, InsertionType::AdvanceAfter).deletion_adjustment(&(3..6)),
            Some(1)
        );
        assert_eq!(
            marker(3, InsertionType::StayBefore).deletion_adjustment(&(3..6)),
            None
        );
        assert_eq!(
            marker(7, InsertionType::StayBefore).deletion_adjustment(&(3..6)),
            None
        );
    }
}
