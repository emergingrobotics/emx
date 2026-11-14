//! The in-place save planner over synthetic piece layouts: original pieces
//! that shift left are moved first in ascending order, then pieces that
//! shift right in descending order, then inserted text is written.

use std::sync::Arc;

use emx_core::inplace::{Move, plan_in_place};
use emx_text::{Sources, Text};

const ORIGINAL_LEN: u64 = 100;

fn original() -> Text {
    let content: Vec<u8> = (0..ORIGINAL_LEN as u8).collect();
    Text::from_original(Sources::memory(Arc::from(content)))
}

fn insert(text: Text, at: u64, len: usize) -> Text {
    text.insert(at, &vec![b'+'; len]).expect("insert")
}

fn delete(text: Text, start: u64, end: u64) -> Text {
    text.delete(start..end).expect("delete")
}

#[test]
fn in_place_plan_orders_moves() {
    // Left shift: everything after a deletion moves down.
    let text = delete(original(), 10, 20);
    assert_eq!(
        plan_in_place(&text, ORIGINAL_LEN),
        vec![Move::Shift {
            from: 20,
            to: 10,
            len: 80
        }]
    );

    // Right shift: an insertion pushes the rest up, then is written.
    let text = insert(original(), 10, 3);
    assert_eq!(
        plan_in_place(&text, ORIGINAL_LEN),
        vec![
            Move::Shift {
                from: 10,
                to: 13,
                len: 90
            },
            Move::Write { at: 10, len: 3 },
        ]
    );

    // Two left shifts go in ascending order.
    let text = delete(delete(original(), 50, 60), 10, 15);
    assert_eq!(
        plan_in_place(&text, ORIGINAL_LEN),
        vec![
            Move::Shift {
                from: 15,
                to: 10,
                len: 35
            },
            Move::Shift {
                from: 60,
                to: 45,
                len: 40
            },
        ]
    );

    // Two right shifts go in descending order, then both inserts.
    let text = insert(insert(original(), 10, 5), 50, 5);
    assert_eq!(
        plan_in_place(&text, ORIGINAL_LEN),
        vec![
            Move::Shift {
                from: 45,
                to: 55,
                len: 55
            },
            Move::Shift {
                from: 10,
                to: 15,
                len: 35
            },
            Move::Write { at: 10, len: 5 },
            Move::Write { at: 50, len: 5 },
        ]
    );

    // Mixed: lefts ascending, then rights descending, then writes.
    let text = insert(delete(original(), 10, 30), 60, 30);
    assert_eq!(
        plan_in_place(&text, ORIGINAL_LEN),
        vec![
            Move::Shift {
                from: 30,
                to: 10,
                len: 50
            },
            Move::Shift {
                from: 80,
                to: 90,
                len: 20
            },
            Move::Write { at: 60, len: 30 },
        ]
    );

    // An overwrite of the same length moves nothing.
    let text = insert(delete(original(), 10, 13), 10, 3);
    assert_eq!(
        plan_in_place(&text, ORIGINAL_LEN),
        vec![Move::Write { at: 10, len: 3 }]
    );

    // A truncation or no change moves nothing; the length is set after.
    assert_eq!(
        plan_in_place(&delete(original(), 40, 100), ORIGINAL_LEN),
        vec![]
    );
    assert_eq!(plan_in_place(&original(), ORIGINAL_LEN), vec![]);
    assert_eq!(
        plan_in_place(&delete(original(), 0, 100), ORIGINAL_LEN),
        vec![]
    );
}

#[test]
fn adjacent_inserted_pieces_are_written_once() {
    let text = insert(insert(delete(original(), 10, 20), 10, 4), 14, 6);
    assert_eq!(
        plan_in_place(&text, ORIGINAL_LEN),
        vec![Move::Write { at: 10, len: 10 }]
    );
}

#[test]
#[should_panic(expected = "original pieces lie inside the")]
fn pieces_beyond_the_file_are_rejected() {
    plan_in_place(&original(), ORIGINAL_LEN - 1);
}
