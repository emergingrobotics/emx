//! Fuzz of the Emacs-to-Rust regexp translator and the pattern compiler.
//! A search pattern is user input: translating and compiling an arbitrary
//! one must return an error, never panic, and a quoted literal must always
//! translate and compile back to a usable pattern.

#![no_main]

use libfuzzer_sys::fuzz_target;

use emx_text::search::{CompiledPattern, Pattern, emacs_regex_to_rust, regexp_quote};

fuzz_target!(|data: &[u8]| {
    // A literal needle of raw bytes is always a valid pattern to compile.
    let _ = CompiledPattern::compile(&Pattern::Literal {
        needle: data.to_vec(),
        case_fold: false,
    });

    let Ok(pattern) = std::str::from_utf8(data) else {
        return;
    };

    // Quoting turns any text into a regexp that matches it literally, so the
    // result must always translate and compile.
    let quoted = regexp_quote(pattern);
    let rust = emacs_regex_to_rust(&quoted).expect("a quoted literal always translates");
    CompiledPattern::compile(&Pattern::Regex {
        emacs: quoted,
        case_fold: false,
    })
    .unwrap_or_else(|error| panic!("a quoted literal always compiles ({rust:?}): {error}"));

    // An arbitrary pattern may be rejected; if it translates it must compile
    // under both case settings without panicking.
    if emacs_regex_to_rust(pattern).is_ok() {
        for case_fold in [false, true] {
            let _ = CompiledPattern::compile(&Pattern::Regex {
                emacs: pattern.to_string(),
                case_fold,
            });
        }
    }
});
