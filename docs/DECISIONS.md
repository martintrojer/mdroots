# Decisions

The design choices behind mdroots that are hard to reverse. Mechanics live in the specs: [roots](specs/roots.md), [index](specs/index.md), [library](specs/library.md). Unresolved points are in [OPEN-QUESTIONS.md](OPEN-QUESTIONS.md).

Measurements below come from two testbed vaults: a ~730-note [zk](https://github.com/zk-org/zk) vault and a ~210-note research vault.

## D1. Name mdroots

**Decision**
- The name is `mdroots`, after the core idea: zero-config discovery of note roots.
- Crates: `mdroots`, `mdroots-syntax`, `mdroots-resolve`, `mdroots-core`, `mdroots-roots`, `mdroots-index`, `mdroots-lsp`. Cache dir and config names follow the name.
- Publish placeholder `0.0.0` crates for all names before announcing. Run a trademark search (USPTO/EUIPO, classes 9/42) before 0.1.
- No short alias yet: `mw` and `mdr` are taken. Run the checklist below on any candidate.

**Why**
- Requirements: contains md/mark/down, free on crates.io, no clash with common commands.
- Namespace check: crates.io free for every crate name; no PATH command, Homebrew formula, PyPI or Debian package; no GitHub repo or org; web search finds no software. `mdroots.com` is registered, `mdroots.org` is free. npm and trademark are unverified.

**Rejected**
- `mdls`: clashes with macOS `/usr/bin/mdls` (Spotlight), on every Mac's PATH.
- `markweft`: already used by a markdown editor and a Rust markdown converter.
- `markloom`, `markwell`, `markroot`, `markmesh`, `markway`: existing apps or markdown tools.
- `markhound`: live US trademark.

## D2. Library first, LSP is a thin adapter

**Decision**
- mdroots is a Rust library; `mdroots lsp`, the CLI and other embedders (TUIs, SSGs, MCP servers, other language servers) call the same API (`mdroots::Workspaces`) directly. The CLI is the first embedder.
- Synchronous core, no async runtime. By default no background thread: nothing polls, and the embedder calls `Workspace::refresh` or `refresh_paths` to pick up changes on disk. The only background thread is the opt-in native watcher (`Options::watch(true)`, set by `mdroots lsp`), which calls the same `refresh_paths`.
- Slow calls (`refresh`, `refresh_paths`, `full_text`, `diagnostics`, `rename_note`) take `&Cancel`. The LSP answers requests one at a time, in order; a `$/cancelRequest` drops a queued request, and a `didChange` drops the queued requests on that document.
- Each query sees one consistent state: the process's in-memory index (D9) with overlays applied at call time. A refresh builds a new index and swaps it in, so a concurrent change is never seen halfway.
- Planned: a non-blocking open that returns a `Lazy` workspace at once and finishes discovery in the background, so an editor never blocks on a slow filesystem. Today `open_for` is synchronous and discovery is bounded by its budgets.
- The library never writes user files. Refactors return a `WorkspaceEdit`.
- Extension traits (`FileSystem`, `Probe`, `Enumerator`, `ResolveEnv`) are `Send + Sync`. A workspace holds its root's DB connection behind a mutex, because `rusqlite::Connection` is `!Sync`; queries never touch it (D9).
- A feature that can't be tested without JSON-RPC is in the wrong crate.

**Why**
- [marksman](https://github.com/artempyanykh/marksman) (F#, exe-only) and zk (Go `internal/` packages) can't be reused as libraries; embedding was a goal from the start.
- Queries are microsecond-to-millisecond lookups; async adds nothing and would force a runtime on every embedder. Async callers use `spawn_blocking`.

**Rejected**
- Build on iwe's `liwe`: in-memory arena (cold every start, one copy per process), needs `.iwe` config, API explicitly unstable.
- Async core on tower-lsp: forces a runtime on embedders for no gain.
- (One crate with feature-gated modules: see D6.)

## D3. In-process only, no daemon; one writer per root (flock reconciler), readers never write

**Decision**
- mdroots always runs inside the embedding process. No daemon, RPC, sockets or remote backend.
- Per root, `flock(LOCK_EX|LOCK_NB)` on `<id>.lock` elects a **reconciler**: the native watcher, the FTS table, and later background sweeps. It is the only process that writes the root's DB.
- Every other process is a **read-only peer**: it serves queries from the DB plus its own overlays (unsaved buffers, and files it just saved, parsed in memory until the DB catches up). Its saves reach the DB through the reconciler's watcher or next sweep.
- A peer that wants freshness while no one holds the lock tries the lock and becomes the reconciler.
- Peers follow commits via `PRAGMA data_version` (~1.2 µs per call; changes only for other connections' commits) plus `change_log`.
- Crash recovery is the kernel releasing the flock (also on SIGKILL). No heartbeats, PID files or stale-lock cleanup.
- Only the reconciler acts on watcher events; peers act only on their own `didOpen`/`didChange`/`didSave`, so a save is not parsed N times.
- Built: the election, the `.open` lock, `discover.lock`, peers that never write, promotion on `refresh` (M4); the reconciler's native watcher and FTS table (M6). Peers pick up the reconciler's commits on their next `refresh` (D9). Not built: `data_version` following, the 5 s takeover retry and the stuck-reconciler warning.
- Use `std::fs::File::try_lock` / `try_lock_shared` (Rust ≥ 1.89). Rust opens files `O_CLOEXEC`, so children don't inherit locks; a second fd in the same process is refused (that process becomes a peer); flock does not conflict with [SQLite](https://sqlite.org)'s fcntl locks.

**Lock files** (in the base dir of D5; full rules in [specs/roots.md](specs/roots.md))

| File | Locked by | Purpose |
|---|---|---|
| `<id>.lock` | reconciler, `LOCK_EX\|LOCK_NB` | elects the reconciler. **Never unlinked**, because a process waiting on an unlinked lock file would elect a second reconciler |
| `<id>.open` | every process, `LOCK_SH`, while the DB is open | GC, schema cleanup and corruption rebuild need `LOCK_EX\|LOCK_NB` on it, so they never unlink a DB another process has open (a documented SQLite corruption path) |
| `discover.lock` (global) | the discovering process, during discovery stages 2–4 | serialises discovery: one walk per user at a time; others re-check the registry after taking it |

`.lock` and `.open` are never deleted, not even by GC of a whole root: they are tiny, and deleting one a process waits on would let a second process take it. One `.lock`/`.open` pair per root and schema covers every generation file of that root's DB.

**Transactions and failure modes**
- The reconciler's writes are `BEGIN IMMEDIATE` with `busy_timeout`, because a deferred read-then-write gets `SQLITE_BUSY` at once and ignores the timeout. Readers retry on `SQLITE_BUSY` during WAL recovery after a crash.
- The reconciler checkpoints periodically and caps WAL size, because a leaked long-lived reader blocks checkpoints.
- Stuck reconciler (alive but SIGSTOPed or hung on a network/virtual FS `stat`): no liveness detection. A peer that sees a stale `meta.reconciled_at` logs one warning and point-checks its own open files and their link targets in memory. It still writes nothing.
- Watcher gap on takeover: changes made while no reconciler watched are found by the re-list and re-stat the new reconciler runs on promotion (and every process runs on open). FSEvents replay would avoid that walk; it is deferred (D9).
- Herd start (ten editors at once): one process walks under `discover.lock`, one wins the root flock, nine serve from the existing index. A "lazy" verdict from the walk-rate check alone is not saved until a second measurement agrees, so a herd-slowed walk doesn't stick.

**Why**
- Many editors run at once (tmux, session restore); many processes are short-lived or killed.
- A single writer removes write races and the freshness-fence problems that come with them (see D4). A peer's own edits are already fresh in its overlay, so peer writes buy nothing visible.
- A daemon would save the per-process in-memory index (D9) and duplicate parsing; the persistent state lives in SQLite, and the derived tables (D9) will move the index there too. gopls's daemon saves hundreds of MB and is still off by default.
- What makes gopls fast (snapshots, cancellation, two-phase diagnostics, in-order requests, a persistent cache) needs no daemon; mdroots adopts all of it in-process ([research/gopls.md](research/gopls.md)).
- A daemon would not remove schema-versioned DB files or GC locks (old and new daemons overlap during upgrades), and embedders would need a running daemon or a second backend.

**Costs accepted**
- Other processes' saves reach a peer after the reconciler's watcher latency (~100–500 ms).
- Memory scales with editors. Target: < 35 MB `phys_footprint` per process, measured with 10 running.

**Revisit only with numbers.** If a 10-editor measurement shows per-process memory or duplicate work is a real problem, write a new decision. Until then no daemon code exists.

**Rejected**
- Per-user daemon (gopls `-remote=auto` style): socket lifecycle, versioned private RPC mirroring the whole API, environment handover, one crash domain for all editors; small gain.
- Daemon with in-process fallback: carries every mechanism of both designs.
- Any process may write point updates: write races and fence bugs for no user-visible gain.
- Every process indexes independently: wasted CPU and N copies in memory.

## D4. Persistent index = one SQLite DB per root, a disposable cache

**Decision**
- What a row holds, and how processes use it, is D9.
- One SQLite DB (WAL) per root, `roots/<id>.v<schema>.db`, plus a registry `roots.v<k>.db`. Schema versions are in the filenames, so old and new binaries never share a file or fight over migrations.
- The DB is a **cache**: correctness never depends on it. It is rebuilt on schema change, corruption or OS purge. `IndexMode::Memory` stays available for embedders.
- Links are resolved at query time, so adding or renaming a doc never rewrites its referrers' rows. Today they are not stored at all (D9); the derived tables will store them unresolved, as normalised keys.
- Writes go in small committed batches; changes are published through `change_log`.
- Corruption: never rename or unlink the file. The reconciler (`quick_check` on open and on promotion, or a `Corrupt` error) builds a new generation `<db>-<gen8>.db` with a new random `meta.generation`, fills it, then repoints the registry's `db_file`; GC deletes the old file once `<id>.open` can be taken exclusively. A peer of a corrupt file serves from memory meanwhile, so `open_for` never fails on a corrupt cache.
- Every process re-reads the registry row on `refresh` and `refresh_paths` and reopens when `db_file` names another file. Planned: a per-query check of `data_version` and the DB inode, to catch cache purges between refreshes.
- Root moves: `files.path` is root-relative. On a registry miss, a DB whose stored marker inode (or volume UUID + marker inode) matches the new root is re-keyed instead of rebuilt.
- The [FTS5](https://sqlite.org/fts5.html) table mirrors each note's content in the same transaction that writes it, reconciler only. Batch size and page cache are bounded to stay under the RSS target.
- GC (at most daily per cache dir): stale generations, DB files of roots without a registry row, old schema versions 7 days old, roots unseen for 30 days or gone, then least recently seen roots over a 1 GiB budget. Skips any root whose `.open` is held. Lock files are never deleted (D3).

**Freshness rule** (single writer, so no fence between writers; details in [specs/index.md](specs/index.md))
- Version = the `fstat` of the fd the content was read from: stat, read, `fstat`; retry if the stats differ.
- Change detection by `(ino, ctime_ns, size)`. Mtime is stored but not compared (a future cheap scan may use it as a "maybe changed" hint), because `cp -p`, `rsync -a`, `tar x` and restores set old mtimes, while `utimes` cannot set ctime.
- Content equality replaces a hash, because the DB stores the bytes (D9): new bytes are written with a `change_log` entry; the same bytes with a new stat (`touch`) update only the stat columns. There is no `parser_ver`: every process parses the stored content, so a parser upgrade needs no re-index.

**Why**
- Parse speed alone does not justify a DB: walk + parse takes 104–129 ms on the ~730-note vault and 12–36 ms on the ~210-note vault, under the 250 ms cold target (pulldown-cmark ~1.1 GB/s).
- Warm start < 30 ms to first result: an in-memory server must open every file at start, ~100 µs per open on macOS, so ~2 s at 20k files before parsing.
- N editors share one cache instead of N cold reads; with the derived tables (D9) they also stop holding N in-memory copies. marksman re-parses every start: ~610–840 ms to first result, 134–145 MB RSS. zk answers in 22–43 ms at 32–35 MB from its DB.
- FTS is too large to rebuild per process. A peer, whose DB may lag the text it serves, scans its in-memory notes with the same matching rules instead.
- Estimated size (unmeasured): metadata ~1 MB per 1k notes; FTS5 with external content ~1× source size. For comparison a clean `zk index` takes 2.2–3.4 s at 42–48 MB RSS.

**Rejected**
- In-memory only (marksman, iwe, markdown-oxide): cold every start, memory per process. iwe's "20k notes < 1 s" is a vendor claim, not reproduced.
- Index inside the user tree (zk `.zk/notebook.db`): pollutes repos, needs an init step, breaks on read-only or virtual trees.
- One global DB: WAL allows one writer per DB, so all roots serialise; `ATTACH` is capped at 10, so splitting later is costly.
- Mtime-based upsert fence (`accept iff mtime >=`): rejects restored files forever, ignores `parser_ver` bumps, re-parses on `touch`.

## D5. Index lives in the user cache dir with a local-FS fallback chain

**Decision** — the base dir is the first entry that is local and writable:

| # | Path | When |
|---|---|---|
| 1 | `$XDG_CACHE_HOME/mdroots` | if set (any OS) |
| 1 | `~/Library/Caches/mdroots` / `~/.cache/mdroots` | macOS / Linux default |
| 2 | `$XDG_RUNTIME_DIR/mdroots` | 1 not local or not writable |
| 3 | `/var/tmp/mdroots-$UID` | 2 unset or unusable; created `0700`, owner checked |
| 4 | in memory for the session | nothing usable |

- "Local" = `statfs` reports `MNT_LOCAL`, with fs-type name matching as fallback (same test as root classification). flock and WAL are used only there, because both are unsafe on NFS/SMB.
- A cache dir or registry that cannot be opened leaves the session in memory instead of failing; a corrupt per-root DB is rebuilt (D4). Dropping to memory on a full disk at write time is not built.
- OS purges and cleared runtime/tmp dirs are treated like deletion: peers reopen (D4), the next reconciler rebuilds. Cost: one cold start.
- `Options::index(IndexMode::Memory)` never touches the cache dir (in-memory index), for read-only embedders. `Options::cache_dir(path)` (the CLI's `MDROOTS_CACHE_DIR`) replaces the chain, for tests.
- Processes with different environments may pick different base dirs; each gets its own reconciler. `mdroots roots` prints the root's DB path (so the base dir) and the process's role; `cache_dir` also returns why each candidate was taken or rejected.

**Why**
- The DB is rebuildable, so by XDG it is cache, not state.
- `~/Library/Caches` is excluded from Time Machine; `~/.local/state` is not, so every reconcile would churn backups.
- The `etcetera` crate's Apple strategy returns no state dir.
- NFS homes, read-only dirs and full disks must still work, just without a persistent index on the remote FS.

**Rejected**
- `$XDG_STATE_HOME`: backed up, wrong XDG category, no macOS mapping.
- `~/.cache` on macOS: not excluded from backups, not where macOS tools look. Users who want it set `XDG_CACHE_HOME`.
- In-tree (like zk): see D4.

## D6. Crate graph and publishing policy

**Decision**

```
syntax ← resolve ← core ← roots ← index ← mdroots ← mdroots-lsp ← mdroots-cli
```

Each crate may also use any crate to its left directly (`mdroots-cli` uses `mdroots` and `mdroots-lsp`, the latter for `mdroots lsp`).

| Crate | Holds |
|---|---|
| `mdroots-syntax` | parse one document → structure + link candidates, `LineIndex`; no I/O; builds for wasm |
| `mdroots-resolve` | resolution ladder, dialect detection, convention vote; defines `ResolveEnv` (existence, case sensitivity, home dir), so no direct I/O; builds for wasm |
| `mdroots-core` | `FileSystem`, `MemStore` (the in-memory index), the markdown walk, the diagnostics policy, `Cancel`; implements `ResolveEnv` |
| `mdroots-roots` | root discovery, the `Registry` trait, `list_root` |
| `mdroots-index` | cache dir choice, flock roles, the per-root DB, reconcile, the SQLite root registry |
| `mdroots` | the facade embedders depend on |
| `mdroots-lsp` | protocol only; the only crate that names `lsp-types` |
| `mdroots-cli` | the `mdroots` binary, including `mdroots lsp` |

- **Independent semver** per crate, `cargo-semver-checks` per crate in CI.
- crates.io refuses path-only dependencies, so every `mdroots` release publishes its sub-crates. Until a sub-crate's API settles it is an **internal 0.x crate** (README: "no stability promise, depend on `mdroots`") and the facade pins it with `=x.y.z`. `mdroots-syntax` and `mdroots` are the first public crates.
- Feature flags on `mdroots` (`roots`, `index`, `fts`, `watch`, `parallel`, `org`, `serde`) each document what they pull in: C code, threads, `libc`. `default-features = false` gives `syntax + resolve + core + MemStore`: pure Rust, no threads, no discovery, wasm-capable. Without `index` there is no FTS (a naive scan fallback exists for small vaults). Table in [specs/library.md](specs/library.md).
- No `lsp-types` feature on the facade; public enums such as `Freshness` are `#[non_exhaustive]`, so examples need a `_` arm.

**Why**
- Wasm and linter users who want only the parser must not compile SQLite.
- `ResolveEnv` lives in `resolve` because `resolve` sits below `core`.
- Lockstep versions would force a major bump of `mdroots-syntax` whenever `mdroots-index` breaks.
- `lsp-types` conversions in the facade would tie `mdroots`'s major version to `lsp-types`'s.

**Rejected**
- One crate with feature-gated modules: recreates the crate split in `cfg` attributes, and parser-only users still pull SQLite unless every module is gated.
- Lockstep versioning: needless major bumps.

## D7. Liberal link model: context decides meaning, code is a mention, never diagnosed

**Decision**
- Two stages: pulldown-cmark classifies byte ranges (prose, heading, code, HTML, comment, math, frontmatter); a single-pass hand-written scanner then finds wiki, org (in `.md` too), md, ref, bare-path, URL, tag, footnote and templating links.
- Every candidate records its context. In **code** (fenced, indented, inline, math) a link is a *mention*: indexed and goto-able if it resolves, hidden from references by default, never completed, **never diagnosed**, never rewritten by rename (a code action offers "also update N mentions in code"). Comments behave the same.
- Confidence levels: *explicit* forms (md, wiki, org, ref, HTML href) may be diagnosed; *implicit* forms (bare paths, frontmatter plain values) become links only if the target exists and are otherwise dropped silently; *external* (URLs, `file:` outside the root, org `#+LINK` abbreviations, unknown `X:` prefixes) are never diagnosed.
- An explicit link whose target is not indexed but exists on disk (gitignored, hidden, pruned) is "unindexed": goto and hover work, no diagnostic.
- One resolution ladder for every feature; the first step with any hit stops it; ties go to the closest path and are flagged ambiguous. Partial (zk-style) matching is never used for diagnostics. Ladder in [specs/index.md](specs/index.md).
- Broken explicit links: warning by default; error if the root's existing config says so or > 98% of explicit links already resolve; hint if < 80% resolve. These thresholds are unvalidated.

**Why**
- Users link in every dialect, often mixed in one vault; the resolver accepts all of them instead of asking which.
- Links in code are useful to jump to (`[[note]]` in a README fence) but are often not links (`[[{{filename-stem}}]]`, Lean's `[[]]`).
- In the ~730-note vault, 2,421 of 2,423 path-like tokens were not files, hence the "only if it resolves" rule for bare paths. 194 of its links are org `#+LINK` abbreviations that would otherwise be false dead links; the ~210-note vault has 44 links to gitignored generated files.
- A vault full of red errors is the main reason people uninstall a zero-config tool.

**Rejected**
- Diagnosing everything that looks like a link: false errors in code, templates and generated folders.
- A per-root link-format setting (marksman `completion.wiki.style`, zk `link-format`): replaced by the vote in D8.

## D8. Zero config: conventions are detected and voted, existing tool configs are read, never written

**Decision**
- No config file and no init step. Roots are discovered from markers ([specs/roots.md](specs/roots.md)).
- Dialects are detected from markers (`.zk/`, `.obsidian/`, `.marksman.toml`, `.foam/`, `dendron.yml`, `logseq/`, org files, mkdocs/Hugo/Docusaurus/Jekyll/mdBook configs, ...). Several markers in one tree are merged, not ranked.
- Existing tool configs are **read** for hints (zk link format, tag syntaxes and `dead-link` severity; [Obsidian](https://obsidian.md) link style and attachment folder; marksman title settings). mdroots never writes them, and never reads or writes zk's `notebook.db`.
- A corpus vote, stored in `meta` and recomputed after each full reconcile, decides: completion insert style (share of links per ladder step), piped-wiki order, wiki vs md, `.md` suffix, tag syntaxes (≥ 3 distinct tags in ≥ 2 files), H1-as-title (≥ 70% of docs have exactly one H1), filename scheme.

**Why**
- Goal: install and forget. zk needs `zk init`; marksman needs a VCS marker or `.marksman.toml` and offers config to pick a style.
- Reading an existing config is still zero config for the user; writing one would touch user trees, often [git](https://git-scm.com) repos.
- The vote picks the style the vault already uses: root-relative paths in the ~730-note vault, stems in the ~210-note vault.

**Rejected**
- A required config file or init step (zk): the main friction zero config removes.
- Writing detected settings back to tool configs: modifies user files (see D2).

## D9. The per-root DB caches content; queries run on an in-memory index

**Decision**
- Each `files` row of the per-root DB (D4) holds a note's root-relative path, its stat (`ino`, `ctime_ns`, `mtime_ns`, `size`) and its **bytes**; an FTS5 table mirrors the bytes for full-text search. There are no derived SQL tables (keys, links, frontmatter).
- Every process hydrates an in-memory `MemStore` from the DB's bytes (`MemStore::from_contents`) and answers every query from memory, except the reconciler's `full_text`, which reads the FTS table. Whole-root results (the diagnostics policy's resolved share, the backlink index) are computed once per store and dropped on the next content change. An unchanged note is never opened: it is re-read only when its `(ino, ctime_ns, size)` differs from its row, or it has no row.
- Content equality replaces the spec's `hash`, because the bytes are stored anyway. A read with new bytes is an upsert plus a `change_log` entry; the same bytes with a new stat (`touch`) update only the stat columns. A parser upgrade needs no re-index, since every process parses the stored content.
- The reconciler (D3) re-lists the root per its mode on open and on `Workspace::refresh` (budgeted walk, git index scan, enumerator, or the lazy working set), re-stats every file and writes what changed in `BEGIN IMMEDIATE` batches of at most 200 files or 50 ms. A peer never lists: it uses the DB's file set, re-stats it and reads changed files into memory only.
- Changes on disk arrive through `refresh` (which `mdroots lsp` calls on `didSave`, and on `didChangeWatchedFiles` for workspaces that do not watch) and through the reconciler's opt-in native watcher, which point-refreshes changed paths with `refresh_paths` and patches the store in place.
- `Workspace::open_at` (a directory chosen by the caller) and single-file decisions stay in memory; only discovered, registered roots get a DB.

**Why**
- It gives the warm-start win of D4 (no file opens for unchanged notes) with a small schema and no query layer to keep in sync with the parser; the in-memory layer already existed (M1–M3).
- Storing bytes makes change detection exact without a hash and makes parser changes free.
- Measured (release build, macOS APFS): an 11-note vault checks in 0.32 s cold and under 0.01 s warm at 2.2 MB peak footprint. On a synthetic 3,000-note notebook (12 MB), `mdroots check` on one note takes 0.6–0.7 s with a fresh cache and 0.11 s with the DB present, at 25.5 MB and 30.5 MB peak footprint; whole-root queries in memory (`bench_root`: diagnostics for every file 46–65 ms, backlinks for 100 files 11 ms) peak at 24.1 MB.

**Costs accepted**
- Every process holds every note's bytes and parse in memory, so memory grows with the vault. The 3,000-note notebook stays under the 35 MB per-process target (D3), with little margin; see [OPEN-QUESTIONS.md](OPEN-QUESTIONS.md).
- Hydrating parses every note at start; a peer sees another process's writes only on its next `refresh`.

**Deferred to M7** (decided in M6, with these numbers):
- **Derived tables** (`keys`, `links`, `frontmatter` with indexed lookups, [specs/index.md](specs/index.md) §1.2). Before M6 opening the 3,000-note notebook peaked at 37 MB with or without the DB, against the 35 MB target. M6 cached whole-root results and stopped copying each note's text into its line index (28.3 → 24.1 MB on the whole-root bench); every measured path now peaks at 23–31 MB. The target vaults (~730 and ~210 notes) are far below that size, so a second query layer kept in sync with the parser is not worth it yet.
- **FSEvents replay** (`sinceWhen`). [notify](https://crates.io/crates/notify) cannot set it, and the alternatives (`fsevent-sys`, direct FFI) need unsafe code, which the workspace forbids (`unsafe_code = "forbid"`). Offline changes are already caught by the re-list and re-stat on open.
- **Cross-root `ATTACH`, a background open, [Watchman](https://facebook.github.io/watchman/) clocks**: no measured need yet.

**Rejected**
- Building the derived tables in M4: a second query layer before the first one was measured.
- Hash column next to the content: redundant with stored bytes.
