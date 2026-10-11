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
| mdroots | 0.2.9, release build of commit 644c8ec, rustc 1.98.0 |
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
| `initialize` response | 10 ms [9–38] | 10 ms [8–16] | 31 ms [31–35] | 2.02 s [1.99–2.12] | 969 ms [961–1259] |
| first definition (correct target) | 15 ms [10–39] | 10 ms [9–17] | 32 ms [32–36] | 2.02 s [1.99–2.12] | 985 ms [977–1279] |
| first non-empty completion | 15 ms [10–39] | 11 ms [9–17] | 40 ms [40–46] | 2.03 s [2.00–2.13] | 1000 ms [993–1296] |
| first full completion (≥10 items) | 323 ms [286–3446] | 63 ms [61–70] | 40 ms [40–46] | 2.03 s [2.00–2.13] | 1000 ms [993–1296] |
| first documentSymbol | 16 ms [10–40] | 11 ms [9–17] | not supported | not supported | 1.00 s [1.00–1.30] |
| first workspaceSymbol ("Note 12") | 331 ms [294–3454] | 71 ms [69–78] | not supported | not supported | 1.01 s [1.01–1.31] |
| broken-link error on open page | 318 ms [280–3440] | 58 ms [56–65] | 33 ms [33–37] | 2.02 s [1.99–2.12] | 1.25 s [1.24–1.55] |
| diagnostics on probe page (count) | 1 | 1 | 1 | 1 | 1 |
| files with published diagnostics | 1 | 1 | 1 | 1 | 112 |
| peak RSS (sampled 20 ms) | 26 MB [26–26] | 25 MB [25–25] | 37 MB [36–37] | 44 MB [44–45] | 139 MB [133–139] |
| phys_footprint at end | 17 MB [17–18] | 17 MB [17–17] | 25 MB [24–26] | 32 MB [29–32] | 105 MB [100–106] |
| phys_footprint_peak | 17 MB [17–18] | 17 MB [17–17] | 25 MB [24–26] | 32 MB [30–32] | 105 MB [100–106] |
| `zk index` from scratch | | | 1.51 s [1.45–1.66] | | |
| `zk index` no-op | | | 36 ms [33–62] | | |

### 10,000 notes (median [min–max] of 5 runs; times are ms since spawn)

| measure | mdroots cold | mdroots warm | zk (indexed) | zk (no index) | marksman |
|---|---|---|---|---|---|
| `initialize` response | 23 ms [8–62] | 8 ms [7–13] | 202 ms [200–212] | 124.51 s [116.71–167.58] | 4.64 s [4.57–5.29] |
| first definition (correct target) | 23 ms [10–64] | 9 ms [8–14] | 203 ms [201–213] | 124.51 s [116.72–167.58] | 4.65 s [4.58–5.31] |
| first non-empty completion | 24 ms [10–64] | 9 ms [8–14] | 287 ms [284–296] | 124.60 s [116.81–167.66] | 4.67 s [4.60–5.32] |
| first full completion (≥10 items) | 2.30 s [2.25–30.87] | 422 ms [406–427] | 287 ms [284–296] | 124.60 s [116.81–167.66] | 4.67 s [4.60–5.32] |
| first documentSymbol | 24 ms [10–64] | 9 ms [8–14] | not supported | not supported | 4.69 s [4.61–5.33] |
| first workspaceSymbol ("Note 12") | 2.38 s [2.33–30.95] | 504 ms [494–518] | not supported | not supported | 4.73 s [4.65–5.38] |
| broken-link error on open page | 2.26 s [2.21–30.82] | 379 ms [363–382] | 215 ms [213–226] | 124.53 s [116.73–167.59] | 5.23 s [5.16–5.87] |
| diagnostics on probe page (count) | 1 | 1 | 1 | 1 | 1 |
| files with published diagnostics | 1 | 1 | 1 | 1 | 1005/1013/1015 |
| peak RSS (sampled 20 ms) | 117 MB [117–118] | 118 MB [118–118] | 85 MB [84–88] | 122 MB [114–134] | 350 MB [350–355] |
| phys_footprint at end | 92 MB [92–92] | 92 MB [92–92] | 67 MB [46–68] | 69 MB [59–79] | 317 MB [317–322] |
| phys_footprint_peak | 92 MB [92–93] | 92 MB [92–92] | 68 MB [64–76] | 106 MB [98–118] | 317 MB [317–322] |
| `zk index` from scratch | | | 97.18 s [95.91–104.58] | | |
| `zk index` no-op | | | 213 ms [209–234] | | |

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
because some notes have two). At 10k two of the five runs shut down before
it had published them all (1,005 and 1,013 files), hence
"1005/1013/1015" in the table. mdroots and zk publish only for open
documents. All three see the same notebook and agree on the probe page.

## Reading the numbers

* **Warm is mdroots' design point, and there it is fast.**
  * At 10k it answers `initialize`, definition and documentSymbol in about
    9 ms.
  * At 10k it returns a full completion list in about 0.42 s,
    workspaceSymbol in about 0.50 s and the broken-link diagnostic in about
    0.38 s, at a steady 92 MB phys_footprint.
* **An already-indexed zk beats mdroots at 10k on the index-dependent
  measures.**
  * zk is faster on full completion (287 vs 422 ms) and on the broken-link
    diagnostic (215 vs 379 ms).
  * zk uses less memory: 67 MB vs 92 MB phys_footprint, and 85 vs 118 MB
    RSS.
  * At 1k, indexed zk is also faster on completion (40 vs 63 ms) and on the
    diagnostic (33 vs 58 ms). mdroots uses less memory there (17 vs 25 MB).
  * mdroots wins on time to `initialize`, definition and documentSymbol. It
    answers those from the open file and the cache before the root is
    loaded.
* **mdroots misses its own memory target at 10k.** The target is
  < 35 MB phys_footprint ([ROADMAP §1.3](../ROADMAP.md)); it was set from a
  3,000-note measurement. At 10k mdroots 0.2.10 holds 92 MB. At 1k it holds
  17 MB. The memory work after 0.2.10 brings that to 60 MB and 14 MB
  ([Memory](#memory)).
* **Cold mdroots vs zk with no index:**
  * mdroots' notebook-wide results take 2.3 s cold at 10k (median) and
    about 0.32 s at 1k.
  * `zk index` from scratch took 97 s at 10k and 1.5 s at 1k: it grows
    faster than the notebook.
  * `zk lsp` with no index blocks `initialize` until indexing is done:
    125 s at 10k.
* **marksman:**
  * It is the slowest to a first useful answer: about 1.0 s at 1k and 4.6 s
    at 10k. It holds every request until the workspace is loaded.
  * It uses the most memory: 105 MB at 1k and 317 MB at 10k phys_footprint.
  * Once loaded, its per-request latency is low (5–20 ms).
* **Against the earlier run** (mdroots 0.2.6, load average 13–34): on the
  quieter machine zk and marksman got faster (`zk index` 112 → 97 s,
  `zk lsp` with no index 144 → 125 s, marksman about 5%) and their ranges
  tightened. mdroots' medians barely moved. The conclusions are the same.

## Memory

After 0.2.10 the in-memory index was made smaller without changing what it
holds (the note text stays in memory, D9). Measured on the same 10k and 1k
notebooks, release build of local main e86c74a:

| | 0.2.10 | after | change |
|---|---|---|---|
| live heap of a `Workspace`, 10k notes | 75.5 MB | 38.0 MB | −50% |
| live heap, 1k notes | 7.5 MB | 3.8 MB | −49% |
| `mdroots lsp` phys_footprint at end, 10k (median of 3) | 92 MB | 60 MB | −35% |
| `mdroots lsp` phys_footprint_peak, 10k | 92 MB | 65 MB | |
| `mdroots lsp` phys_footprint, 1k | 17 MB | 14 MB | |
| peak RSS, 10k | 117 MB | 85 MB | |

Speed, `run.py --runs 3 10k`, medians (0.2.10 in brackets): warm full
completion 411 ms (422), warm broken-link diagnostic 368 ms (379), cold full
completion 2.33 s (2.30), cold diagnostic 2.29 s (2.26). Within the run's
noise; the first cold run of a fresh notebook was again the slow outlier
(30.8 s).

**Where the live heap went at 10k** (a counting global allocator over the
public API; the probe is not in the repo):

| | 0.2.10 | after | how |
|---|---|---|---|
| note text | 17.8 MB | 17.8 MB | kept (D9) |
| element vectors | ~19 MB + 6.4 MB spare capacity | 8.6 MB | `Link` 160 → 72 B and `Heading` 120 → 64 B (rare fields in one box, read through accessors), so `Element` 160 → 72 B; vectors shrunk to fit after parsing |
| key index | 12.2 MB | ~2.4 MB | note ids and per-kind hash maps of id lists, instead of a one-element `BTreeSet<String>` per key |
| per-note entries and stored keys | ~8.8 MB | ~5 MB | entries boxed and keyed by id; keys derived from the document when it changes |
| frontmatter | 4.0 MB | 1.8 MB | `fields()` built on demand (`Document::frontmatter_fields`) |
| backlink index | 3.6 MB | 1.0 MB | note ids instead of path strings |

The rest of the gap to phys_footprint (38 MB live, 60 MB footprint) is the
allocator, SQLite, thread stacks and the runtime.

**What is left above the 35 MB target at 10k:** the note text (17.8 MB) is
the largest single part. Dropping it means answering from the DB instead of
memory: derived tables, D9 ([ROADMAP §3](../ROADMAP.md#3-deferred-work)).
At 1k notes the target holds with room to spare.

## Caveats

* **The machine was quiet, not idle.**
  * The 1-minute load average was 5.3–11.9 (mean 7.4) during the run on
    16 cores, from background monitoring and security daemons. Nothing else
    was building. The earlier run had 13–34.
  * The medians are the figures to quote. The maxima are noisy.
* **The first cold mdroots run is an outlier.** In round 1 of each size,
  the first launch after the notebook was generated, mdroots cold took
  3.4 s at 1k (vs about 0.3 s in rounds 2–5) and 30.9 s at 10k (vs about
  2.3 s) to its notebook-wide results. Rounds 2–5 agree with each other,
  and the medians use all five. The cause was not measured; security
  daemons scanning the fresh notes and the new cache DB are a likely one.
  A rerun with `--work` on an existing work dir reuses the notebooks.
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
  * mdroots warm uses its [SQLite](https://sqlite.org) cache: 9.5 MB for
    1k notes and 50 MB for 10k, in the work dir.
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
