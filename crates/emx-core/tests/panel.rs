//! The file panel (DESIGN section 6): a side window over a tree of the
//! project, run against real directories.

use std::fs;
use std::path::Path;

use emx_core::testing::Harness;
use emx_core::{Config, Event};
use emx_types::{Face, Mods, MouseButton, MouseEvent, MouseKind};

/// A project: `.git` marks its root; `target/` and `*.log` are ignored.
fn project() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
    fs::create_dir_all(root.join("src/deep")).unwrap();
    fs::create_dir(root.join("target")).unwrap();
    fs::write(root.join("README"), "read me\n").unwrap();
    fs::write(root.join("build.log"), "").unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("src/deep/a.rs"), "").unwrap();
    directory
}

fn harness_in(dir: &Path, config: Config) -> Harness {
    let mut harness = Harness::with_config(config);
    harness.set_default_directory(dir);
    harness
}

/// The panel's text columns of each row, trimmed.
fn panel_rows(harness: &Harness) -> Vec<String> {
    harness
        .screen()
        .iter()
        .map(|row| {
            row.chars()
                .take(29)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn open_panel(harness: &mut Harness) {
    harness.keys("<f8>");
}

#[test]
fn f8_opens_a_panel_listing_the_project_root() {
    let project = project();
    let mut harness = harness_in(&project.path().join("src"), Config::default());
    open_panel(&mut harness);
    let rows = panel_rows(&harness);
    assert!(rows[0].ends_with('/'), "the root: {:?}", rows[0]);
    assert_eq!(
        rows[1..4],
        ["  + src/", "    .gitignore", "    README"],
        "ignored entries are hidden; directories come first"
    );
    let screen = harness.screen();
    assert_eq!(&screen[0][29..30], "|", "the panel's border");
    assert!(
        screen[22].starts_with("- "),
        "the panel's modeline: {:?}",
        screen[22]
    );
    assert!(
        screen[22][30..].starts_with("-UUU:"),
        "the window's modeline starts after the panel"
    );
    assert_eq!(harness.grid().row(1)[4].face, Face::PanelDirectory);
    assert_eq!(harness.buffer().name, " *tree F1*", "the panel is selected");
    assert_eq!(harness.watched_trees().len(), 1, "the root is watched");
    harness.keys("q");
    assert_eq!(harness.buffer().name, "*scratch*");
    assert!(harness.screen()[0].starts_with("  "), "the panel is gone");
    assert!(harness.watched_trees().is_empty());
}

#[test]
fn directories_open_and_close_and_files_open_in_the_main_window() {
    let project = project();
    let mut harness = harness_in(project.path(), Config::default());
    open_panel(&mut harness);
    harness.keys("n TAB");
    assert_eq!(
        panel_rows(&harness)[1..5],
        ["  - src/", "    + deep/", "      main.rs", "    .gitignore"]
    );
    assert_eq!(harness.watched_trees().len(), 2);
    harness.keys("n n RET");
    assert_eq!(harness.buffer().name, "main.rs");
    assert!(harness.screen()[0][30..].starts_with("fn main() {}"));
    harness.keys("C-x o");
    assert_eq!(
        harness.buffer().name,
        " *tree F1*",
        "C-x o reaches the panel"
    );
    harness.keys("p p TAB");
    assert_eq!(panel_rows(&harness)[1..3], ["  + src/", "    .gitignore"]);
    harness.keys("C-x o");
    assert_eq!(harness.buffer().name, "main.rs");
}

#[test]
fn ignored_entries_show_on_request() {
    let project = project();
    let mut harness = harness_in(project.path(), Config::default());
    open_panel(&mut harness);
    harness.keys("i");
    assert!(harness.echo().contains("Showing ignored files"));
    let rows = panel_rows(&harness);
    assert!(rows.contains(&"! + target/".to_string()), "{rows:?}");
    assert!(rows.contains(&"!   build.log".to_string()), "{rows:?}");
    assert_eq!(harness.grid().row(2)[4].face, Face::GitIgnored);
}

#[test]
fn files_are_created_renamed_and_deleted() {
    let project = project();
    let root = project.path();
    let mut harness = harness_in(root, Config::default());
    open_panel(&mut harness);
    harness.keys("+");
    harness.type_text("docs");
    harness.keys("RET");
    assert!(root.join("docs").is_dir());
    assert!(panel_rows(&harness).contains(&"  + docs/".to_string()));
    // README is the last line: rename it.
    harness.keys("M-> R C-a C-k");
    harness.type_text("NOTES");
    harness.keys("RET");
    assert!(root.join("NOTES").exists() && !root.join("README").exists());
    assert!(panel_rows(&harness).contains(&"    NOTES".to_string()));
    harness.keys("M-> d");
    let client = harness.client();
    assert!(
        harness
            .echo_line_of(client)
            .contains("Delete NOTES? (yes or no) ")
    );
    harness.type_text("yes");
    harness.keys("RET");
    assert!(!root.join("NOTES").exists());
    // A directory that is not empty is not deleted.
    harness.keys("M-< n n d");
    harness.type_text("yes");
    harness.keys("RET");
    assert!(root.join("src").exists());
    assert!(
        harness.echo().contains("Directory not empty"),
        "{}",
        harness.echo()
    );
}

#[test]
fn a_renamed_file_keeps_its_buffer() {
    let project = project();
    let root = project.path();
    let mut harness = harness_in(root, Config::default());
    open_panel(&mut harness);
    harness.keys("M-> RET");
    assert_eq!(harness.buffer().name, "README");
    harness.keys("C-x o M-> R C-a C-k");
    harness.type_text("README.md");
    harness.keys("RET C-x o");
    let file = harness.buffer().file.clone().expect("visits a file");
    assert_eq!(file.path, root.join("README.md"));
}

#[test]
fn window_commands_treat_the_panel_as_a_side_window() {
    let project = project();
    let mut harness = harness_in(project.path(), Config::default());
    open_panel(&mut harness);
    harness.keys("C-x 2");
    assert!(harness.echo().contains("Cannot split side window"));
    harness.keys("C-x 1");
    assert!(
        harness
            .echo()
            .contains("Cannot make side window the only window")
    );
    harness.keys("C-x b");
    harness.type_text("other");
    harness.keys("RET");
    assert_eq!(harness.buffer().name, "other", "shown in the main window");
    assert!(harness.screen()[0].starts_with("  "), "the panel stays");
    harness.keys("C-x o C-x 0");
    assert_eq!(harness.buffer().name, "other", "C-x 0 closed the panel");
    assert!(!harness.screen()[0].contains('|'));
}

#[test]
fn killing_the_panels_buffer_closes_it() {
    let project = project();
    let mut harness = harness_in(project.path(), Config::default());
    open_panel(&mut harness);
    harness.keys("C-x k");
    harness.type_text(" *tree F1*");
    harness.keys("RET");
    assert_eq!(harness.buffer().name, "*scratch*");
    assert!(!harness.screen()[0].contains('|'));
    open_panel(&mut harness);
    assert_eq!(harness.buffer().name, " *tree F1*", "it opens again");
}

#[test]
fn a_narrow_terminal_refuses_the_panel_and_a_narrowing_one_hides_it() {
    let project = project();
    let mut harness = harness_in(project.path(), Config::default());
    harness.resize(20, 24);
    open_panel(&mut harness);
    assert!(harness.echo().contains("too narrow"));
    harness.resize(80, 24);
    open_panel(&mut harness);
    harness.resize(12, 24);
    // Ten columns are left for the windows: the panel gets two, too few.
    assert!(!harness.screen()[0].contains('|'));
    harness.resize(80, 24);
    assert_eq!(&harness.screen()[0][29..30], "|", "it comes back");
}

#[test]
fn changed_directories_are_listed_again() {
    let project = project();
    let root = project.path();
    let mut harness = harness_in(root, Config::default());
    open_panel(&mut harness);
    fs::write(root.join("NEW"), "").unwrap();
    harness.handle(Event::TreeChanged {
        dir: root.to_path_buf(),
    });
    assert!(
        !panel_rows(&harness).contains(&"    NEW".to_string()),
        "not yet"
    );
    harness.fire_timers();
    assert!(panel_rows(&harness).contains(&"    NEW".to_string()));
}

fn mouse(harness: &mut Harness, kind: MouseKind, col: u16, row: u16) {
    let client = harness.client();
    harness.handle(Event::Mouse(
        client,
        MouseEvent {
            kind,
            button: MouseButton::Left,
            col,
            row,
            mods: Mods::default(),
            clicks: 1,
        },
    ));
}

#[test]
fn the_mouse_opens_entries_and_resizes_the_panel() {
    let project = project();
    let config = Config {
        mouse: true,
        ..Config::default()
    };
    let mut harness = harness_in(project.path(), config);
    open_panel(&mut harness);
    mouse(&mut harness, MouseKind::Press, 4, 1);
    assert_eq!(
        panel_rows(&harness)[1],
        "  - src/",
        "a click opens a directory"
    );
    // src/ now shows deep/ and main.rs; README is on row 5.
    mouse(&mut harness, MouseKind::Press, 4, 5);
    assert_eq!(harness.buffer().name, "README", "a click opens a file");
    mouse(&mut harness, MouseKind::Press, 29, 3);
    mouse(&mut harness, MouseKind::Drag, 19, 3);
    mouse(&mut harness, MouseKind::Release, 19, 3);
    assert_eq!(&harness.screen()[0][19..20], "|", "the border moved");
    harness.keys("<f8> <f8>");
    assert_eq!(
        &harness.screen()[0][19..20],
        "|",
        "the next panel opens as wide"
    );
}

#[test]
fn quick_open_finds_project_files_by_flex_matching() {
    let project = project();
    let mut harness = harness_in(project.path(), Config::default());
    harness.keys("C-x p f");
    let client = harness.client();
    assert!(
        harness
            .echo_line_of(client)
            .contains("Find file in project: "),
        "{:?}",
        harness.echo_line_of(client)
    );
    let candidates = harness.candidates();
    assert!(
        candidates.contains(&"src/deep/a.rs".to_string()),
        "{candidates:?}"
    );
    assert!(
        !candidates.iter().any(|name| name.ends_with(".log")),
        "ignored files are left out"
    );
    harness.type_text("smr");
    assert_eq!(harness.candidates(), vec!["src/main.rs".to_string()]);
    harness.keys("RET");
    assert_eq!(harness.buffer().name, "main.rs");
}

#[test]
fn git_status_marks_untracked_files_and_their_directories() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    gix::init(root).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/new.rs"), "").unwrap();
    let mut harness = harness_in(root, Config::default());
    open_panel(&mut harness);
    let rows = panel_rows(&harness);
    assert_eq!(rows[1], "? + src/", "{rows:?}");
    assert_eq!(harness.grid().row(1)[0].face, Face::GitUntracked);
}
