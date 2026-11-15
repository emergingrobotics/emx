//! Writes benchmark results to `bench/README.md` in the repository: a small
//! text file that stays in version control. The preamble (methodology and
//! the DESIGN 8 targets) is written once; each run appends a section with a
//! results table and a pass or fail against every measurable target.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::corpus::Size;
use crate::measure::{
    KEYPRESS_TARGET_MS, RELOAD_EXTRA_MS, RELOAD_TARGET_MS, RSS_OVERHEAD_TARGET_MIB, Report,
    SETTLE_WINDOW_MS,
};

/// The file this crate appends results to, under the repository's `bench/`.
fn readme_path() -> PathBuf {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = crate_dir
        .ancestors()
        .nth(2)
        .unwrap_or(crate_dir)
        .to_path_buf();
    repo_root.join("bench").join("README.md")
}

const PREAMBLE: &str = "\
# emx benchmarks

`emx-bench` measures `emx --no-server` against the DESIGN section 8 \
performance targets. It is run through the Makefile: `make bench` covers the \
10 MB, 100 MB single-line and 1 GB corpora; `make bench-full` adds the \
optional 10 GB corpus.

## Method

- Corpora are generated on local disk (`$EMX_BENCH_CORPUS_DIR`, else the \
system temporary directory), never under this repository: the repository is \
on a network filesystem where multi-gigabyte writes are slow and would \
measure the network rather than the editor. A corpus is written once and \
reused; each ends with a unique needle at EOF for the search benchmark.
- The editor is driven on a real pseudo-terminal, its screen parsed with \
`vt100`, keys sent with the Kitty flags-5 encoding. Every interaction is \
bounded by a deadline that kills the editor if it hangs.
- Keypress and scroll latency is the time from writing the key to the first \
byte of the paint that follows, over 200 `C-n` and 100 `C-v`. Keystrokes \
that paint nothing (the buffer end is already shown) are counted but left \
out of the statistics; the single-line corpus paints only on the first.
- Open-to-first-paint is measured cold (page cache dropped with \
`fadvise(DontNeed)`) and warm. Resident memory (`VmRSS` from \
`/proc/<pid>/status`) is read for the empty scratch buffer and, on the \
corpus, after opening and after scrolling.

## Targets (DESIGN 8) and what is measurable here

- Keypress-to-paint under 16 ms at all file sizes, measured locally at the \
PTY: measured, pass/fail recorded.
- First paint under 50 ms on a Raspberry Pi 4 with a warm cache: NOT \
measurable in this environment (x86-64, no Pi hardware). The x86-64 open \
times are recorded for reference only.
- Memory overhead for an unedited multi-gigabyte file in single-digit MB \
plus the block-cache budget (64 MiB): measured as peak corpus RSS minus \
scratch RSS, checked against a 73 MiB ceiling; most meaningful on the 1 GB \
and 10 GB corpora.
- External write to updated buffer within the settle window plus 100 ms \
(300 ms): measured, pass/fail recorded.
- Binary size: no numeric budget was fixed in DESIGN (\"to be set before \
M2\"); the static-build sizes are recorded in the static-builds section \
below rather than failed against a number.
- aarch64 runs: the aarch64 static binary is built but cannot be run here \
(no aarch64 hardware or emulator); these numbers are x86-64 only.

";

pub fn append(report: &Report) -> Result<(), String> {
    ensure_preamble()?;
    let mut section = String::new();
    write_section(&mut section, report);
    append_raw(&section)
}

pub fn append_raw(text: &str) -> Result<(), String> {
    let path = readme_path();
    ensure_preamble()?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("opening {}: {error}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|error| format!("writing {}: {error}", path.display()))
}

pub fn skip_note(size: Size, reason: &str) -> String {
    format!(
        "## Run {} — {} SKIPPED\n\n{} was skipped: {reason}.\n\n",
        utc_now(),
        size.label(),
        size.label(),
    )
}

fn ensure_preamble() -> Result<(), String> {
    let path = readme_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    if !path.exists() {
        fs::write(&path, PREAMBLE)
            .map_err(|error| format!("writing {}: {error}", path.display()))?;
    }
    Ok(())
}

fn write_section(out: &mut String, report: &Report) {
    let _ = writeln!(out, "## Run {} — {}", utc_now(), report.corpus_label);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "- corpus: `{}` ({} bytes{})",
        report.corpus_path.display(),
        report.corpus_bytes,
        if report.single_line {
            ", single line"
        } else {
            ""
        }
    );
    let _ = writeln!(out, "- editor: `{}`", report.emx.display());
    let _ = writeln!(out, "- host: x86-64 (aarch64 and Pi 4 not measurable here)");
    let _ = writeln!(out);
    let _ = writeln!(out, "| Metric | Measured | DESIGN 8 target | Result |");
    let _ = writeln!(out, "|---|---|---|---|");

    row(
        out,
        "startup-to-first-frame (scratch)",
        &ms(report.startup_first_frame_ms),
        "informational",
        "INFO",
    );
    row(
        out,
        "open-to-first-paint, cold cache",
        &ms(report.open_cold_ms),
        "<50 ms on Pi 4 warm",
        "NOT MEASURABLE HERE",
    );
    row(
        out,
        "open-to-first-paint, warm cache",
        &ms(report.open_warm_ms),
        "<50 ms on Pi 4 warm",
        "NOT MEASURABLE HERE",
    );
    row(
        out,
        &format!(
            "keypress-to-paint median (C-n, {}/{} painted)",
            report.keypress.painted, report.keypress.total
        ),
        &ms(report.keypress.median_ms),
        &format!("<{KEYPRESS_TARGET_MS:.0} ms"),
        &verdict_below(report.keypress.median_ms, KEYPRESS_TARGET_MS),
    );
    row(
        out,
        "keypress-to-paint p99 (C-n)",
        &ms(report.keypress.p99_ms),
        &format!("<{KEYPRESS_TARGET_MS:.0} ms"),
        &verdict_below(report.keypress.p99_ms, KEYPRESS_TARGET_MS),
    );
    row(
        out,
        &format!(
            "scroll median (C-v, {}/{} painted)",
            report.scroll.painted, report.scroll.total
        ),
        &ms(report.scroll.median_ms),
        &format!("<{KEYPRESS_TARGET_MS:.0} ms"),
        &verdict_below(report.scroll.median_ms, KEYPRESS_TARGET_MS),
    );
    row(
        out,
        "scroll p99 (C-v)",
        &ms(report.scroll.p99_ms),
        &format!("<{KEYPRESS_TARGET_MS:.0} ms"),
        &verdict_below(report.scroll.p99_ms, KEYPRESS_TARGET_MS),
    );
    row(
        out,
        "search to EOF needle (C-s)",
        &search(report),
        "informational",
        "INFO",
    );
    row(
        out,
        "reload latency (external append to frame)",
        &ms(report.reload_ms),
        &format!("<={RELOAD_TARGET_MS:.0} ms (settle {SETTLE_WINDOW_MS} + {RELOAD_EXTRA_MS})"),
        &verdict_at_or_below(report.reload_ms, RELOAD_TARGET_MS),
    );
    row(
        out,
        "RSS, scratch buffer",
        &mib_kib(report.scratch_rss_kib),
        "informational",
        "INFO",
    );
    row(
        out,
        "RSS, after open",
        &mib_kib(report.open_rss_kib),
        "informational",
        "INFO",
    );
    row(
        out,
        "RSS, peak after scroll",
        &mib_kib(report.peak_rss_kib),
        "informational",
        "INFO",
    );
    let overhead = report.rss_overhead_mib();
    row(
        out,
        "RSS overhead vs scratch",
        &overhead.map_or("n/a".to_string(), |v| format!("{v:.1} MiB")),
        &format!("<={RSS_OVERHEAD_TARGET_MIB} MiB (9 + 64 cache)"),
        &verdict_at_or_below(overhead, RSS_OVERHEAD_TARGET_MIB as f64),
    );
    let _ = writeln!(out);
}

fn row(out: &mut String, metric: &str, measured: &str, target: &str, result: &str) {
    let _ = writeln!(out, "| {metric} | {measured} | {target} | {result} |");
}

fn verdict_below(value: Option<f64>, target: f64) -> String {
    match value {
        Some(v) if v < target => "PASS".to_string(),
        Some(_) => "FAIL".to_string(),
        None => "NOT OBSERVED".to_string(),
    }
}

fn verdict_at_or_below(value: Option<f64>, target: f64) -> String {
    match value {
        Some(v) if v <= target => "PASS".to_string(),
        Some(_) => "FAIL".to_string(),
        None => "NOT OBSERVED".to_string(),
    }
}

fn ms(value: Option<f64>) -> String {
    value.map_or("not observed".to_string(), |v| format!("{v:.2} ms"))
}

fn mib_kib(value: Option<u64>) -> String {
    value.map_or("n/a".to_string(), |kib| {
        format!("{:.1} MiB", kib as f64 / 1024.0)
    })
}

fn search(report: &Report) -> String {
    match (report.search_ms, report.search_mib_per_second) {
        (Some(time), Some(rate)) => format!("{time:.1} ms, {rate:.0} MiB/s"),
        _ => "not observed".to_string(),
    }
}

/// A UTC timestamp without pulling in a date dependency.
fn utc_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = civil_from_unix(seconds);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Converts Unix seconds to a UTC calendar date (Howard Hinnant's algorithm).
fn civil_from_unix(seconds: u64) -> (i64, u32, u32, u32, u32, u32) {
    const SECONDS_PER_DAY: u64 = 86_400;
    let days = (seconds / SECONDS_PER_DAY) as i64;
    let time_of_day = seconds % SECONDS_PER_DAY;
    let hour = (time_of_day / 3600) as u32;
    let minute = ((time_of_day % 3600) / 60) as u32;
    let second = (time_of_day % 60) as u32;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_part + 2) / 5 + 1) as u32;
    let month = if month_part < 10 {
        month_part + 3
    } else {
        month_part - 9
    } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, minute, second)
}
