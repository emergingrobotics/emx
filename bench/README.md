# emx benchmarks

`emx-bench` measures `emx --no-server` against the DESIGN section 8 performance targets. It is run through the Makefile: `make bench` covers the 10 MB, 100 MB single-line and 1 GB corpora; `make bench-full` adds the optional 10 GB corpus.

## Method

- Corpora are generated on local disk (`$EMX_BENCH_CORPUS_DIR`, else the system temporary directory), never under this repository: the repository is on a network filesystem where multi-gigabyte writes are slow and would measure the network rather than the editor. A corpus is written once and reused; each ends with a unique needle at EOF for the search benchmark.
- The editor is driven on a real pseudo-terminal, its screen parsed with `vt100`, keys sent with the Kitty flags-5 encoding. Every interaction is bounded by a deadline that kills the editor if it hangs.
- Keypress and scroll latency is the time from writing the key to the first byte of the paint that follows, over 200 `C-n` and 100 `C-v`. Keystrokes that paint nothing (the buffer end is already shown) are counted but left out of the statistics; the single-line corpus paints only on the first.
- Open-to-first-paint is measured cold (page cache dropped with `fadvise(DontNeed)`) and warm. Resident memory (`VmRSS` from `/proc/<pid>/status`) is read for the empty scratch buffer and, on the corpus, after opening and after scrolling.

## Targets (DESIGN 8) and what is measurable here

- Keypress-to-paint under 16 ms at all file sizes, measured locally at the PTY: measured, pass/fail recorded.
- First paint under 50 ms on a Raspberry Pi 4 with a warm cache: NOT measurable in this environment (x86-64, no Pi hardware). The x86-64 open times are recorded for reference only.
- Memory overhead for an unedited multi-gigabyte file in single-digit MB plus the block-cache budget (64 MiB): measured as peak corpus RSS minus scratch RSS, checked against a 73 MiB ceiling; most meaningful on the 1 GB and 10 GB corpora.
- External write to updated buffer within the settle window plus 100 ms (300 ms): measured, pass/fail recorded.
- Binary size: no numeric budget was fixed in DESIGN ("to be set before M2"); the static-build sizes are recorded in the static-builds section below rather than failed against a number.
- aarch64 runs: the aarch64 static binary is built but cannot be run here (no aarch64 hardware or emulator); these numbers are x86-64 only.

## Run 2026-11-15 02:52:19 UTC — 10MB

- corpus: `/tmp/emx-bench-10MB.log` (10485790 bytes)
- editor: `/home/gherlein/h/src/er/emx/bin/emx`
- host: x86-64 (aarch64 and Pi 4 not measurable here)

| Metric | Measured | DESIGN 8 target | Result |
|---|---|---|---|
| startup-to-first-frame (scratch) | 105.03 ms | informational | INFO |
| open-to-first-paint, cold cache | 5.59 ms | <50 ms on Pi 4 warm | NOT MEASURABLE HERE |
| open-to-first-paint, warm cache | 4.92 ms | <50 ms on Pi 4 warm | NOT MEASURABLE HERE |
| keypress-to-paint median (C-n, 200/200 painted) | 0.39 ms | <16 ms | PASS |
| keypress-to-paint p99 (C-n) | 2.29 ms | <16 ms | PASS |
| scroll median (C-v, 100/100 painted) | 2.10 ms | <16 ms | PASS |
| scroll p99 (C-v) | 3.13 ms | <16 ms | PASS |
| search to EOF needle (C-s) | 5.2 ms, 1931 MiB/s | informational | INFO |
| reload latency (external append to frame) | 222.24 ms | <=300 ms (settle 200 + 100) | PASS |
| RSS, scratch buffer | 9.3 MiB | informational | INFO |
| RSS, after open | 19.9 MiB | informational | INFO |
| RSS, peak after scroll | 20.2 MiB | informational | INFO |
| RSS overhead vs scratch | 10.9 MiB | <=73 MiB (9 + 64 cache) | PASS |

## Run 2026-11-15 02:54:04 UTC — 100MB-line

- corpus: `/tmp/emx-bench-100MB-line.txt` (104857629 bytes, single line)
- editor: `/home/gherlein/h/src/er/emx/bin/emx`
- host: x86-64 (aarch64 and Pi 4 not measurable here)

| Metric | Measured | DESIGN 8 target | Result |
|---|---|---|---|
| startup-to-first-frame (scratch) | 8.01 ms | informational | INFO |
| open-to-first-paint, cold cache | 65.15 ms | <50 ms on Pi 4 warm | NOT MEASURABLE HERE |
| open-to-first-paint, warm cache | 5.71 ms | <50 ms on Pi 4 warm | NOT MEASURABLE HERE |
| keypress-to-paint median (C-n, 200/200 painted) | 1.67 ms | <16 ms | PASS |
| keypress-to-paint p99 (C-n) | 5.32 ms | <16 ms | PASS |
| scroll median (C-v, 100/100 painted) | 15.29 ms | <16 ms | PASS |
| scroll p99 (C-v) | 25.91 ms | <16 ms | FAIL |
| search to EOF needle (C-s) | not observed | informational | INFO |
| reload latency (external append to frame) | not observed | <=300 ms (settle 200 + 100) | NOT OBSERVED |
| RSS, scratch buffer | 9.4 MiB | informational | INFO |
| RSS, after open | 9.0 MiB | informational | INFO |
| RSS, peak after scroll | 10.6 MiB | informational | INFO |
| RSS overhead vs scratch | 1.2 MiB | <=73 MiB (9 + 64 cache) | PASS |

## Run 2026-11-15 02:54:06 UTC — 1GB

- corpus: `/tmp/emx-bench-1GB.log` (1073741854 bytes)
- editor: `/home/gherlein/h/src/er/emx/bin/emx`
- host: x86-64 (aarch64 and Pi 4 not measurable here)

| Metric | Measured | DESIGN 8 target | Result |
|---|---|---|---|
| startup-to-first-frame (scratch) | 4.87 ms | informational | INFO |
| open-to-first-paint, cold cache | 5.01 ms | <50 ms on Pi 4 warm | NOT MEASURABLE HERE |
| open-to-first-paint, warm cache | 4.88 ms | <50 ms on Pi 4 warm | NOT MEASURABLE HERE |
| keypress-to-paint median (C-n, 200/200 painted) | 0.38 ms | <16 ms | PASS |
| keypress-to-paint p99 (C-n) | 2.57 ms | <16 ms | PASS |
| scroll median (C-v, 100/100 painted) | 1.31 ms | <16 ms | PASS |
| scroll p99 (C-v) | 2.68 ms | <16 ms | PASS |
| search to EOF needle (C-s) | 113.3 ms, 9036 MiB/s | informational | INFO |
| reload latency (external append to frame) | 224.53 ms | <=300 ms (settle 200 + 100) | PASS |
| RSS, scratch buffer | 9.4 MiB | informational | INFO |
| RSS, after open | 10.7 MiB | informational | INFO |
| RSS, peak after scroll | 11.4 MiB | informational | INFO |
| RSS overhead vs scratch | 2.0 MiB | <=73 MiB (9 + 64 cache) | PASS |

## Static builds

`make static` builds static musl binaries for both targets with
cargo-zigbuild (zig as the cross C toolchain) and `mimalloc` as the global
allocator (DESIGN 8: musl's default allocator is too slow for this workload;
only the musl target compiles mimalloc in). Both are statically linked and
stripped.

| Target | Size (bytes) | Size | Linkage | Runnable here |
|---|---|---|---|---|
| x86_64-unknown-linux-musl | 5176432 | 4.94 MiB | static | yes (`emx 0.0.1`) |
| aarch64-unknown-linux-musl | 4579976 | 4.37 MiB | static | no (no aarch64 hardware or emulator; build-only) |

DESIGN 8 set no numeric binary-size budget ("to be set before M2"), so these
sizes are recorded rather than failed against a target. The bundled
tree-sitter grammar set DESIGN 8 says to count against that budget is not
present in this release.
