# Language server benchmark: mdroots vs zk vs marksman

mdroots, [zk](https://github.com/zk-org/zk) and
[marksman](https://github.com/artempyanykh/marksman) were timed as language
servers on two generated notebooks, 1,000 and 10,000 notes. The scripts are
in [bench/compare](../../bench/compare/README.md).

## Reproduce

From the repo root, with `zk` and `marksman` on `PATH`:

```
cargo build --release -p mdroots-cli && python3 bench/compare/run.py --mdroots target/release/mdroots
```

It works in a fresh temp dir and prints the tables below. On another
machine, or at another load, the numbers will differ.

## Versions and machine

| | |
|---|---|
| mdroots | 0.2.6, release build of commit 385d41e, rustc 1.98.0 |
| zk | 0.15.6 |
| marksman | 2026-02-08 |
| machine | Apple M4 Max, 16 cores, 64 GB RAM, macOS 27.0.1, APFS |
| harness | Python 3.14.7 |

## Notebooks

`bench/compare/gen.py N DIR` is deterministic. It uses the generator of
`crates/mdroots/examples/bench_root.rs` (same LCG, seed 42, same words).
Each note has:

* frontmatter `title`/`tags` and `# Note i`;
* four 60-word paragraphs, two `##` headings and two inline `#topicN` tags;
* five `[[note-NNNNN]]` links, of which 1 in 50 is a broken `[[missing-k]]`.

The notebook also gets a `.zk/config.toml` (wiki links,
`dead-link = "error"`) and an empty `.marksman.toml`. The 1k notebook has
112 broken links (3.9 MB); the 10k notebook has 1,049 (39 MB).

The probe page is `note-00000.md`. It has four valid links, one
`[[missing-probe]]` on line 23, and a trailing bare `[[` on line 25, where
completion is requested.

## Method

`bench/lspbench.py` is a stdio LSP client that times one server process:

* All times are **ms since the server process was spawned**: the time an
  editor user would wait. The client opens the probe page right after
  `initialized`.
* Methods missing from the server's `initialize` result are reported "not
  supported" and never sent.
* A result counts once it is useful. Definition must point at
  `note-00001.md`. Completion needs at least 10 items. documentSymbol and
  workspaceSymbol need at least one item. Pending methods are retried
  round-robin every 20 ms. "First non-empty completion" is the first answer
  with at least one item.
* Peak RSS of the process and its children is sampled with `ps` every
  20 ms. `phys_footprint` and `phys_footprint_peak` come from
  `footprint -j`, taken just before shutdown.
* Each run ends with a clean `shutdown`/`exit`, so mdroots persists its
  cache.

`bench/compare/run.py` runs 5 rounds per size. Each round runs every config
once, interleaved, after the page cache was warmed by reading every note:

* **mdroots cold**: `mdroots lsp` on a fresh, empty `MDROOTS_CACHE_DIR`.
* **mdroots warm**: a second process on the same cache dir.
* **zk (no index)**: `.zk/notebook.db*` deleted, then `zk lsp`, which
  indexes before it answers `initialize` (`--init-timeout 400`).
* **`zk index` from scratch / no-op**: db deleted, then
  `zk index --no-input -q` timed twice.
* **zk (indexed)**: `zk lsp` on that index.
* **marksman**: `marksman server`. It has no persistent state, so it is
  always cold.

`bench/compare/report.py` turns `results.jsonl` into the tables.

## Results

### 1,000 notes (median [min–max] of 5 runs; times are ms since spawn)

| measure | mdroots cold | mdroots warm | zk (indexed) | zk (no index) | marksman |
|---|---|---|---|---|---|
| `initialize` response | 11 ms [8–17] | 12 ms [8–17] | 32 ms [31–34] | 1.97 s [1.83–2.04] | 981 ms [964–1207] |
| first definition (correct target) | 12 ms [8–18] | 13 ms [9–18] | 33 ms [32–35] | 1.97 s [1.83–2.04] | 997 ms [980–1223] |
| first non-empty completion | 12 ms [9–18] | 13 ms [10–18] | 41 ms [40–43] | 1.98 s [1.84–2.05] | 1.01 s [0.99–1.24] |
| first full completion (≥10 items) | 260 ms [229–415] | 61 ms [57–71] | 41 ms [40–43] | 1.98 s [1.84–2.05] | 1.01 s [0.99–1.24] |
| first documentSymbol | 12 ms [9–18] | 14 ms [10–18] | not supported | not supported | 1.02 s [1.00–1.24] |
| first workspaceSymbol ("Note 12") | 265 ms [235–422] | 67 ms [64–78] | not supported | not supported | 1.03 s [1.01–1.26] |
| broken-link error on open page | 258 ms [226–412] | 59 ms [55–68] | 34 ms [33–36] | 1.97 s [1.83–2.04] | 1.26 s [1.24–1.50] |
| diagnostics on probe page (count) | 1 | 1 | 1 | 1 | 1 |
| files with published diagnostics | 1 | 1 | 1 | 1 | 112 |
| peak RSS (sampled 20 ms) | 26 MB [25–26] | 25 MB [25–25] | 36 MB [36–37] | 44 MB [43–45] | 139 MB [137–139] |
| phys_footprint at end | 17 MB [17–18] | 17 MB [17–17] | 24 MB [24–25] | 30 MB [28–31] | 105 MB [104–106] |
| phys_footprint_peak | 17 MB [17–18] | 17 MB [17–17] | 24 MB [24–25] | 30 MB [29–31] | 105 MB [104–106] |
| `zk index` from scratch | | | 1.44 s [1.42–1.53] | | |
| `zk index` no-op | | | 36 ms [35–43] | | |

### 10,000 notes (median [min–max] of 5 runs; times are ms since spawn)

| measure | mdroots cold | mdroots warm | zk (indexed) | zk (no index) | marksman |
|---|---|---|---|---|---|
| `initialize` response | 22 ms [14–92] | 8 ms [7–10] | 209 ms [207–250] | 143.94 s [122.45–162.60] | 4.90 s [4.45–9.60] |
| first definition (correct target) | 28 ms [15–98] | 9 ms [8–11] | 210 ms [208–250] | 143.94 s [122.45–162.60] | 4.92 s [4.47–9.61] |
| first non-empty completion | 28 ms [16–98] | 9 ms [8–11] | 294 ms [287–339] | 144.02 s [122.54–162.68] | 4.93 s [4.49–9.63] |
| first full completion (≥10 items) | 2.34 s [2.14–9.76] | 404 ms [396–424] | 294 ms [287–339] | 144.02 s [122.54–162.68] | 4.93 s [4.49–9.63] |
| first documentSymbol | 29 ms [16–98] | 9 ms [8–11] | not supported | not supported | 4.94 s [4.49–9.64] |
| first workspaceSymbol ("Note 12") | 2.42 s [2.22–9.84] | 480 ms [467–510] | not supported | not supported | 4.99 s [4.54–9.69] |
| broken-link error on open page | 2.32 s [2.12–9.74] | 380 ms [373–398] | 227 ms [220–264] | 143.95 s [122.47–162.61] | 5.54 s [5.08–10.25] |
| diagnostics on probe page (count) | 1 | 1 | 1 | 1 | 1 |
| files with published diagnostics | 1 | 1 | 1 | 1 | 1015 |
| peak RSS (sampled 20 ms) | 117 MB [117–118] | 117 MB [117–117] | 79 MB [78–83] | 136 MB [122–142] | 354 MB [351–354] |
| phys_footprint at end | 92 MB [92–93] | 92 MB [92–92] | 54 MB [51–64] | 68 MB [65–80] | 320 MB [317–321] |
| phys_footprint_peak | 92 MB [92–93] | 92 MB [92–92] | 64 MB [62–68] | 120 MB [106–126] | 320 MB [317–321] |
| `zk index` from scratch | | | 111.72 s [100.03–122.49] | | |
| `zk index` no-op | | | 220 ms [211–280] | | |

**Capabilities.** mdroots and marksman advertise all four methods. zk
0.15.6 has no documentSymbol or workspace/symbol: they are missing from its
capabilities, and workspace/symbol returns -32601.

**Diagnostics.** All three servers publish exactly one diagnostic on the
probe page, on line 23 (`[[missing-probe]]`), at error severity:

* mdroots: "broken link: missing-probe"
* zk: "not found"
* marksman: "Link to non-existent document 'missing-probe'"

marksman also publishes diagnostics for every file with a broken link: 112
files at 1k and 1,015 at 10k (the 1,049 broken links sit in 1,015 notes,
because some notes have two). mdroots and zk publish only for open
documents. All three see the same notebook and agree on the probe page.

## Reading the numbers

* **Warm is mdroots' design point, and there it is fast.**
  * At 10k it answers `initialize`, definition and documentSymbol in about
    9 ms.
  * At 10k it returns a full completion list in about 0.40 s,
    workspaceSymbol in about 0.48 s and the broken-link diagnostic in about
    0.38 s, at a steady 92 MB phys_footprint.
* **An already-indexed zk beats mdroots at 10k on the index-dependent
  measures.**
  * zk is faster on full completion (294 vs 404 ms) and on the broken-link
    diagnostic (227 vs 380 ms).
  * zk uses less memory: 54 MB vs 92 MB phys_footprint, and 79 vs 117 MB
    RSS.
  * At 1k, indexed zk is also faster on completion (41 vs 61 ms) and on the
    diagnostic (34 vs 59 ms). mdroots uses less memory there (17 vs 24 MB).
  * mdroots wins on time to `initialize`, definition and documentSymbol. It
    answers those from the open file and the cache before the root is
    loaded.
* **mdroots misses its own memory target at 10k.** The target is
  < 35 MB phys_footprint ([ROADMAP §1.3](../ROADMAP.md)); it was set from a
  3,000-note measurement. At 10k mdroots holds 92 MB. At 1k it holds 17 MB.
* **Cold mdroots vs zk with no index:**
  * mdroots' notebook-wide results take 2.3 s cold at 10k (median; one run
    took 9.8 s under load).
  * `zk index` from scratch took 112 s at 10k and 1.4 s at 1k: it grows
    faster than the notebook.
  * `zk lsp` with no index blocks `initialize` until indexing is done:
    144 s at 10k.
* **marksman:**
  * It is the slowest to a first useful answer: about 1.0 s at 1k and 4.9 s
    at 10k. It holds every request until the workspace is loaded.
  * It uses the most memory: 105 MB at 1k and 320 MB at 10k phys_footprint.
  * Once loaded, its per-request latency is low (5–20 ms).

## Caveats

* **The machine was loaded.**
  * The load average was 13–34 during the runs on 16 cores, from
    background security daemons and parallel builds.
  * The medians are the figures to quote; they are fairly stable. The
    maxima are noisy: one mdroots cold 10k run took 9.8 s, one marksman 10k
    run took 9.6 s, and the zk 10k index ranged from 100 to 122 s.
  * Re-run on an idle machine before relying on absolute numbers.
* **Synthetic notebook.** Notes are uniform and flat (one directory), link
  to random targets, and have no attachments or nested folders. Real
  notebooks differ.
* **"First result" depends on retries.** The client polls every 20 ms, so
  values carry about ±20 ms of retry granularity, plus the time spent
  serving the other pending methods in the round.
* **`initialize` time is not "ready" time.**
  * mdroots answers at once and loads the root in the background. Its early
    answers are partial: completion has only the current note,
    workspaceSymbol is empty, and the probe page gets a "not in indexed
    set" hint, which is not counted as a broken-link error.
  * zk (no index) and marksman block `initialize` until loaded.
  * Compare readiness on the full completion, workspaceSymbol and
    diagnostic rows.
* **Completion sizes differ.** mdroots and marksman cap the list at 50
  items with `isIncomplete: true`. zk returns all 999 or 9,999 notes, and
  the extra JSON is part of its latency.
* **Feature coverage differs.**
  * zk has no documentSymbol or workspaceSymbol.
  * marksman computes diagnostics for the whole workspace up front (more
    work, more memory). mdroots and zk diagnose open files only.
* **Persistence differs.**
  * mdroots warm uses its [SQLite](https://sqlite.org) cache: 9 MB for 1k
    notes and 51 MB for 10k, in the work dir.
  * zk indexed uses `.zk/notebook.db`, which it writes into the notebook.
  * marksman keeps nothing on disk.
  * zk (indexed) and mdroots warm are the like-for-like pair. zk (no index)
    or `zk index` is the counterpart of mdroots cold. marksman is always
    cold.
* **Memory accounting.** RSS includes shared pages. phys_footprint is what
  macOS charges the process, and is the better comparison. marksman is a
  .NET process, so its numbers include the GC heap and runtime.
  phys_footprint is a snapshot taken after all requests; phys_footprint_peak
  is the kernel's lifetime peak.
* **Isolation.** HOME, XDG dirs and TMPDIR were redirected into the work
  dir. marksman still looked up its user config under the macOS
  application-support dir, because .NET resolves that path without
  `$HOME`; none existed, and it created none. Nothing else was written
  outside the work dir. zk's `.zk/notebook.db` lives in the generated
  notebooks, inside it.
