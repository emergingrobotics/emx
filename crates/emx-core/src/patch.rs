//! Unified diffs for `emx apply-patch` (DESIGN section 2): parsing one
//! file's diff and finding where its hunks go in a buffer's text. A hunk
//! goes where its old lines are found exactly, at the line it names or
//! the nearest place after the previous hunk; if any hunk is not found the
//! whole patch fails and nothing changes.

use std::ops::Range;

const HUNK_HEADER: &[u8] = b"@@ ";
const OLD_FILE: &[u8] = b"--- ";
const NEW_FILE: &[u8] = b"+++ ";
const NO_NEWLINE: &[u8] = b"\\ ";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    #[error("the patch has no hunks")]
    Empty,
    #[error("the patch changes more than one file")]
    ManyFiles,
    #[error("line {line} of the patch: {reason}")]
    Malformed { line: usize, reason: String },
    #[error("hunk {hunk} does not apply")]
    Mismatch { hunk: usize },
}

/// One hunk: the lines it expects and what replaces them, each with its
/// newline unless it is the last line of a file without one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The 1-based line the old lines start at (the line they follow when
    /// there are none).
    pub old_start: usize,
    pub old: Vec<Vec<u8>>,
    pub new: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Both,
    Old,
    New,
}

pub fn parse(patch: &[u8]) -> Result<Vec<Hunk>, PatchError> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut files = 0;
    let mut remaining = (0usize, 0usize);
    let mut last_side = None;
    let lines: Vec<&[u8]> = patch.split_inclusive(|byte| *byte == b'\n').collect();
    for (index, raw) in lines.iter().enumerate() {
        let number = index + 1;
        let malformed = |reason: &str| PatchError::Malformed {
            line: number,
            reason: reason.to_string(),
        };
        if remaining == (0, 0) {
            if raw.starts_with(OLD_FILE) {
                files += 1;
                if files > 1 {
                    return Err(PatchError::ManyFiles);
                }
                continue;
            }
            if raw.starts_with(NEW_FILE) {
                continue;
            }
            if raw.starts_with(HUNK_HEADER) {
                let (old_start, old_count, new_count) =
                    hunk_header(raw).ok_or_else(|| malformed("a malformed hunk header"))?;
                hunks.push(Hunk {
                    old_start,
                    old: Vec::new(),
                    new: Vec::new(),
                });
                remaining = (old_count, new_count);
                last_side = None;
                continue;
            }
            if raw.starts_with(NO_NEWLINE) {
                let hunk = hunks
                    .last_mut()
                    .ok_or_else(|| malformed("no line before it"))?;
                drop_newline(
                    hunk,
                    last_side.ok_or_else(|| malformed("no line before it"))?,
                );
                continue;
            }
            // Text around the diff (a commit message, `diff --git` and
            // `index` lines) is not part of any hunk.
            continue;
        }
        let Some((&marker, content)) = raw.split_first() else {
            return Err(malformed("an empty line in a hunk"));
        };
        let hunk = hunks.last_mut().expect("counts are set by a hunk header");
        let side = match marker {
            b' ' => Side::Both,
            b'-' => Side::Old,
            b'+' => Side::New,
            b'\\' => {
                drop_newline(
                    hunk,
                    last_side.ok_or_else(|| malformed("no line before it"))?,
                );
                continue;
            }
            // Some tools drop the space of an empty context line.
            b'\n' => Side::Both,
            _ => return Err(malformed("a line that is not context, removed or added")),
        };
        let content = if marker == b'\n' { &b"\n"[..] } else { content };
        if matches!(side, Side::Both | Side::Old) {
            remaining.0 = remaining
                .0
                .checked_sub(1)
                .ok_or_else(|| malformed("more old lines than the header says"))?;
            hunk.old.push(content.to_vec());
        }
        if matches!(side, Side::Both | Side::New) {
            remaining.1 = remaining
                .1
                .checked_sub(1)
                .ok_or_else(|| malformed("more new lines than the header says"))?;
            hunk.new.push(content.to_vec());
        }
        last_side = Some(side);
        if remaining == (0, 0) {
            // A `\ No newline` line may still follow.
            continue;
        }
    }
    if remaining != (0, 0) {
        return Err(PatchError::Malformed {
            line: lines.len(),
            reason: "the patch ends inside a hunk".to_string(),
        });
    }
    if hunks.is_empty() {
        return Err(PatchError::Empty);
    }
    Ok(hunks)
}

fn drop_newline(hunk: &mut Hunk, side: Side) {
    let strip = |lines: &mut Vec<Vec<u8>>| {
        if let Some(last) = lines.last_mut()
            && last.last() == Some(&b'\n')
        {
            last.pop();
        }
    };
    if matches!(side, Side::Both | Side::Old) {
        strip(&mut hunk.old);
    }
    if matches!(side, Side::Both | Side::New) {
        strip(&mut hunk.new);
    }
}

/// `@@ -a[,b] +c[,d] @@`: the old start, old count and new count.
fn hunk_header(line: &[u8]) -> Option<(usize, usize, usize)> {
    let text = std::str::from_utf8(line).ok()?;
    let mut words = text.split_whitespace();
    if words.next()? != "@@" {
        return None;
    }
    let range = |word: &str, sign: char| -> Option<(usize, usize)> {
        let rest = word.strip_prefix(sign)?;
        match rest.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((rest.parse().ok()?, 1)),
        }
    };
    let (old_start, old_count) = range(words.next()?, '-')?;
    let (_, new_count) = range(words.next()?, '+')?;
    (words.next()? == "@@").then_some((old_start, old_count, new_count))
}

/// A byte range of the text and what replaces it.
pub type Edit = (Range<usize>, Vec<u8>);

/// Where each hunk goes in `text`, as byte ranges and their new bytes, in
/// order and without overlap.
pub fn locate(text: &[u8], hunks: &[Hunk]) -> Result<Vec<Edit>, PatchError> {
    let mut starts: Vec<usize> = vec![0];
    for (index, byte) in text.iter().enumerate() {
        if *byte == b'\n' {
            starts.push(index + 1);
        }
    }
    // A text ending in a newline has no line after it.
    if starts.len() > 1 && starts.last() == Some(&text.len()) {
        starts.pop();
    }
    let line_count = if text.is_empty() { 0 } else { starts.len() };
    let line = |index: usize| -> &[u8] {
        let end = starts.get(index + 1).copied().unwrap_or(text.len());
        &text[starts[index]..end]
    };
    let offset = |index: usize| -> usize { starts.get(index).copied().unwrap_or(text.len()) };
    let mut edits = Vec::new();
    let mut earliest = 0usize;
    for (number, hunk) in hunks.iter().enumerate() {
        let size = hunk.old.len();
        let expected = if size == 0 {
            hunk.old_start
        } else {
            hunk.old_start.saturating_sub(1)
        };
        let fits = |at: usize| -> bool {
            at + size <= line_count && (0..size).all(|i| line(at + i) == hunk.old[i].as_slice())
        };
        let last = line_count.saturating_sub(size);
        let found = (0..=line_count)
            .flat_map(|distance| {
                let after = expected.checked_add(distance);
                let before = expected.checked_sub(distance).filter(|_| distance > 0);
                [after, before]
            })
            .flatten()
            .filter(|at| *at >= earliest && *at <= last)
            .find(|at| fits(*at))
            .ok_or(PatchError::Mismatch { hunk: number + 1 })?;
        let range = offset(found)..offset(found + size);
        edits.push((range, hunk.new.concat()));
        earliest = found + size;
    }
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, patch: &str) -> Result<String, PatchError> {
        let hunks = parse(patch.as_bytes())?;
        let mut bytes = text.as_bytes().to_vec();
        for (range, new) in locate(text.as_bytes(), &hunks)?.into_iter().rev() {
            bytes.splice(range, new);
        }
        Ok(String::from_utf8(bytes).unwrap())
    }

    const TEXT: &str = "one\ntwo\nthree\nfour\nfive\n";

    #[test]
    fn a_git_diff_applies() {
        let patch = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n\
                     @@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n\
                     @@ -4,2 +4,3 @@\n four\n five\n+six\n";
        assert_eq!(
            apply(TEXT, patch).unwrap(),
            "one\nTWO\nthree\nfour\nfive\nsix\n"
        );
    }

    #[test]
    fn hunks_find_their_lines_when_the_text_moved() {
        let moved = format!("zero\n{TEXT}");
        let patch = "@@ -2,1 +2,1 @@\n-two\n+2\n";
        assert_eq!(
            apply(&moved, patch).unwrap(),
            "zero\none\n2\nthree\nfour\nfive\n"
        );
    }

    #[test]
    fn a_hunk_that_is_not_found_fails_the_patch() {
        let patch = "@@ -1,1 +1,1 @@\n-one\n+1\n@@ -3,1 +3,1 @@\n-THREE\n+3\n";
        assert_eq!(apply(TEXT, patch), Err(PatchError::Mismatch { hunk: 2 }));
    }

    #[test]
    fn missing_final_newlines() {
        let patch = "@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n";
        assert_eq!(apply("a", patch).unwrap(), "b\n");
        let patch = "@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n";
        assert_eq!(apply("a\n", patch).unwrap(), "b");
    }

    #[test]
    fn insertions_into_an_empty_file_and_at_the_start() {
        assert_eq!(apply("", "@@ -0,0 +1,2 @@\n+x\n+y\n").unwrap(), "x\ny\n");
        assert_eq!(
            apply(TEXT, "@@ -0,0 +1 @@\n+zero\n").unwrap(),
            format!("zero\n{TEXT}")
        );
    }

    #[test]
    fn malformed_patches_are_refused() {
        assert_eq!(parse(b"just text\n"), Err(PatchError::Empty));
        assert!(matches!(
            parse(b"@@ -1,2 +1,2 @@\n one\n"),
            Err(PatchError::Malformed { .. })
        ));
        assert!(matches!(
            parse(b"@@ -1 +1 @@\n*x\n"),
            Err(PatchError::Malformed { .. })
        ));
        assert_eq!(
            parse(b"--- a\n+++ a\n@@ -1 +1 @@\n-x\n+y\n--- b\n+++ b\n"),
            Err(PatchError::ManyFiles)
        );
    }
}
