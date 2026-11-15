//! Corpus generation on local disk. Sizes mirror DESIGN 8's defined
//! corpora (10 MB, 1 GB, 10 GB logs with realistic line lengths, plus a
//! 100 MB single-line file). Generation is deterministic, so a corpus is
//! written once and reused across runs, and every corpus ends with a unique
//! needle at EOF for the search benchmark.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// A corpus is written once and reused; generating needs this much free
/// space beyond the file itself for the filesystem's own overhead.
const FREE_SPACE_HEADROOM: u64 = 64 * MIB;

/// Size of one deterministic template block, copied to fill a corpus.
const TEMPLATE_SIZE: usize = 64 * 1024;
/// Shortest and longest log line in the multi-line corpora.
const MIN_LINE: usize = 24;
const MAX_LINE: usize = 200;
/// Seed for the template's reproducible pseudo-random content.
const TEMPLATE_SEED: u64 = 0x9E37_79B9_7F4A_7C15;
/// Bytes a generated log line is built from: lowercase so the uppercase
/// needle and reload marker can never occur in the content by chance.
const CONTENT_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789 ./:-_=";

/// A unique token placed only at EOF, so a forward search for it must scan
/// the whole corpus.
pub const NEEDLE: &str = "QZXKV_EMX_BENCH_NEEDLE_7F3C9";
/// The line a reload benchmark appends; unique for the same reason.
pub const RELOAD_MARKER: &str = "QZXKV_EMX_RELOAD_MARKER_5A1D8";

const BUFFERED_WRITE_CAPACITY: usize = 4 * 1024 * 1024;

/// Which corpus to benchmark.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Size {
    TenMegabytes,
    HundredMegabyteLine,
    OneGigabyte,
    TenGigabytes,
}

impl FromStr for Size {
    type Err = String;

    fn from_str(text: &str) -> Result<Size, String> {
        match text {
            "10MB" => Ok(Size::TenMegabytes),
            "100MB-line" => Ok(Size::HundredMegabyteLine),
            "1GB" => Ok(Size::OneGigabyte),
            "10GB" => Ok(Size::TenGigabytes),
            other => Err(format!(
                "unknown size {other:?}; expected 10MB, 100MB-line, 1GB or 10GB"
            )),
        }
    }
}

impl Size {
    pub fn label(self) -> &'static str {
        match self {
            Size::TenMegabytes => "10MB",
            Size::HundredMegabyteLine => "100MB-line",
            Size::OneGigabyte => "1GB",
            Size::TenGigabytes => "10GB",
        }
    }

    /// Target body size before the EOF needle is added.
    fn body_bytes(self) -> u64 {
        match self {
            Size::TenMegabytes => 10 * MIB,
            Size::HundredMegabyteLine => 100 * MIB,
            Size::OneGigabyte => GIB,
            Size::TenGigabytes => 10 * GIB,
        }
    }

    /// Whether the corpus is one very long line (no interior newlines).
    fn single_line(self) -> bool {
        matches!(self, Size::HundredMegabyteLine)
    }

    fn file_name(self) -> String {
        if self.single_line() {
            format!("emx-bench-{}.txt", self.label())
        } else {
            format!("emx-bench-{}.log", self.label())
        }
    }

    /// The exact size the generated file has, so a cached file can be
    /// recognised without rereading it.
    fn expected_size(self) -> u64 {
        let copies = self.body_bytes() / TEMPLATE_SIZE as u64;
        let body = copies * TEMPLATE_SIZE as u64;
        let tail = if self.single_line() {
            // The needle extends the single line, then one newline ends it.
            NEEDLE.len() as u64 + 1
        } else {
            // A newline ends the last body line, then the needle line.
            1 + NEEDLE.len() as u64 + 1
        };
        body + tail
    }
}

/// A corpus ready to benchmark.
pub struct Corpus {
    pub path: PathBuf,
    pub size: Size,
    pub bytes: u64,
    pub single_line: bool,
}

/// The result of preparing a corpus: ready, or skipped because it would not
/// fit local disk.
pub enum Prepared {
    Ready(Corpus),
    Skipped { reason: String },
}

/// Where corpora live: `$EMX_BENCH_CORPUS_DIR`, else the system temporary
/// directory. Never the repository, which is on a network filesystem.
pub fn directory() -> PathBuf {
    match std::env::var_os("EMX_BENCH_CORPUS_DIR") {
        Some(value) => PathBuf::from(value),
        None => std::env::temp_dir(),
    }
}

/// Reuses the corpus if it is already present at its exact size, else checks
/// free space and generates it.
pub fn ensure(dir: &Path, size: Size) -> Result<Prepared, String> {
    let path = dir.join(size.file_name());
    let expected = size.expected_size();

    if let Ok(metadata) = fs::metadata(&path)
        && metadata.len() == expected
    {
        return Ok(Prepared::Ready(Corpus {
            path,
            size,
            bytes: expected,
            single_line: size.single_line(),
        }));
    }

    let required = expected + expected / 10 + FREE_SPACE_HEADROOM;
    let available = available_bytes(dir)?;
    if available < required {
        return Ok(Prepared::Skipped {
            reason: format!(
                "needs {} but only {} is free on {}",
                human(required),
                human(available),
                dir.display()
            ),
        });
    }

    generate(&path, size).map_err(|error| format!("generating {}: {error}", path.display()))?;
    Ok(Prepared::Ready(Corpus {
        path,
        size,
        bytes: expected,
        single_line: size.single_line(),
    }))
}

fn available_bytes(dir: &Path) -> Result<u64, String> {
    let stat = rustix::fs::statvfs(dir)
        .map_err(|error| format!("checking free space on {}: {error}", dir.display()))?;
    Ok(stat.f_bavail * stat.f_frsize)
}

fn generate(path: &Path, size: Size) -> std::io::Result<()> {
    let template = template(size.single_line());
    let copies = size.body_bytes() / TEMPLATE_SIZE as u64;
    let file = File::create(path)?;
    let mut writer = BufWriter::with_capacity(BUFFERED_WRITE_CAPACITY, &file);
    for _ in 0..copies {
        writer.write_all(&template)?;
    }
    if size.single_line() {
        writer.write_all(NEEDLE.as_bytes())?;
        writer.write_all(b"\n")?;
    } else {
        writer.write_all(b"\n")?;
        writer.write_all(NEEDLE.as_bytes())?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    // Make pages clean so a cold-cache pass can drop them with fadvise.
    file.sync_all()?;
    Ok(())
}

/// One reproducible template block: log lines of varying length, or one
/// continuous run of content bytes for the single-line corpus.
fn template(single_line: bool) -> Vec<u8> {
    let mut state = TEMPLATE_SEED;
    let mut buffer = Vec::with_capacity(TEMPLATE_SIZE);
    let mut column = 0usize;
    let mut line_length = next_line_length(&mut state);
    while buffer.len() < TEMPLATE_SIZE {
        if !single_line && column >= line_length {
            buffer.push(b'\n');
            column = 0;
            line_length = next_line_length(&mut state);
        } else {
            let index = (next_random(&mut state) as usize) % CONTENT_ALPHABET.len();
            buffer.push(CONTENT_ALPHABET[index]);
            column += 1;
        }
    }
    buffer.truncate(TEMPLATE_SIZE);
    buffer
}

fn next_line_length(state: &mut u64) -> usize {
    MIN_LINE + (next_random(state) as usize) % (MAX_LINE - MIN_LINE)
}

fn next_random(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    value
}

/// Appends the reload marker as a new line, returning the file's length
/// before the append so the caller can restore the corpus afterwards.
pub fn append_reload_marker(path: &Path) -> std::io::Result<u64> {
    let original = fs::metadata(path)?.len();
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(RELOAD_MARKER.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.sync_all()?;
    Ok(original)
}

/// Restores a corpus to `length`, undoing an appended reload marker so the
/// cached corpus keeps its exact size for the next run.
pub fn truncate_to(path: &Path, length: u64) -> std::io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(length)?;
    file.sync_all()
}

fn human(bytes: u64) -> String {
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    }
}
