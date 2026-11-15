//! Differential fuzz of the piece tree: a program of insert and delete
//! operations is applied to both a `Text` and a plain `Vec<u8>` reference
//! model, and the two must agree in length and content after every run.
//! A divergence is a piece-tree defect and shows as a panic the fuzzer
//! records.

#![no_main]

use libfuzzer_sys::fuzz_target;

use emx_text::{Sources, Text};

/// The most bytes a single insert adds, so one operation stays small and a
/// short input still exercises many operations.
const MAX_INSERT: usize = 8;

/// Reads a byte program left to right, handing out the fields each
/// operation needs.
struct Program<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Program<'a> {
    fn new(bytes: &'a [u8]) -> Program<'a> {
        Program { bytes, at: 0 }
    }

    fn next_u8(&mut self) -> Option<u8> {
        let byte = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(byte)
    }

    fn next_u16(&mut self) -> Option<u16> {
        let high = u16::from(self.next_u8()?);
        let low = u16::from(self.next_u8()?);
        Some((high << 8) | low)
    }

    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(count)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }
}

fn run(data: &[u8]) -> Result<(), String> {
    let mut program = Program::new(data);
    let mut model: Vec<u8> = Vec::new();
    let mut text = Text::from_original(Sources::memory(Vec::new().into()));

    while let Some(control) = program.next_u8() {
        let length = model.len();
        if control & 1 == 0 {
            let Some(span) = program.next_u16() else { break };
            let at = usize::from(span) % (length + 1);
            let count = 1 + usize::from(control >> 1) % MAX_INSERT;
            let Some(payload) = program.take(count) else {
                break;
            };
            text = text
                .insert(at as u64, payload)
                .map_err(|error| format!("insert: {error}"))?;
            model.splice(at..at, payload.iter().copied());
        } else {
            if length == 0 {
                continue;
            }
            let Some(span) = program.next_u16() else { break };
            let Some(width) = program.next_u8() else { break };
            let at = usize::from(span) % length;
            let count = usize::from(width) % (length - at) + 1;
            text = text
                .delete(at as u64..(at + count) as u64)
                .map_err(|error| format!("delete: {error}"))?;
            model.drain(at..at + count);
        }

        assert_eq!(
            text.len(),
            model.len() as u64,
            "length diverged from the model"
        );
    }

    let mut out = Vec::new();
    text.read(0..text.len(), &mut out)
        .map_err(|error| format!("read back: {error}"))?;
    assert_eq!(out, model, "content diverged from the model");
    Ok(())
}

fuzz_target!(|data: &[u8]| {
    // A `ReadError` here is a genuine failure to read memory the tree
    // itself owns, so surface it rather than swallow it.
    run(data).expect("piece tree stays consistent with the model");
});
