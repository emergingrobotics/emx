use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::source::ReadError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceSource {
    Original,
    Add,
}

/// A run of bytes from one source. `newlines` is `None` while the original
/// file region it covers has not been indexed yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Piece {
    pub source: PieceSource,
    pub start: u64,
    pub len: u64,
    pub newlines: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Summary {
    pub(crate) bytes: u64,
    pub(crate) newlines: Option<u64>,
    pub(crate) pieces: u64,
}

const EMPTY_SUMMARY: Summary = Summary {
    bytes: 0,
    newlines: Some(0),
    pieces: 0,
};

/// Treap node. Nodes are immutable once built; edits copy the path from the
/// root to the change, so every older `Text` keeps a valid tree.
#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) piece: Piece,
    priority: u64,
    pub(crate) left: Tree,
    pub(crate) right: Tree,
    pub(crate) summary: Summary,
}

pub(crate) type Tree = Option<Arc<Node>>;

static NODE_SERIAL: AtomicU64 = AtomicU64::new(0);

/// Hashing a serial instead of drawing random numbers keeps tree shapes
/// reproducible within a run, which makes failures replayable.
fn next_priority() -> u64 {
    let serial = NODE_SERIAL.fetch_add(1, Ordering::Relaxed);
    xxhash_rust::xxh3::xxh3_64(&serial.to_le_bytes())
}

pub(crate) fn summary(tree: &Tree) -> Summary {
    tree.as_ref().map_or(EMPTY_SUMMARY, |node| node.summary)
}

impl Node {
    fn build(piece: Piece, priority: u64, left: Tree, right: Tree) -> Arc<Node> {
        let left_summary = summary(&left);
        let right_summary = summary(&right);
        let newlines = match (
            left_summary.newlines,
            piece.newlines,
            right_summary.newlines,
        ) {
            (Some(left), Some(own), Some(right)) => Some(left + own + right),
            _ => None,
        };
        Arc::new(Node {
            piece,
            priority,
            left,
            right,
            summary: Summary {
                bytes: left_summary.bytes + piece.len + right_summary.bytes,
                newlines,
                pieces: left_summary.pieces + 1 + right_summary.pieces,
            },
        })
    }
}

pub(crate) fn leaf(piece: Piece) -> Tree {
    assert!(piece.len > 0, "pieces are never empty");
    Some(Node::build(piece, next_priority(), None, None))
}

/// Concatenates two trees; every byte of `left` precedes every byte of `right`.
pub(crate) fn merge(left: &Tree, right: &Tree) -> Tree {
    match (left, right) {
        (None, _) => right.clone(),
        (_, None) => left.clone(),
        (Some(left_node), Some(right_node)) => {
            if left_node.priority > right_node.priority {
                Some(Node::build(
                    left_node.piece,
                    left_node.priority,
                    left_node.left.clone(),
                    merge(&left_node.right, right),
                ))
            } else {
                Some(Node::build(
                    right_node.piece,
                    right_node.priority,
                    merge(left, &right_node.left),
                    right_node.right.clone(),
                ))
            }
        }
    }
}

/// Counts the newlines of a piece cut out of a larger one.
pub(crate) type NewlineCounter<'counter> =
    dyn FnMut(&Piece) -> Result<Option<u64>, ReadError> + 'counter;

/// Splits into bytes `[0, at)` and `[at, len)`, cutting a piece when `at`
/// falls inside it. The two halves of a cut piece become fresh leaves joined
/// at the top level: reusing the cut node's priority for both would tie them,
/// and repeated cuts of one piece would then pile up into a chain.
pub(crate) fn split(
    tree: &Tree,
    at: u64,
    count: &mut NewlineCounter<'_>,
) -> Result<(Tree, Tree), ReadError> {
    let (before, cut, after) = split_around(tree, at);
    match cut {
        None => Ok((before, after)),
        Some((piece, offset)) => {
            let (head, tail) = split_piece(&piece, offset, count)?;
            Ok((merge(&before, &leaf(head)), merge(&leaf(tail), &after)))
        }
    }
}

/// Structural split at `at`. When `at` falls strictly inside a piece, that
/// piece's node is removed and returned with the offset into it; `before` and
/// `after` then hold the bytes on either side of the piece.
fn split_around(tree: &Tree, at: u64) -> (Tree, Option<(Piece, u64)>, Tree) {
    let Some(node) = tree else {
        return (None, None, None);
    };
    if at == 0 {
        return (None, None, tree.clone());
    }
    if at >= node.summary.bytes {
        return (tree.clone(), None, None);
    }
    let left_bytes = summary(&node.left).bytes;
    let piece_end = left_bytes + node.piece.len;
    if at <= left_bytes {
        let (outer, cut, inner) = split_around(&node.left, at);
        let rebuilt = Node::build(node.piece, node.priority, inner, node.right.clone());
        (outer, cut, Some(rebuilt))
    } else if at >= piece_end {
        let (inner, cut, outer) = split_around(&node.right, at - piece_end);
        let rebuilt = Node::build(node.piece, node.priority, node.left.clone(), inner);
        (Some(rebuilt), cut, outer)
    } else {
        (
            node.left.clone(),
            Some((node.piece, at - left_bytes)),
            node.right.clone(),
        )
    }
}

fn split_piece(
    piece: &Piece,
    offset: u64,
    count: &mut NewlineCounter<'_>,
) -> Result<(Piece, Piece), ReadError> {
    let mut head = Piece {
        len: offset,
        newlines: None,
        ..*piece
    };
    let mut tail = Piece {
        start: piece.start + offset,
        len: piece.len - offset,
        newlines: None,
        ..*piece
    };
    head.newlines = count(&head)?;
    tail.newlines = match (piece.newlines, head.newlines) {
        (Some(whole), Some(head_count)) => Some(whole - head_count),
        _ => count(&tail)?,
    };
    Ok((head, tail))
}

pub(crate) fn last_piece(tree: &Tree) -> Option<Piece> {
    let mut node = tree.as_ref()?;
    while let Some(right) = &node.right {
        node = right;
    }
    Some(node.piece)
}

/// Replaces the last piece, copying only the right spine.
pub(crate) fn replace_last_piece(tree: &Tree, piece: Piece) -> Tree {
    let node = tree
        .as_ref()
        .expect("replace_last_piece needs a non-empty tree");
    let rebuilt = match &node.right {
        Some(_) => Node::build(
            node.piece,
            node.priority,
            node.left.clone(),
            replace_last_piece(&node.right, piece),
        ),
        None => Node::build(piece, node.priority, node.left.clone(), None),
    };
    Some(rebuilt)
}

/// Fills unknown piece counts, reusing every subtree whose count is known.
pub(crate) fn refresh(tree: &Tree, count: &mut NewlineCounter<'_>) -> Result<Tree, ReadError> {
    let Some(node) = tree else {
        return Ok(None);
    };
    if node.summary.newlines.is_some() {
        return Ok(tree.clone());
    }
    let mut piece = node.piece;
    if piece.newlines.is_none() {
        piece.newlines = count(&piece)?;
    }
    let left = refresh(&node.left, count)?;
    let right = refresh(&node.right, count)?;
    Ok(Some(Node::build(piece, node.priority, left, right)))
}

/// Longest root-to-leaf path, in nodes.
#[cfg(test)]
pub(crate) fn depth(tree: &Tree) -> u64 {
    tree.as_ref()
        .map_or(0, |node| 1 + depth(&node.left).max(depth(&node.right)))
}

/// In-order iterator over `(buffer offset, piece)`.
pub(crate) struct Pieces {
    stack: Vec<(Arc<Node>, u64)>,
}

impl Pieces {
    /// Starts at the piece containing byte `position`.
    pub(crate) fn starting_at(tree: &Tree, position: u64) -> Self {
        let mut stack = Vec::new();
        let mut current = tree.clone();
        let mut base = 0;
        while let Some(node) = current {
            let piece_offset = base + summary(&node.left).bytes;
            if position < piece_offset {
                current = node.left.clone();
                stack.push((node, piece_offset));
            } else if position < piece_offset + node.piece.len {
                stack.push((node, piece_offset));
                break;
            } else {
                base = piece_offset + node.piece.len;
                current = node.right.clone();
            }
        }
        Self { stack }
    }

    fn push_left_spine(&mut self, tree: &Tree, base: u64) {
        let mut current = tree.clone();
        while let Some(node) = current {
            let piece_offset = base + summary(&node.left).bytes;
            current = node.left.clone();
            self.stack.push((node, piece_offset));
        }
    }
}

impl Iterator for Pieces {
    type Item = (u64, Piece);

    fn next(&mut self) -> Option<Self::Item> {
        let (node, offset) = self.stack.pop()?;
        self.push_left_spine(&node.right, offset + node.piece.len);
        Some((offset, node.piece))
    }
}

/// Reverse in-order iterator over `(buffer offset, piece)`.
pub(crate) struct PiecesBackward {
    stack: Vec<(Arc<Node>, u64)>,
}

impl PiecesBackward {
    /// Starts at the last piece beginning before `position`.
    pub(crate) fn before(tree: &Tree, position: u64) -> Self {
        let mut stack = Vec::new();
        let mut current = tree.clone();
        let mut base = 0;
        while let Some(node) = current {
            let piece_offset = base + summary(&node.left).bytes;
            if position <= piece_offset {
                current = node.left.clone();
            } else if position <= piece_offset + node.piece.len {
                stack.push((node, piece_offset));
                break;
            } else {
                base = piece_offset + node.piece.len;
                current = node.right.clone();
                stack.push((node, piece_offset));
            }
        }
        Self { stack }
    }

    fn push_right_spine(&mut self, tree: &Tree, base: u64) {
        let mut current = tree.clone();
        let mut base = base;
        while let Some(node) = current {
            let piece_offset = base + summary(&node.left).bytes;
            base = piece_offset + node.piece.len;
            current = node.right.clone();
            self.stack.push((node, piece_offset));
        }
    }
}

impl Iterator for PiecesBackward {
    type Item = (u64, Piece);

    fn next(&mut self) -> Option<Self::Item> {
        let (node, offset) = self.stack.pop()?;
        let left_base = offset - summary(&node.left).bytes;
        self.push_right_spine(&node.left, left_base);
        Some((offset, node.piece))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_piece(start: u64, len: u64) -> Piece {
        Piece {
            source: PieceSource::Add,
            start,
            len,
            newlines: Some(0),
        }
    }

    fn check_heap(tree: &Tree) {
        if let Some(node) = tree {
            for child in [&node.left, &node.right].into_iter().flatten() {
                assert!(child.priority <= node.priority);
            }
            check_heap(&node.left);
            check_heap(&node.right);
        }
    }

    #[test]
    fn merge_keeps_heap_order_and_summary() {
        let mut tree = None;
        for index in 0..200 {
            tree = merge(&tree, &leaf(add_piece(index * 3, 3)));
        }
        check_heap(&tree);
        assert_eq!(summary(&tree).bytes, 600);
        assert_eq!(summary(&tree).pieces, 200);
        let offsets: Vec<u64> = Pieces::starting_at(&tree, 0)
            .map(|(offset, _)| offset)
            .collect();
        assert_eq!(offsets, (0..200).map(|index| index * 3).collect::<Vec<_>>());
        let backward: Vec<u64> = PiecesBackward::before(&tree, 301)
            .map(|(offset, _)| offset)
            .collect();
        assert_eq!(
            backward,
            (0..=100).rev().map(|index| index * 3).collect::<Vec<_>>()
        );
    }

    #[test]
    fn repeated_cuts_keep_heap_order() {
        let mut tree = None;
        for index in 0..64 {
            tree = merge(&tree, &leaf(add_piece(index * 7, 7)));
        }
        for step in 0..500u64 {
            let total = summary(&tree).bytes;
            let at = xxhash_rust::xxh3::xxh3_64(&step.to_le_bytes()) % (total + 1);
            let (head, tail) = split(&tree, at, &mut |_| Ok(Some(0))).expect("split");
            check_heap(&head);
            check_heap(&tail);
            tree = merge(&merge(&head, &leaf(add_piece(1000 + step, 1))), &tail);
            check_heap(&tree);
            assert_eq!(summary(&tree).bytes, total + 1);
        }
    }

    #[test]
    fn split_inside_piece_keeps_old_tree() {
        let tree = merge(&leaf(add_piece(0, 10)), &leaf(add_piece(10, 10)));
        let (head, tail) = split(&tree, 15, &mut |_| Ok(Some(0))).expect("split");
        assert_eq!(summary(&head).bytes, 15);
        assert_eq!(summary(&tail).bytes, 5);
        assert_eq!(summary(&tree).bytes, 20);
        check_heap(&head);
        check_heap(&tail);
    }
}
