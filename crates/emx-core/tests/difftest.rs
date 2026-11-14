//! Differential tests: every case in `difftest/cases/*.jsonl` runs through
//! GNU Emacs (`difftest/emx-difftest.el`) and through the headless harness,
//! and the outcomes must match. Tier-1 cases must all pass.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use emx_core::testing::Harness;
use emx_types::parse_kbd;
use serde::Deserialize;

/// Cases that must match Emacs exactly (DESIGN 10).
const REQUIRED_TIER: u8 = 1;
const EMACS_ENV: &str = "EMACS";
const DEFAULT_EMACS: &str = "emacs";
/// The driver reports this many of the newest kills.
const KILL_RING_COMPARED: usize = 3;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    text: String,
    keys: String,
    tier: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Outcome {
    text: String,
    point: u64,
    mark: Option<u64>,
    mark_active: bool,
    kill_ring: Vec<String>,
    modified: bool,
    /// The error that ended the keyboard macro.
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmacsResult {
    name: String,
    #[serde(flatten)]
    outcome: Outcome,
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn emacs_binary() -> String {
    std::env::var(EMACS_ENV).unwrap_or_else(|_| DEFAULT_EMACS.to_string())
}

fn case_files(root: &Path) -> Vec<PathBuf> {
    let directory = root.join("difftest/cases");
    let mut files: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
        .map(|entry| entry.expect("directory entry is readable").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no cases in {}", directory.display());
    files
}

fn read_cases(file: &Path) -> Vec<Case> {
    let content = fs::read_to_string(file)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", file.display()));
    content
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("{}:{}: invalid case: {error}", file.display(), index + 1)
            })
        })
        .collect()
}

/// Runs every case in `file` through one Emacs process.
fn run_emacs(root: &Path, file: &Path) -> Vec<EmacsResult> {
    let emacs = emacs_binary();
    let directory = tempfile::tempdir().expect("temporary directory");
    let out = directory.path().join("emacs-results.jsonl");
    let output = Command::new(&emacs)
        .arg("--batch")
        .arg("-Q")
        .arg("-l")
        .arg(root.join("difftest/emx-difftest.el"))
        .arg("--cases")
        .arg(file)
        .arg("--out")
        .arg(&out)
        // Anything that prompts must fail instead of waiting for input.
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "the differential tests need GNU Emacs 29: could not run `{emacs}` \
                 ({error}); install Emacs or set {EMACS_ENV} to its path"
            )
        });
    assert!(
        output.status.success(),
        "Emacs failed on {}: {}\n{}",
        file.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let content = fs::read_to_string(&out).expect("Emacs wrote its results");
    content
        .lines()
        .map(|line| serde_json::from_str(line).expect("Emacs results are valid JSON"))
        .collect()
}

/// Feeds the keys one at a time; like a keyboard macro in Emacs, the run
/// stops at the first command that signals an error.
fn run_emx(case: &Case) -> Outcome {
    let mut harness = Harness::with_text(&case.text);
    let keys = parse_kbd(&case.keys)
        .unwrap_or_else(|error| panic!("{}: invalid keys {:?}: {error}", case.name, case.keys));
    let mut error = None;
    for key in keys {
        harness.key(key);
        if harness.signaled() {
            error = Some(harness.echo());
            break;
        }
    }
    Outcome {
        text: harness.text(),
        point: harness.point(),
        mark: harness.mark(),
        mark_active: harness.mark_active(),
        kill_ring: harness
            .kill_ring()
            .into_iter()
            .take(KILL_RING_COMPARED)
            .collect(),
        modified: harness.buffer().is_modified(),
        error,
    }
}

#[test]
fn emx_matches_emacs() {
    let root = repository_root();
    let mut mismatches = Vec::new();
    let mut required_total = 0;
    let mut required_passed = 0;
    let mut other_total = 0;
    let mut other_passed = 0;
    for file in case_files(&root) {
        let cases = read_cases(&file);
        let results = run_emacs(&root, &file);
        assert_eq!(
            cases.len(),
            results.len(),
            "Emacs returned a result for every case in {}",
            file.display()
        );
        for (case, expected) in cases.iter().zip(results) {
            assert_eq!(case.name, expected.name, "results are in case order");
            let actual = run_emx(case);
            let passed = actual == expected.outcome;
            if case.tier == REQUIRED_TIER {
                required_total += 1;
                required_passed += usize::from(passed);
            } else {
                other_total += 1;
                other_passed += usize::from(passed);
            }
            if !passed {
                mismatches.push(format!(
                    "MISMATCH {} (tier {})\n  text:  {:?}\n  keys:  {}\n  emacs: {:?}\n  emx:   {:?}",
                    case.name, case.tier, case.text, case.keys, expected.outcome, actual
                ));
            }
        }
    }
    for mismatch in &mismatches {
        println!("{mismatch}");
    }
    println!("tier 1: {required_passed}/{required_total} passed");
    if other_total > 0 {
        println!("other tiers: {other_passed}/{other_total} passed");
    }
    assert!(
        required_passed == required_total,
        "{} tier-1 case(s) differ from Emacs; see the mismatches above",
        required_total - required_passed
    );
}
