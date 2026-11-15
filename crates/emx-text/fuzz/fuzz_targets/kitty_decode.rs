//! Fuzz of the Kitty keyboard-protocol decoder: arbitrary bytes are fed in
//! arbitrarily sized chunks so the decoder's incremental buffering of
//! split escape sequences and bracketed pastes is exercised. Terminal
//! input is external and must never panic the decoder or make it buffer
//! without bound.

#![no_main]

use libfuzzer_sys::fuzz_target;

use emx_term::{KittyDecoder, MAX_PENDING_SEQUENCE, PASTE_CHUNK_MAX};

/// The decoder holds at most a full escape sequence plus one undelivered
/// paste chunk and a partial end marker; nothing may exceed that.
const BUFFER_CEILING: usize = MAX_PENDING_SEQUENCE + PASTE_CHUNK_MAX + 8;

/// Largest chunk boundary used when splitting the input, keeping chunks
/// small so a sequence is routinely torn across feeds.
const MAX_CHUNK: usize = 16;

fuzz_target!(|data: &[u8]| {
    let mut decoder = KittyDecoder::new();
    let mut at = 0;
    while at < data.len() {
        let step = 1 + usize::from(data[at]) % MAX_CHUNK;
        let end = (at + step).min(data.len());
        let _events = decoder.feed(&data[at..end]);
        assert!(
            decoder.buffered_len() <= BUFFER_CEILING,
            "decoder buffered {} bytes, past the ceiling {BUFFER_CEILING}",
            decoder.buffered_len(),
        );
        at = end;
    }
});
