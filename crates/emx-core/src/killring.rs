use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KillRingError {
    #[error("Kill ring is empty")]
    Empty,
}

/// Emacs `kill-ring` with `kill-ring-yank-pointer`. Entries are bytes,
/// since buffers may hold invalid UTF-8; the newest kill is first.
#[derive(Debug, Clone)]
pub struct KillRing {
    entries: VecDeque<Vec<u8>>,
    yank_pointer: usize,
    max: usize,
    /// Counts changes to the newest kill, so whoever mirrors it (the
    /// terminal clipboard) knows when it changed.
    generation: u64,
}

impl KillRing {
    /// `max` is `kill-ring-max`; config validation guarantees it is nonzero.
    pub fn new(max: usize) -> KillRing {
        assert!(max > 0, "kill-ring-max is validated to be nonzero");
        KillRing {
            entries: VecDeque::new(),
            yank_pointer: 0,
            max,
            generation: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Newest first.
    pub fn entries(&self) -> impl Iterator<Item = &[u8]> {
        self.entries.iter().map(Vec::as_slice)
    }

    /// Emacs `kill-new`: push, drop the oldest past the maximum, and point
    /// the yank pointer at the new kill.
    pub fn kill_new(&mut self, text: Vec<u8>) {
        self.entries.push_front(text);
        self.entries.truncate(self.max);
        self.yank_pointer = 0;
        self.generation += 1;
    }

    /// Changes whenever the newest kill does.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn newest(&self) -> Option<&[u8]> {
        self.entries.front().map(Vec::as_slice)
    }

    /// Emacs `kill-append`: extend the newest kill (prepend when `before`),
    /// or start one when the ring is empty. The yank pointer is reset.
    pub fn kill_append(&mut self, text: &[u8], before: bool) {
        let Some(newest) = self.entries.front_mut() else {
            self.kill_new(text.to_vec());
            return;
        };
        if before {
            let mut combined = text.to_vec();
            combined.extend_from_slice(newest);
            *newest = combined;
        } else {
            newest.extend_from_slice(text);
        }
        self.yank_pointer = 0;
        self.generation += 1;
    }

    /// Where `kill-ring-yank-pointer` is: 0 for the newest kill.
    pub fn yank_pointer(&self) -> usize {
        self.yank_pointer
    }

    /// Emacs `(setq kill-ring-yank-pointer (nthcdr INDEX kill-ring))`.
    pub fn set_yank_pointer(&mut self, index: usize) {
        assert!(
            index < self.entries.len(),
            "the yank pointer points at a kill"
        );
        self.yank_pointer = index;
    }

    /// The index of the newest kill equal to `text` (`seq-position`).
    pub fn position(&self, text: &[u8]) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.as_slice() == text)
    }

    /// Emacs `current-kill`: rotate the yank pointer by `n` (wrapping, either
    /// direction) and return the kill it then points at.
    pub fn current_kill(&mut self, n: i64) -> Result<&[u8], KillRingError> {
        if self.entries.is_empty() {
            return Err(KillRingError::Empty);
        }
        let length = i64::try_from(self.entries.len()).expect("kill ring length fits in i64");
        let pointer = i64::try_from(self.yank_pointer).expect("yank pointer fits in i64");
        let index = (pointer + n.rem_euclid(length)).rem_euclid(length);
        self.yank_pointer = usize::try_from(index).expect("rem_euclid is non-negative");
        Ok(&self.entries[self.yank_pointer])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values from Emacs 29.3 `kill-new`, `kill-append` and
    // `current-kill` run in batch.

    fn ring(max: usize, kills: &[&str]) -> KillRing {
        let mut ring = KillRing::new(max);
        for kill in kills {
            ring.kill_new(kill.as_bytes().to_vec());
        }
        ring
    }

    fn contents(ring: &KillRing) -> Vec<&str> {
        ring.entries()
            .map(|entry| std::str::from_utf8(entry).expect("utf-8 test data"))
            .collect()
    }

    #[test]
    fn kill_new_pushes_newest_first_and_truncates() {
        let mut ring = ring(2, &["1", "2", "3"]);
        assert_eq!(contents(&ring), ["3", "2"]);
        ring.kill_new(Vec::new());
        assert_eq!(contents(&ring), ["", "3"]);
    }

    #[test]
    fn current_kill_rotates_and_wraps() {
        let mut ring = ring(120, &["a", "b", "c"]);
        assert_eq!(ring.current_kill(1), Ok(&b"b"[..]));
        assert_eq!(ring.current_kill(1), Ok(&b"a"[..]));
        assert_eq!(ring.current_kill(1), Ok(&b"c"[..]));
        assert_eq!(ring.current_kill(-1), Ok(&b"a"[..]));
        assert_eq!(ring.current_kill(0), Ok(&b"a"[..]));
        assert_eq!(ring.current_kill(-7), Ok(&b"b"[..]));
    }

    #[test]
    fn kill_append_extends_newest_and_resets_pointer() {
        let mut ring = ring(120, &["a", "b", "c"]);
        ring.current_kill(1).expect("non-empty");
        ring.kill_append(b"X", false);
        assert_eq!(contents(&ring), ["cX", "b", "a"]);
        assert_eq!(ring.current_kill(0), Ok(&b"cX"[..]));
        ring.kill_append(b"Y", true);
        assert_eq!(contents(&ring), ["YcX", "b", "a"]);
    }

    #[test]
    fn kill_append_on_empty_ring_starts_a_kill() {
        let mut ring = KillRing::new(120);
        ring.kill_append(b"Z", false);
        assert_eq!(contents(&ring), ["Z"]);
    }

    #[test]
    fn current_kill_on_empty_ring_errors() {
        let mut ring = KillRing::new(120);
        let error = ring.current_kill(0).expect_err("empty");
        assert_eq!(error.to_string(), "Kill ring is empty");
    }
}
