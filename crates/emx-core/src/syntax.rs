//! Syntax-aware editing for the bundled languages (DESIGN section 7):
//! which language a file is in, tree-sitter highlighting run off the
//! editor thread, and the per-language rules indentation and comments use.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;

use emx_text::Text;
use emx_types::Face;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

use crate::buffer::BufferId;
use crate::event::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    Rust,
    C,
    Go,
    Python,
    JavaScript,
    Bash,
    Json,
    Toml,
    Yaml,
}

const SHEBANG: &[u8] = b"#!";
/// File names that say their language without an extension.
const NAMED_FILES: [(&str, Language); 6] = [
    (".bashrc", Language::Bash),
    (".bash_profile", Language::Bash),
    (".bash_logout", Language::Bash),
    (".profile", Language::Bash),
    ("Cargo.lock", Language::Toml),
    ("Pipfile", Language::Toml),
];

impl Language {
    pub const ALL: [Language; 9] = [
        Language::Rust,
        Language::C,
        Language::Go,
        Language::Python,
        Language::JavaScript,
        Language::Bash,
        Language::Json,
        Language::Toml,
        Language::Yaml,
    ];

    /// The language a file name says, by its extension or whole name.
    pub fn from_path(path: &Path) -> Option<Language> {
        let name = path.file_name()?.to_str()?;
        if let Some((_, language)) = NAMED_FILES.iter().find(|(known, _)| *known == name) {
            return Some(*language);
        }
        let extension = path.extension()?.to_str()?;
        Some(match extension {
            "rs" => Language::Rust,
            "c" | "h" => Language::C,
            "go" => Language::Go,
            "py" | "pyw" | "pyi" => Language::Python,
            "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
            "sh" | "bash" => Language::Bash,
            "json" => Language::Json,
            "toml" => Language::Toml,
            "yaml" | "yml" => Language::Yaml,
            _ => return None,
        })
    }

    /// The language a script's `#!` line names (`#!/bin/sh`,
    /// `#!/usr/bin/env python3`).
    pub fn from_shebang(first_line: &[u8]) -> Option<Language> {
        let rest = first_line.strip_prefix(SHEBANG)?;
        let rest = std::str::from_utf8(rest).ok()?;
        let mut words = rest.split_whitespace();
        let mut program = words.next()?.rsplit('/').next()?;
        if program == "env" {
            program = words.find(|word| !word.starts_with('-'))?;
        }
        let program = program
            .trim_end_matches(|character: char| character.is_ascii_digit() || character == '.');
        Some(match program {
            "sh" | "bash" | "dash" | "ksh" => Language::Bash,
            "python" => Language::Python,
            "node" | "nodejs" => Language::JavaScript,
            _ => return None,
        })
    }

    /// The modeline's mode name, as the Emacs major mode would show it.
    pub fn mode_name(self) -> &'static str {
        match self {
            Language::Rust => "Rust",
            Language::C => "C",
            Language::Go => "Go",
            Language::Python => "Python",
            Language::JavaScript => "JavaScript",
            Language::Bash => "Bash",
            Language::Json => "JSON",
            Language::Toml => "TOML",
            Language::Yaml => "YAML",
        }
    }

    /// Columns per indentation level, and whether levels are tabs.
    pub fn indentation(self) -> (u64, bool) {
        match self {
            Language::Rust | Language::C | Language::Python => (4, false),
            Language::Go => (8, true),
            Language::JavaScript
            | Language::Bash
            | Language::Json
            | Language::Toml
            | Language::Yaml => (2, false),
        }
    }

    /// What starts a line comment (Emacs `comment-start`), if the language
    /// has one.
    pub fn comment_start(self) -> Option<&'static str> {
        match self {
            Language::Rust | Language::C | Language::Go | Language::JavaScript => Some("//"),
            Language::Python | Language::Bash | Language::Toml | Language::Yaml => Some("#"),
            Language::Json => None,
        }
    }

    /// A trailing character that opens a block in this language besides the
    /// brackets (Python's and YAML's `:`).
    pub fn opens_block_with_colon(self) -> bool {
        matches!(self, Language::Python | Language::Yaml)
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::C => tree_sitter_c::LANGUAGE.into(),
            Language::Go => tree_sitter_go::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::Bash => tree_sitter_bash::LANGUAGE.into(),
            Language::Json => tree_sitter_json::LANGUAGE.into(),
            Language::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Language::Yaml => tree_sitter_yaml::LANGUAGE.into(),
        }
    }

    fn highlights(self) -> &'static str {
        match self {
            Language::Rust => tree_sitter_rust::HIGHLIGHTS_QUERY,
            Language::C => tree_sitter_c::HIGHLIGHT_QUERY,
            Language::Go => tree_sitter_go::HIGHLIGHTS_QUERY,
            Language::Python => tree_sitter_python::HIGHLIGHTS_QUERY,
            Language::JavaScript => tree_sitter_javascript::HIGHLIGHT_QUERY,
            Language::Bash => tree_sitter_bash::HIGHLIGHT_QUERY,
            Language::Json => tree_sitter_json::HIGHLIGHTS_QUERY,
            Language::Toml => tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            Language::Yaml => tree_sitter_yaml::HIGHLIGHTS_QUERY,
        }
    }
}

/// The face a highlight capture is drawn in, after Emacs's font-lock
/// faces; `None` for captures drawn plain (operators, punctuation,
/// variables in general, as font-lock leaves them).
pub fn capture_face(name: &str) -> Option<Face> {
    let mut parts = name.split('.');
    let first = parts.next()?;
    let second = parts.next();
    Some(match (first, second) {
        ("comment", _) => Face::Comment,
        ("string", Some("special" | "escape" | "regex")) | ("escape", _) => Face::Constant,
        ("string" | "character", _) => Face::String,
        ("keyword", _) | ("conditional" | "repeat" | "exception" | "include", _) => Face::Keyword,
        ("type" | "constructor", _) => Face::Type,
        ("function", Some("builtin" | "macro")) => Face::Builtin,
        ("function" | "method", _) => Face::Function,
        ("constant" | "number" | "boolean" | "float", Some("builtin")) => Face::Builtin,
        ("constant" | "number" | "boolean" | "float", _) => Face::Constant,
        ("variable", Some("builtin")) => Face::Builtin,
        ("variable", Some("parameter")) => Face::Variable,
        ("attribute" | "preproc" | "preprocessor" | "label" | "tag", _) => Face::Preprocessor,
        ("property", _) if second.is_none() => Face::Variable,
        _ => return None,
    })
}

/// What a highlight request asks for: the faces of `range` of `text`,
/// which is the buffer's text at change `generation`.
#[derive(Debug, Clone)]
pub struct HighlightJob {
    pub buffer: BufferId,
    pub language: Language,
    pub generation: u64,
    pub text: Text,
    pub range: Range<u64>,
    /// Lines longer than this get no faces.
    pub max_line: u64,
}

impl PartialEq for HighlightJob {
    fn eq(&self, other: &HighlightJob) -> bool {
        (
            self.buffer,
            self.language,
            self.generation,
            &self.range,
            self.max_line,
        ) == (
            other.buffer,
            other.language,
            other.generation,
            &other.range,
            other.max_line,
        )
    }
}

impl Eq for HighlightJob {}

struct Grammar {
    parser: Parser,
    query: Query,
    /// The face of each of the query's captures.
    faces: Vec<Option<Face>>,
}

/// Parsers and compiled queries, made once per language and kept: a
/// highlighting thread owns one.
#[derive(Default)]
pub struct Highlighter {
    grammars: HashMap<Language, Grammar>,
}

impl std::fmt::Debug for Highlighter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Highlighter")
            .field("languages", &self.grammars.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Highlighter {
    pub fn new() -> Highlighter {
        Highlighter::default()
    }

    /// Parses the whole text and returns the faces of the job's range, as
    /// `Event::Highlighted`. A text that cannot be read or parsed gets no
    /// faces.
    pub fn run(&mut self, job: &HighlightJob) -> Event {
        let spans = self.spans(job).unwrap_or_default();
        Event::Highlighted {
            buffer: job.buffer,
            generation: job.generation,
            range: job.range.clone(),
            spans,
        }
    }

    fn grammar(&mut self, language: Language) -> Option<&mut Grammar> {
        match self.grammars.entry(language) {
            std::collections::hash_map::Entry::Occupied(entry) => Some(entry.into_mut()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let grammar = language.grammar();
                let mut parser = Parser::new();
                parser.set_language(&grammar).ok()?;
                let query = Query::new(&grammar, language.highlights()).ok()?;
                let faces = query
                    .capture_names()
                    .iter()
                    .map(|name| capture_face(name))
                    .collect();
                Some(entry.insert(Grammar {
                    parser,
                    query,
                    faces,
                }))
            }
        }
    }

    fn spans(&mut self, job: &HighlightJob) -> Option<Vec<(Range<u64>, Face)>> {
        let mut bytes = Vec::new();
        job.text.read(0..job.text.len(), &mut bytes).ok()?;
        let grammar = self.grammar(job.language)?;
        let tree = grammar.parser.parse(&bytes, None)?;
        let start = usize::try_from(job.range.start).ok()?.min(bytes.len());
        let end = usize::try_from(job.range.end).ok()?.min(bytes.len());
        // Later, narrower captures paint over the ones they nest in; a
        // node captured again keeps its first capture, as the query's
        // earlier patterns take precedence.
        let mut painted: Vec<Option<Face>> = vec![None; end - start];
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(start..end);
        let mut captures = cursor.captures(&grammar.query, tree.root_node(), bytes.as_slice());
        while let Some((found, index)) = captures.next() {
            let capture = found.captures()[*index];
            let node = capture.node;
            if !seen.insert((node.start_byte(), node.end_byte())) {
                continue;
            }
            let Some(face) = grammar.faces[capture.index as usize] else {
                continue;
            };
            let from = node.start_byte().max(start);
            let to = node.end_byte().min(end);
            for slot in painted
                .iter_mut()
                .take(to.saturating_sub(start))
                .skip(from.saturating_sub(start))
            {
                *slot = Some(face);
            }
        }
        clear_long_lines(&bytes[start..end], &mut painted, job.max_line);
        Some(runs(&painted, job.range.start))
    }
}

/// No faces on lines longer than `max_line` (DESIGN section 5).
fn clear_long_lines(bytes: &[u8], painted: &mut [Option<Face>], max_line: u64) {
    let mut line_start = 0;
    for end in bytes
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .map(|(index, _)| index)
        .chain(std::iter::once(bytes.len()))
    {
        if (end - line_start) as u64 > max_line {
            for slot in &mut painted[line_start..end] {
                *slot = None;
            }
        }
        line_start = end + 1;
    }
}

/// Runs of one face, as spans from `base`.
fn runs(painted: &[Option<Face>], base: u64) -> Vec<(Range<u64>, Face)> {
    let mut spans: Vec<(Range<u64>, Face)> = Vec::new();
    for (offset, face) in painted.iter().enumerate() {
        let Some(face) = face else {
            continue;
        };
        let at = base + offset as u64;
        match spans.last_mut() {
            Some((range, last)) if range.end == at && last == face => range.end = at + 1,
            _ => spans.push((at..at + 1, *face)),
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use emx_text::Sources;
    use slotmap::KeyData;

    use super::*;

    fn job(language: Language, source: &str) -> HighlightJob {
        let text = Text::from_original(Sources::memory(Arc::from(source.as_bytes())));
        HighlightJob {
            buffer: BufferId::from(KeyData::from_ffi(1)),
            language,
            generation: 7,
            range: 0..text.len(),
            text,
            max_line: 10_000,
        }
    }

    fn faces_of(language: Language, source: &str) -> Vec<(String, Face)> {
        let job = job(language, source);
        let Event::Highlighted { spans, .. } = Highlighter::new().run(&job) else {
            panic!("a highlight result");
        };
        spans
            .into_iter()
            .map(|(range, face)| {
                (
                    source[range.start as usize..range.end as usize].to_string(),
                    face,
                )
            })
            .collect()
    }

    #[test]
    fn languages_come_from_names_and_shebangs() {
        let path = |name: &str| PathBuf::from(name);
        assert_eq!(
            Language::from_path(&path("src/main.rs")),
            Some(Language::Rust)
        );
        assert_eq!(Language::from_path(&path("a.h")), Some(Language::C));
        assert_eq!(Language::from_path(&path("x.yml")), Some(Language::Yaml));
        assert_eq!(
            Language::from_path(&path("Cargo.lock")),
            Some(Language::Toml)
        );
        assert_eq!(Language::from_path(&path(".bashrc")), Some(Language::Bash));
        assert_eq!(Language::from_path(&path("notes.txt")), None);
        assert_eq!(Language::from_path(&path("Makefile")), None);
        assert_eq!(Language::from_shebang(b"#!/bin/sh"), Some(Language::Bash));
        assert_eq!(
            Language::from_shebang(b"#!/usr/bin/env python3"),
            Some(Language::Python)
        );
        assert_eq!(
            Language::from_shebang(b"#!/usr/bin/env -S node --flag"),
            Some(Language::JavaScript)
        );
        assert_eq!(Language::from_shebang(b"#!/usr/bin/perl"), None);
        assert_eq!(Language::from_shebang(b"# not one"), None);
    }

    #[test]
    fn every_bundled_query_compiles() {
        let mut highlighter = Highlighter::new();
        for language in Language::ALL {
            assert!(highlighter.grammar(language).is_some(), "{language:?}");
        }
    }

    #[test]
    fn rust_is_highlighted_by_capture() {
        let faces = faces_of(Language::Rust, "// hi\nfn main() { let s = \"x\"; }\n");
        assert!(
            faces.contains(&("// hi".to_string(), Face::Comment)),
            "{faces:?}"
        );
        assert!(
            faces.contains(&("fn".to_string(), Face::Keyword)),
            "{faces:?}"
        );
        assert!(
            faces.contains(&("main".to_string(), Face::Function)),
            "{faces:?}"
        );
        assert!(
            faces.contains(&("\"x\"".to_string(), Face::String)),
            "{faces:?}"
        );
    }

    #[test]
    fn python_and_json_too() {
        let faces = faces_of(Language::Python, "def f(x):\n    return 'a'  # c\n");
        assert!(
            faces.contains(&("def".to_string(), Face::Keyword)),
            "{faces:?}"
        );
        assert!(
            faces.contains(&("# c".to_string(), Face::Comment)),
            "{faces:?}"
        );
        let faces = faces_of(Language::Json, "{\"k\": 1}");
        assert!(
            faces.iter().any(|(_, face)| *face == Face::Constant),
            "{faces:?}"
        );
    }

    #[test]
    fn long_lines_get_no_faces() {
        let mut job = job(Language::Rust, "fn a() {}\nfn b() {}\n");
        job.max_line = 5;
        let Event::Highlighted { spans, .. } = Highlighter::new().run(&job) else {
            panic!("a highlight result");
        };
        assert!(spans.is_empty(), "{spans:?}");
    }

    #[test]
    fn only_the_asked_range_is_returned() {
        let source = "fn a() {}\nfn b() {}\n";
        let mut job = job(Language::Rust, source);
        job.range = 10..20;
        let Event::Highlighted { spans, range, .. } = Highlighter::new().run(&job) else {
            panic!("a highlight result");
        };
        assert_eq!(range, 10..20);
        assert!(
            spans
                .iter()
                .all(|(span, _)| span.start >= 10 && span.end <= 20)
        );
        assert!(!spans.is_empty());
    }
}
