//! Fuzz of the edit-journal parser: arbitrary bytes are parsed the way
//! crash recovery parses a `.jnl` file, driving the frame scanner, the
//! record decoder and the edit-application arithmetic. A journal left by a
//! crashed session is external input that must parse to an error, never a
//! panic.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = emx_core::journal::parse_for_fuzz(data);
});
