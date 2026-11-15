//! Syntax-aware editing of the bundled languages: highlighting, paren
//! matching, indentation and comments. Buffers in no language keep
//! Fundamental mode's behavior.

use std::fs;
use std::path::Path;

use emx_core::Config;
use emx_core::testing::Harness;
use emx_types::Face;

fn open(dir: &Path, name: &str, content: &str) -> Harness {
    fs::write(dir.join(name), content).unwrap();
    let mut harness = Harness::with_config(Config::default());
    harness.set_default_directory(dir);
    harness.keys("C-x C-f");
    harness.type_text(name);
    harness.keys("RET");
    harness
}

/// The face of each character of row `row`'s first `count` cells.
fn faces(harness: &Harness, row: u16, count: usize) -> Vec<Face> {
    harness.grid().row(row)[..count]
        .iter()
        .map(|cell| cell.face)
        .collect()
}

#[test]
fn rust_files_are_highlighted_and_named_in_the_modeline() {
    let directory = tempfile::tempdir().unwrap();
    let mut harness = open(directory.path(), "main.rs", "fn main() {} // hi\n");
    assert!(
        harness.screen()[22].contains("(Rust)"),
        "{:?}",
        harness.screen()[22]
    );
    let row = faces(&harness, 0, 18);
    assert_eq!(row[0..2], [Face::Keyword, Face::Keyword]);
    assert_eq!(row[3..7], [Face::Function; 4]);
    assert_eq!(row[13..18], [Face::Comment; 5]);
    // Typing at the start moves the faces with the text, and the new text
    // is highlighted.
    harness.type_text("pub ");
    let row = faces(&harness, 0, 6);
    assert_eq!(row[0..3], [Face::Keyword; 3]);
    assert_eq!(row[4..6], [Face::Keyword; 2]);
}

#[test]
fn a_script_is_known_by_its_shebang_and_text_files_stay_fundamental() {
    let directory = tempfile::tempdir().unwrap();
    let harness = open(directory.path(), "run", "#!/bin/sh\necho hi\n");
    assert!(
        harness.screen()[22].contains("(Bash)"),
        "{:?}",
        harness.screen()[22]
    );
    let harness = open(directory.path(), "notes.txt", "fn main() {}\n");
    assert!(harness.screen()[22].contains("(Fundamental)"));
    assert!(
        faces(&harness, 0, 12)
            .iter()
            .all(|face| *face == Face::Default)
    );
}

#[test]
fn the_paren_at_point_and_its_match_are_shown() {
    let directory = tempfile::tempdir().unwrap();
    let mut harness = open(directory.path(), "a.rs", "fn f(x: [u8; 2]) {}\n");
    harness.keys("C-a C-f C-f C-f C-f");
    // Point before `(`: it and the `)` at column 15 are shown.
    assert_eq!(harness.grid().row(0)[4].face, Face::ShowParenMatch);
    assert_eq!(harness.grid().row(0)[15].face, Face::ShowParenMatch);
    harness.keys("C-e");
    // After `}`: its `{`.
    assert_eq!(harness.grid().row(0)[17].face, Face::ShowParenMatch);
    assert_eq!(harness.grid().row(0)[18].face, Face::ShowParenMatch);
    // Fundamental mode shows matches too, as Emacs 28 and later do.
    let mut harness = open(directory.path(), "b.txt", "(a]\n");
    harness.keys("C-e");
    assert_eq!(harness.grid().row(0)[2].face, Face::ShowParenMismatch);
    assert_eq!(harness.grid().row(0)[0].face, Face::ShowParenMismatch);
}

#[test]
fn tab_and_return_indent_by_the_language() {
    let directory = tempfile::tempdir().unwrap();
    let mut harness = open(directory.path(), "a.rs", "fn f() {\nx\n}\n");
    harness.keys("C-n TAB");
    assert_eq!(harness.text(), "fn f() {\n    x\n}\n");
    assert_eq!(harness.point(), 13, "point goes after the indentation");
    harness.keys("C-e RET");
    harness.type_text("y");
    assert_eq!(harness.text(), "fn f() {\n    x\n    y\n}\n");
    harness.keys("C-n TAB");
    assert_eq!(
        harness.text(),
        "fn f() {\n    x\n    y\n}\n",
        "a closer dedents"
    );
    let mut harness = open(directory.path(), "a.py", "def f():\nreturn 1\n");
    harness.keys("C-n TAB");
    assert_eq!(harness.text(), "def f():\n    return 1\n");
    let mut harness = open(directory.path(), "a.go", "func f() {\nx\n}\n");
    harness.keys("C-n TAB");
    assert_eq!(
        harness.text(),
        "func f() {\n\tx\n}\n",
        "Go indents with tabs"
    );
}

#[test]
fn comment_dwim_adds_finds_and_toggles_comments() {
    let directory = tempfile::tempdir().unwrap();
    let mut harness = open(directory.path(), "a.rs", "let a = 1;\nlet b = 2;\n");
    harness.keys("M-;");
    assert_eq!(
        harness.text(),
        format!("let a = 1;{}// \nlet b = 2;\n", " ".repeat(22))
    );
    harness.type_text("one");
    harness.keys("C-a M-;");
    assert_eq!(harness.point(), 35, "M-; goes to the line's comment");
    let mut harness = open(directory.path(), "b.rs", "let a = 1;\n  let b = 2;\n");
    harness.keys("C-x h M-;");
    assert_eq!(harness.text(), "// let a = 1;\n//   let b = 2;\n");
    harness.keys("C-x h M-;");
    assert_eq!(harness.text(), "let a = 1;\n  let b = 2;\n", "and back");
    let mut harness = open(directory.path(), "c.txt", "plain\n");
    harness.keys("M-;");
    assert!(harness.echo().contains("No comment syntax is defined"));
}
