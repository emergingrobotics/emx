//! The measurements themselves: startup, open (cold and warm), keypress and
//! scroll latency, search throughput, reload latency and resident memory,
//! each bounded by a deadline so a wedged editor cannot run forever.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::corpus::{self, Corpus, NEEDLE, RELOAD_MARKER};
use crate::pty::{self, Pty};

/// DESIGN 8: keypress-to-paint under 16 ms at all file sizes.
pub const KEYPRESS_TARGET_MS: f64 = 16.0;
/// The settle window from the global constraints (DESIGN 1 and 3).
pub const SETTLE_WINDOW_MS: u64 = 200;
/// DESIGN 8: external write to updated buffer within the settle window plus
/// this much.
pub const RELOAD_EXTRA_MS: u64 = 100;
pub const RELOAD_TARGET_MS: f64 = (SETTLE_WINDOW_MS + RELOAD_EXTRA_MS) as f64;
/// DESIGN 8: overhead for an unedited multi-gigabyte file in single-digit MB
/// plus the block-cache budget.
pub const RSS_OVERHEAD_SINGLE_DIGIT_MIB: u64 = 9;
pub const BLOCK_CACHE_BUDGET_MIB: u64 = 64;
pub const RSS_OVERHEAD_TARGET_MIB: u64 = RSS_OVERHEAD_SINGLE_DIGIT_MIB + BLOCK_CACHE_BUDGET_MIB;

const KEYPRESS_SAMPLES: usize = 200;
const SCROLL_SAMPLES: usize = 100;
/// Per-keystroke paint deadline: generous next to the 16 ms target, and
/// short enough that a keystroke that paints nothing (point already at the
/// buffer end) is passed over quickly rather than stalling the run.
const KEYPRESS_PAINT_WAIT: Duration = Duration::from_millis(300);
const FIRST_PAINT_DEADLINE: Duration = Duration::from_secs(60);
const RELOAD_DEADLINE: Duration = Duration::from_secs(30);
/// Floor on the search deadline, plus one second for every this-many bytes,
/// so a larger corpus is allowed proportionally longer to be scanned.
const SEARCH_DEADLINE_FLOOR: Duration = Duration::from_secs(60);
const SEARCH_BYTES_PER_DEADLINE_SECOND: u64 = 10 * 1024 * 1024;
/// Let the first frame finish and the file watcher establish before a
/// measurement that depends on them.
const SETTLE: Duration = Duration::from_millis(500);

const MIB_F: f64 = (1024 * 1024) as f64;

pub struct Statistics {
    pub median_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub painted: usize,
    pub total: usize,
}

pub struct Report {
    pub corpus_label: String,
    pub corpus_path: PathBuf,
    pub corpus_bytes: u64,
    pub single_line: bool,
    pub emx: PathBuf,
    pub startup_first_frame_ms: Option<f64>,
    pub open_cold_ms: Option<f64>,
    pub open_warm_ms: Option<f64>,
    pub keypress: Statistics,
    pub scroll: Statistics,
    pub search_ms: Option<f64>,
    pub search_mib_per_second: Option<f64>,
    pub reload_ms: Option<f64>,
    pub scratch_rss_kib: Option<u64>,
    pub open_rss_kib: Option<u64>,
    pub peak_rss_kib: Option<u64>,
}

impl Report {
    pub fn rss_overhead_mib(&self) -> Option<f64> {
        let peak = self.peak_rss_kib?;
        let scratch = self.scratch_rss_kib?;
        Some((peak.saturating_sub(scratch)) as f64 / 1024.0)
    }

    pub fn summary(&self) -> String {
        let field = |value: Option<f64>| value.map_or("n/a".to_string(), |v| format!("{v:.1}ms"));
        format!(
            "emx-bench {}: open_cold={} keypress_p99={} scroll_p99={} search={} reload={} rss_overhead={}",
            self.corpus_label,
            field(self.open_cold_ms),
            field(self.keypress.p99_ms),
            field(self.scroll.p99_ms),
            field(self.search_ms),
            field(self.reload_ms),
            self.rss_overhead_mib()
                .map_or("n/a".to_string(), |v| format!("{v:.1}MiB")),
        )
    }
}

pub fn run(emx: &Path, corpus: &Corpus) -> Result<Report, String> {
    let session_root = corpus::directory().join(format!("emx-bench-env-{}", std::process::id()));
    let mut session = 0u32;
    let mut next_env = || {
        session += 1;
        session_root.join(format!("s{session}"))
    };

    let corpus_arg = corpus
        .path
        .to_str()
        .ok_or_else(|| "corpus path is not valid UTF-8".to_string())?
        .to_string();

    // Startup to first frame and the baseline resident memory, with no file.
    let (startup_first_frame_ms, scratch_rss_kib) = {
        let pty = Pty::spawn(emx, &["--no-server"], &next_env())?;
        let start = Instant::now();
        let first = pty.wait_screen(start, start + FIRST_PAINT_DEADLINE, any_content);
        thread::sleep(SETTLE);
        let rss = pty.pid().and_then(rss_kib);
        (first.map(millis), rss)
    };

    // Cold open: drop the corpus from the page cache first, then open it and
    // run the interactive suite on the same session.
    drop_page_cache(&corpus.path)?;
    let (
        open_cold_ms,
        keypress,
        scroll,
        search_ms,
        search_mib_per_second,
        reload_ms,
        open_rss_kib,
        peak_rss_kib,
    ) = {
        let pty = Pty::spawn(emx, &["--no-server", &corpus_arg], &next_env())?;
        let start = Instant::now();
        let open_cold = pty.wait_screen(start, start + FIRST_PAINT_DEADLINE, any_content);
        thread::sleep(SETTLE);
        let open_rss = pty.pid().and_then(rss_kib);

        let keypress = latency_series(&pty, pty::CTRL_N, KEYPRESS_SAMPLES);
        let scroll = latency_series(&pty, pty::CTRL_V, SCROLL_SAMPLES);
        let peak_rss = pty.pid().and_then(rss_kib);

        let (search_ms, search_mib) = measure_search(&pty, corpus.bytes);
        let reload_ms = measure_reload(&pty, &corpus.path)?;

        (
            open_cold.map(millis),
            keypress,
            scroll,
            search_ms,
            search_mib,
            reload_ms,
            open_rss,
            peak_rss,
        )
    };

    // Warm open: the cold pass has warmed the cache, so this is the warm-cache
    // open time.
    let open_warm_ms = {
        let pty = Pty::spawn(emx, &["--no-server", &corpus_arg], &next_env())?;
        let start = Instant::now();
        pty.wait_screen(start, start + FIRST_PAINT_DEADLINE, any_content)
            .map(millis)
    };

    drop(std::fs::remove_dir_all(&session_root));

    Ok(Report {
        corpus_label: corpus.size.label().to_string(),
        corpus_path: corpus.path.clone(),
        corpus_bytes: corpus.bytes,
        single_line: corpus.single_line,
        emx: emx.to_path_buf(),
        startup_first_frame_ms,
        open_cold_ms,
        open_warm_ms,
        keypress,
        scroll,
        search_ms,
        search_mib_per_second,
        reload_ms,
        scratch_rss_kib,
        open_rss_kib,
        peak_rss_kib,
    })
}

/// Sends `key` `count` times, timing the paint after each; keystrokes that
/// paint nothing (the buffer end is already shown) are counted but excluded
/// from the latency statistics.
fn latency_series(pty: &Pty, key: &[u8], count: usize) -> Statistics {
    let mut samples = Vec::new();
    for _ in 0..count {
        let from = pty.received();
        let start = Instant::now();
        pty.write(key);
        if let Some(elapsed) = pty.wait_received_beyond(from, start, start + KEYPRESS_PAINT_WAIT) {
            samples.push(elapsed);
        }
    }
    statistics(samples, count)
}

/// Searches forward for the EOF needle and times the scan to the match.
fn measure_search(pty: &Pty, corpus_bytes: u64) -> (Option<f64>, Option<f64>) {
    let deadline = SEARCH_DEADLINE_FLOOR
        + Duration::from_secs(corpus_bytes / SEARCH_BYTES_PER_DEADLINE_SECOND);
    let start = Instant::now();
    pty.write(pty::CTRL_S);
    pty.write(NEEDLE.as_bytes());
    let found = pty.wait_screen(start, start + deadline, |rows| {
        window_rows(rows).iter().any(|row| row.contains(NEEDLE))
    });
    // Leave incremental search either way.
    pty.write(pty::CTRL_G);
    pty.write(pty::CTRL_G);
    match found {
        Some(elapsed) => {
            let seconds = elapsed.as_secs_f64();
            let mib_per_second = if seconds > 0.0 {
                Some((corpus_bytes as f64 / MIB_F) / seconds)
            } else {
                None
            };
            (Some(millis(elapsed)), mib_per_second)
        }
        None => (None, None),
    }
}

/// Goes to the buffer end, appends a marker to the file, and times the frame
/// that shows it. Restores the corpus afterwards so its cached size holds.
fn measure_reload(pty: &Pty, path: &Path) -> Result<Option<f64>, String> {
    let from = pty.received();
    let moved = Instant::now();
    pty.write(pty::META_GREATER);
    let _ = pty.wait_received_beyond(from, moved, moved + KEYPRESS_PAINT_WAIT);
    thread::sleep(SETTLE);

    let start = Instant::now();
    let original = corpus::append_reload_marker(path)
        .map_err(|error| format!("appending the reload marker to {}: {error}", path.display()))?;
    let reloaded = pty.wait_screen(start, start + RELOAD_DEADLINE, |rows| {
        window_rows(rows)
            .iter()
            .any(|row| row.contains(RELOAD_MARKER))
    });
    corpus::truncate_to(path, original).map_err(|error| {
        format!(
            "restoring {} after the reload test: {error}",
            path.display()
        )
    })?;
    Ok(reloaded.map(millis))
}

/// The window's content rows, without the modeline and echo area, so the
/// echo area's live search prompt does not count as a match.
fn window_rows(rows: &[String]) -> &[String] {
    let content = usize::from(pty::ROWS).saturating_sub(2);
    &rows[..content.min(rows.len())]
}

fn any_content(rows: &[String]) -> bool {
    rows.iter().any(|row| !row.trim().is_empty())
}

fn statistics(mut samples: Vec<Duration>, total: usize) -> Statistics {
    samples.sort_unstable();
    let painted = samples.len();
    let percentile = |fraction: f64| -> Option<f64> {
        if samples.is_empty() {
            return None;
        }
        let rank = ((samples.len() as f64) * fraction).ceil() as usize;
        let index = rank.saturating_sub(1).min(samples.len() - 1);
        Some(millis(samples[index]))
    };
    Statistics {
        median_ms: percentile(0.5),
        p99_ms: percentile(0.99),
        painted,
        total,
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn rss_kib(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

/// Drops the file from the page cache for a cold-cache pass. On some
/// filesystems `DontNeed` does not fully evict; the corpora live on local
/// disk, where it does.
fn drop_page_cache(path: &Path) -> Result<(), String> {
    let file = File::open(path)
        .map_err(|error| format!("opening {} to drop its cache: {error}", path.display()))?;
    // A length of `None` advises to the end of the file.
    rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed)
        .map_err(|error| format!("fadvise DontNeed on {}: {error}", path.display()))?;
    Ok(())
}
