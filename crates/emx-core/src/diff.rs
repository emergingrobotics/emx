//! `diff-external` (DESIGN section 1): a unified diff between a buffer's
//! text before an external change and after it. Computed on a worker, since
//! diffing large texts takes long enough to stall the editor.

use std::path::PathBuf;

use emx_text::Text;

use crate::buffer::BufferId;
use crate::event::ClientId;
use crate::reload::read_for_diff;

/// Lines of unchanged context around each hunk, as `diff -u` prints.
pub const DIFF_CONTEXT_LINES: usize = 3;

/// Who asked for a diff: a terminal client running the command, or a
/// socket API request (client and request id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRequester {
    Command(ClientId),
    Request(ClientId, u64),
}

/// The newer side of a diff.
#[derive(Debug, Clone)]
pub enum DiffSide {
    Text(Text),
    /// The file as it is on disk now (a conflict: the buffer was not
    /// reloaded).
    Disk(PathBuf),
}

#[derive(Debug, Clone)]
pub struct DiffJob {
    pub buffer: BufferId,
    pub requester: DiffRequester,
    pub old_label: String,
    pub new_label: String,
    pub old: Text,
    pub new: DiffSide,
    /// Texts and files larger than this are refused.
    pub limit: u64,
}

impl PartialEq for DiffJob {
    fn eq(&self, other: &Self) -> bool {
        let same_new = match (&self.new, &other.new) {
            (DiffSide::Text(left), DiffSide::Text(right)) => left.same_root(right),
            (DiffSide::Disk(left), DiffSide::Disk(right)) => left == right,
            _ => false,
        };
        self.buffer == other.buffer
            && self.requester == other.requester
            && self.old_label == other.old_label
            && self.new_label == other.new_label
            && self.old.same_root(&other.old)
            && same_new
            && self.limit == other.limit
    }
}

impl Eq for DiffJob {}

fn read_text(text: &Text, label: &str, limit: u64) -> Result<Vec<u8>, String> {
    if text.len() > limit {
        return Err(format!("{label} is too large to diff (over {limit} bytes)"));
    }
    let mut bytes = Vec::new();
    text.read(0..text.len(), &mut bytes)
        .map_err(|error| format!("{label}: {error}"))?;
    Ok(bytes)
}

/// Unified diff of two byte strings, shown as text: invalid UTF-8 is
/// replaced, since the diff is for reading.
pub fn unified_diff(old_label: &str, new_label: &str, old: &[u8], new: &[u8]) -> String {
    let old = String::from_utf8_lossy(old);
    let new = String::from_utf8_lossy(new);
    similar::TextDiff::from_lines(old.as_ref(), new.as_ref())
        .unified_diff()
        .context_radius(DIFF_CONTEXT_LINES)
        .header(old_label, new_label)
        .to_string()
}

/// Carries out a `DiffJob`: reads both sides and diffs them.
pub fn run_diff(job: &DiffJob) -> Result<String, String> {
    let old = read_text(&job.old, &job.old_label, job.limit)?;
    let new = match &job.new {
        DiffSide::Text(text) => read_text(text, &job.new_label, job.limit)?,
        DiffSide::Disk(path) => read_for_diff(path, job.limit)?,
    };
    Ok(unified_diff(&job.old_label, &job.new_label, &old, &new))
}
