# Decisions

The design choices behind mdroots that are hard to reverse: the rule, why,
and what was rejected. Mechanics live in the specs:
[roots](specs/roots.md), [index](specs/index.md), [library](specs/library.md).
What is not built, not validated or still open is in
[ROADMAP.md](ROADMAP.md).

Measurements come from two testbed vaults: a ~730-note
[zk](https://github.com/zk-org/zk) vault and a ~210-note research vault
([index spec §0](specs/index.md#0-testbed)).

## D1. Name mdroots

**Decision**
- The name is `mdroots`, after the core idea: zero-config discovery of note roots.
- Crates: `mdroots`, `mdroots-cli`, `mdroots-syntax`, `mdroots-resolve`,
  `mdroots-core`, `mdroots-roots`, `mdroots-index`, `mdroots-lsp`. The cache
  dir and the `.mdroots` and `.mdrootsignore` file names follow the name.
- No short alias until a candidate passes the checks below (`mw` and `mdr`
  are taken).

**Why**
- Requirements: contains md/mark/down, free on crates.io, no clash with common commands.
- Namespace check: every crate name free on crates.io; no PATH command,
  Homebrew formula, PyPI or Debian package, GitHub repo or org, or software
  found by web search. `mdroots.com` is registered, `mdroots.org` is
  free. Open checks and the alias search: [ROADMAP §4](ROADMAP.md#4-open-questions).

**Rejected**
- `mdls`: clashes with macOS `/usr/bin/mdls` (Spotlight), on every Mac's PATH.
- `markweft`: used by a markdown editor and a Rust markdown converter.
- `markloom`, `markwell`, `markroot`, `markmesh`, `markway`: existing apps or markdown tools.
- `markhound`: live US trademark.

## D2. Library first, LSP is a thin adapter

**Decision**
- mdroots is a Rust library. `mdroots lsp`, the CLI (the first embedder)
  and others (TUIs, SSGs, MCP servers, language servers) call the same API,
  `mdroots::Workspaces`.
- Synchronous core, no async runtime, nothing polls: the embedder calls
  `Workspace::refresh` or `refresh_paths`. The only background thread is
  the opt-in native watcher (`Options::watch(true)`, set by `mdroots lsp`),
  which calls `refresh_paths`.
- Slow calls (`refresh`, `refresh_paths`, `full_text`, `diagnostics`,
  `rename_note`, `extract_note`) take `&Cancel`.
- Each query sees one consistent state: the in-memory index (D9) plus
  overlays. A refresh swaps in a new index, so a change is never seen
  halfway.
- `open_for` is synchronous and budgeted. An embedder that must not block
  serves the file with `Workspace::open_single` and runs `open_for` on its
  own thread, as `mdroots lsp` does
  ([library spec §3.6](specs/library.md#36-embedding-the-server)).
- The library never writes user files. Refactors return a `WorkspaceEdit`
  (D10).
- Extension traits (`FileSystem`, `Probe`, `Enumerator`, `ResolveEnv`) are
  `Send + Sync`. The DB connection sits behind a mutex
  ([rusqlite](https://github.com/rusqlite/rusqlite)'s `Connection` is
  `!Sync`); queries never touch it (D9).
- A feature that can't be tested without JSON-RPC is in the wrong crate.

**Why**
- [marksman](https://github.com/artempyanykh/marksman) (F#, exe-only) and
  zk (Go `internal/` packages) can't be reused as libraries; embedders need one.
- Queries take microseconds to milliseconds; async would add nothing but a
  forced runtime. Async callers use `spawn_blocking`.

**Rejected**
- Build on [iwe](https://github.com/iwe-org/iwe)'s `liwe`: in-memory arena
  (cold every start, one copy per process), needs `.iwe` config, API
  explicitly unstable.
- Async core on [tower-lsp](https://github.com/ebkalderon/tower-lsp): forces
  a runtime on embedders for no gain.
- (One crate with feature-gated modules: see D6.)

## D3. In-process, no daemon; one writer per root

**Decision**
- mdroots always runs inside the embedding process. No daemon, RPC, sockets
  or remote backend.
- Per root, `flock(LOCK_EX|LOCK_NB)` on the root's `.lock` file elects a
  **reconciler**, the only process that writes the root's DB (in
  `BEGIN IMMEDIATE` transactions), keeps the FTS table and acts on native
  watcher events.
- Every other process is a **read-only peer**: it serves the DB plus its
  overlays and acts only on its own `didOpen`/`didChange`/`didSave`, so a
  save is not parsed N times. On each `refresh` it picks up others' changes
  (D9) and retries the lock.
- Crash recovery is the kernel releasing the flock (also on SIGKILL). No
  heartbeats, PID files, stale-lock cleanup or liveness detection: a stuck
  reconciler keeps the lock while peers keep answering. A new reconciler's
  re-list finds changes made while nobody watched.
- Lock files are never unlinked: a waiter on an unlinked lock would elect
  a second reconciler. A shared lock on the root's `.open` file guards open
  DB files, so GC and rebuilds never unlink a DB in use (a documented
  [SQLite](https://sqlite.org) corruption path).
- Locks use `std::fs::File::try_lock` (Rust ≥ 1.89). Rust opens files
  `O_CLOEXEC`, so children don't inherit locks; a second fd in the same
  process is refused (that process becomes a peer); flock does not conflict
  with SQLite's fcntl locks.
- Herd start (ten editors at once): one process walks under `discover.lock`,
  one wins the root flock, nine serve the existing index.
- Files, roles, stuck reconcilers and herd rules:
  [roots spec §5](specs/roots.md#5-many-processes-on-one-root).

**Why**
- Many editors run at once (tmux, session restore), often short-lived or
  killed.
- A single writer removes write races and their freshness-fence problems
  (D4); a peer's own edits are already fresh in its overlay.
- A daemon would save the per-process index (D9) and duplicate parsing,
  but persistent state already lives in SQLite.
  [gopls](https://go.dev/gopls)'s daemon saves hundreds of MB and is still
  off by default.
- What makes gopls fast (snapshots, cancellation, in-order requests, a
  persistent cache) needs no daemon ([research/gopls.md](research/gopls.md)).
- A daemon would still need schema-versioned DB files and GC locks (old
  and new daemons overlap during upgrades), and embedders would need a
  running daemon or a second backend.

**Costs accepted**
- Other processes' saves reach a peer on its next `refresh`.
- Memory scales with editors. Target: < 35 MB `phys_footprint` per process
  with 10 running ([ROADMAP §1](ROADMAP.md#1-built-but-not-validated)).

**Revisit only with numbers**: a new decision only if a 10-editor
measurement shows memory or duplicate work is a real problem. No daemon
code exists.

**Rejected**
- Per-user daemon (gopls `-remote=auto` style): socket lifecycle, versioned
  private RPC mirroring the whole API, environment handover, one crash
  domain for all editors; small gain.
- Daemon with in-process fallback: every mechanism of both designs.
- Any process may write point updates: write races and fence bugs, no
  visible gain.
- Every process indexes independently: wasted CPU, N copies in memory.

## D4. One disposable SQLite DB per root

**Decision**
- One SQLite DB (WAL) per root, `roots/<id>.v<schema>.db`, plus a registry
  `roots.v<k>.db`. Schema versions are in the filenames, so old and new
  binaries never share a file or fight over migrations. Row contents: D9.
- The DB is a **cache**: correctness never depends on it. It is rebuilt on
  schema change, corruption or OS purge. `IndexMode::Memory` stays
  available for embedders.
- Links are resolved at query time, never stored (D9), so adding or
  renaming a doc never rewrites its referrers' rows.
- Writes go in small committed batches, published through `change_log`;
  the [FTS5](https://sqlite.org/fts5.html) table is updated in the same
  transaction.
- A corrupt file is never renamed or unlinked: the reconciler builds a new
  generation and `open_for` never fails on a corrupt cache
  ([index spec §1.6](specs/index.md#16-failures-and-races)).
- `files.path` is root-relative, so a moved root's DB is re-keyed, not
  rebuilt ([roots spec §1](specs/roots.md#stage-1-registry-lookup)).
- Change detection by `(ino, ctime_ns, size)` plus content equality, not
  mtime ([index spec §1.2](specs/index.md#12-schema)).
- GC runs at most daily and never deletes lock files or an open DB
  ([index spec §1.7](specs/index.md#17-files-and-connections)).

**Why**
- Warm start < 30 ms to first result: an in-memory server opens every file
  at start, ~100 µs each on macOS, so ~2 s at 20k files before parsing;
  parse speed alone would not justify a DB.
- N editors share one cache instead of N cold reads (marksman re-parses on
  every start, zk answers from its DB;
  [index spec §0](specs/index.md#0-testbed) baselines).
- Mtime is not compared: `cp -p`, `rsync -a`, `tar x` and restores set old
  mtimes, while `utimes` cannot set ctime.
- FTS is too large to rebuild per process; a peer, whose DB may lag its
  text, scans its in-memory notes with the same matching rules.

**Rejected**
- In-memory only (marksman, iwe,
  [markdown-oxide](https://github.com/Feel-ix-343/markdown-oxide)): cold
  every start, memory per process. iwe's "20k notes < 1 s" is a vendor
  claim, not reproduced.
- Index inside the user tree (zk `.zk/notebook.db`): pollutes repos, needs
  init, breaks on read-only or virtual trees.
- One global DB: one WAL writer, so all roots serialise; `ATTACH` is capped
  at 10, so splitting later is costly.
- Mtime-based upsert fence (`accept iff mtime >=`): rejects restored files
  forever and re-parses on `touch`.

## D5. Cache dir with a local-FS fallback chain

**Decision**: the base dir is the first entry that is local and writable:

| # | Path | When |
|---|---|---|
| 1 | `$XDG_CACHE_HOME/mdroots` | if set (any OS) |
| 1 | `~/Library/Caches/mdroots` / `~/.cache/mdroots` | macOS / Linux default |
| 2 | `$XDG_RUNTIME_DIR/mdroots` | 1 not local or not writable |
| 3 | `/var/tmp/mdroots-$UID` | 2 unset or unusable; created `0700`, owner checked |
| 4 | in memory for the session | nothing usable |

- "Local" = `statfs` reports `MNT_LOCAL`, with fs-type name matching as
  fallback (the root-classification test). flock and WAL are used only
  there; both are unsafe on NFS/SMB.
- An unopenable cache dir or registry leaves the session in memory; a
  corrupt per-root DB is rebuilt (D4). OS purges count as deletion: one
  cold start.
- `IndexMode::Memory` never touches the cache dir; `Options::cache_dir`
  replaces the chain ([index spec §1.7](specs/index.md#17-files-and-connections)).
- Processes with different environments may pick different base dirs, each
  with its own reconciler; `mdroots roots` shows the DB path and role, and
  `cache_dir` says why each candidate was taken or rejected.

**Why**
- The DB is rebuildable, so by XDG it is cache, not state.
- `~/Library/Caches` is excluded from Time Machine; `~/.local/state` is
  not, so every reconcile would churn backups.
- The [etcetera](https://crates.io/crates/etcetera) crate's Apple strategy
  returns no state dir.
- NFS homes, read-only dirs and full disks must still work, without a
  persistent index on the remote FS.

**Rejected**
- `$XDG_STATE_HOME`: backed up, wrong XDG category, no macOS mapping.
- `~/.cache` on macOS: backed up, not where macOS tools look; set
  `XDG_CACHE_HOME` to use it.
- In-tree (like zk): see D4.

## D6. Crate graph and lockstep versions

**Decision**

```
syntax ← resolve ← core ← roots ← index ← mdroots ← mdroots-lsp ← mdroots-cli
```

A crate may also use any crate to its left. What each crate holds:
[library spec §2](specs/library.md#2-workspace-layout).

- **One version for all crates**, set once in `[workspace.package] version`;
  every release publishes every crate.
- **Two public crates**: `mdroots` (library) and `mdroots-cli` (binary).
  The other `mdroots-*` crates are internal: no stability promise (their
  READMEs say depend on `mdroots`), pinned by every dependent to the exact
  version (`=x.y.z`), and published only because crates.io refuses
  path-only dependencies.
- No feature flags: `mdroots` always builds discovery, the SQLite index
  (with FTS5) and the watcher. A lighter build is deferred
  ([ROADMAP §3](ROADMAP.md#3-deferred-work)).
- No `lsp-types` feature on the facade; `mdroots-lsp` is the only crate
  that names [lsp-types](https://crates.io/crates/lsp-types). Public enums
  such as `Freshness` are `#[non_exhaustive]`, so examples need a `_` arm.

**Why**
- One version gives one changelog and one compatibility statement; an
  embedder tracks one crate.
- Internal crates have no outside users, so their APIs change in any
  release; exact pins mean a published `mdroots` only builds against the
  sub-crates it was tested with.
- The split keeps the parser free of SQLite and I/O, so a parser-only offer
  stays cheap.
- `ResolveEnv` lives in `resolve` because `resolve` sits below `core`.
- `lsp-types` conversions in the facade would tie `mdroots`'s major version
  to `lsp-types`'s.

**Rejected**
- Independent semver per crate: a release and compatibility check per crate
  for APIs nobody outside depends on (whether a sub-crate should become
  public is open: [ROADMAP §4](ROADMAP.md#4-open-questions)).
- One crate with feature-gated modules: recreates the split in `cfg`
  attributes; parser-only users still pull SQLite unless every module is
  gated.

## D7. Liberal link model

Context decides meaning; a link in code is a mention and never diagnosed.

**Decision**
- Two stages: [pulldown-cmark](https://github.com/pulldown-cmark/pulldown-cmark)
  classifies byte ranges, then a hand-written single-pass scanner finds
  every link form, org links in `.md` included
  ([index spec §2.1](specs/index.md#21-structure-first-then-a-liberal-scan)).
- In **code** (fenced, indented, inline, math) and comments a link is a
  *mention*: goto-able if it resolves, but never a backlink or reference,
  **never diagnosed**, never rewritten by rename.
- Only *explicit* forms (md, wiki, org, ref, HTML href) may be diagnosed;
  *implicit* forms (bare paths, plain frontmatter values) are links only if
  the target exists; *external* forms are never diagnosed
  ([index spec §2.3](specs/index.md#23-confidence-levels)). An existing
  bare path in prose (`see notes/b.md`) is a full link (backlink, graph
  edge, `notes -l`/`-L` hit); zk does not count these, so mdroots finds
  more links than zk by design.
- An explicit link to a file on disk but not indexed (gitignored, hidden,
  pruned) is "unindexed": goto and hover work, no diagnostic.
- One resolution ladder for every feature; the first step with a hit stops
  it; ties go to the closest path, flagged ambiguous. Partial (zk-style)
  matching never feeds diagnostics
  ([index spec §2.4](specs/index.md#24-resolution-ladder)).
- Broken explicit links: warning by default; error if the root's config
  says so or > 98% of explicit links resolve; hint if < 80% resolve
  ([index spec §3.3](specs/index.md#33-diagnostics); thresholds unvalidated,
  [ROADMAP §1](ROADMAP.md#1-built-but-not-validated)).

**Why**
- Users mix dialects, even in one vault; the resolver accepts all of them
  instead of asking.
- Links in code are worth jumping to (`[[note]]` in a README fence) but
  are often not links (`[[{{filename-stem}}]]`, Lean's `[[]]`).
- Bare paths count only if they resolve: in the ~730-note vault, 2,421 of
  2,423 path-like tokens were not files.
- A vault full of red errors is why people uninstall a zero-config tool.

**Rejected**
- Diagnosing everything that looks like a link: false errors in code,
  templates and generated folders.
- An mdroots link-format setting (like marksman `completion.wiki.style`):
  an existing tool config (zk `link-format`, [Obsidian](https://obsidian.md)
  `useMarkdownLinks`/`newLinkFormat`) wins for inserted links, else the
  vote (D8).

## D8. Zero config

Conventions are detected and voted; existing tool configs are read, never
required or written.

**Decision**
- No mdroots config file and no init step. Roots are discovered from
  markers ([roots spec §1](specs/roots.md#1-discovery-stages-cheap-to-expensive)).
- Dialects are detected from markers, merged, not ranked
  ([index spec §3.1](specs/index.md#31-markers)).
- A tool's config is read only when its marker is present and a setting
  changes an answer (zk's link format, tag syntaxes and `dead-link`
  severity, Obsidian's link style, a [MkDocs](https://www.mkdocs.org) docs
  dir); a missing or malformed one falls back to the marker's defaults and
  the vote. mdroots never writes a tool config, and never touches zk's
  `notebook.db`.
- [git](https://git-scm.com)'s files (`.git/config`) and ignore files
  (`.gitignore`, `.ignore`, `.mdrootsignore`) are not tool configs and are
  always read.
- Settings are visible: `mdroots roots` prints the link style, tag syntaxes,
  broken-link severity (and docs dir when set), each with its source, so a
  stale config is easy to spot.
- A per-process corpus vote over the in-memory index measures the root's
  conventions. Inserted links (`Workspace::link_to`, extract-note) follow a
  zk config, then an Obsidian config, then the vote, then the marker's
  default ([index spec §3.2](specs/index.md#32-vote-and-link-style)).

**Why**
- Install and forget. zk needs `zk init`; marksman needs a VCS marker or
  `.marksman.toml` and a config to pick a style.
- Reading an existing config is still zero config: it honours settings the
  user already made. Writing one would touch user trees, often git repos.
- The vote picks the style the vault already uses: root-relative paths in
  the ~730-note vault, stems in the ~210-note vault.

**Rejected**
- A required config file or init step (zk): the friction zero config removes.
- Ignoring tool configs: a setting the user made (say zk
  `dead-link = "none"`) would silently not apply.
- Writing detected settings back to tool configs: modifies user files (D2).

## D9. The DB caches content; queries run in memory

**Decision**
- Each `files` row (D4) holds a note's root-relative path, stat (`ino`,
  `ctime_ns`, `mtime_ns`, `size`) and **bytes**; an FTS5 table mirrors the
  bytes. No derived SQL tables
  ([index spec §1.2](specs/index.md#12-schema)).
- Every process hydrates an in-memory `MemStore` from the stored bytes
  (`MemStore::from_contents`) and answers every query from memory, except
  the reconciler's `full_text`, which reads the FTS table. Whole-root
  results are cached per store until the next content change. An unchanged
  note is never opened: it is re-read only when its `(ino, ctime_ns, size)`
  differs from its row, or it has no row.
- Content equality instead of a hash. A parser upgrade needs no re-index,
  because every process parses the stored content.
- The reconciler (D3) re-lists, re-stats and writes changes on open and on
  `refresh`; a peer only re-stats the DB's file set and reads changes into
  memory. Changes also arrive through the opt-in watcher
  ([index spec §1.3](specs/index.md#13-finding-what-changed)).
- `Workspace::open_at` (a caller-chosen directory) and single-file
  decisions stay in memory; only discovered, registered roots get a DB.

**Why**
- D4's warm-start win (no file opens for unchanged notes) with a small
  schema and no query layer to keep in sync with the parser.
- Stored bytes make change detection exact without a hash and parser
  changes free.
- Measured: on a synthetic 3,000-note notebook, `mdroots check` on one note
  takes 0.6–0.7 s with a fresh cache, 0.11 s with the DB present
  ([index spec §1.5](specs/index.md#15-short-lived-instances)).

**Costs accepted**
- Every process holds every note's bytes and parse, so memory grows with
  the vault: 3,000 notes stay under the 35 MB target (D3) with little
  margin; where that stops holding is open
  ([ROADMAP §4](ROADMAP.md#4-open-questions)).
- Hydrating parses every note at start; a peer sees another process's
  writes only on its next `refresh`.

**Deferred** ([ROADMAP §3](ROADMAP.md#3-deferred-work)):
- **Derived tables** (`keys`, `links`, `frontmatter`): every measured path
  stays under 35 MB on 3,000 notes and target vaults are far smaller, so a
  second query layer is not worth it yet.
- **FSEvents replay** (`sinceWhen`): [notify](https://crates.io/crates/notify)
  cannot set it; `fsevent-sys` or direct FFI need unsafe code, which the
  workspace forbids (`unsafe_code = "forbid"`). The re-list on open
  already catches offline changes.
- **Cross-root `ATTACH`, [Watchman](https://facebook.github.io/watchman/)
  clocks**: no measured need.

**Rejected**
- Derived tables before the in-memory layer is measured to fall short.
- Hash column next to the content: redundant with stored bytes.

## D10. mdroots proposes edits; the editor applies them

**Decision**
- mdroots reads notes and returns edits. The editor applies them: text
  changes, new files and moves alike, saved or not. Nothing in mdroots
  (library, server, CLI) writes under a notebook; its only writes are
  its cache (D5).
- Every feature that changes notes returns a `WorkspaceEdit`: rename,
  extract note, quick fixes, link conversion, the link edits for a move
  someone else made (`willRenameFiles`).
- Within that line, a quick fix that creates a file (`Create note`) is
  never the preferred fix, so a one-key or bulk fix only edits text.
- Whole-notebook graph facts (orphans, missing backlinks) are commands
  returning lists, not per-note diagnostics.

**Why**
- One rule a user can check and a reviewer can grep: does mdroots write
  a user path? Undo, saving and conflict handling stay with the editor.
- Language servers for code work this way (rename, create-file fixes):
  the server proposes, the editor applies.
- Orphans and one-way links fire on most notes of a typical vault and
  change with every edit elsewhere; as diagnostics they are noise (D7).

**Rejected**
- A write path in the library or the server, even for "safe" fixes.
- Templates or frontmatter in created notes: note management.
- Orphan and missing-backlink diagnostics, even off by default.

**Deferred** ([ROADMAP §3](ROADMAP.md#3-deferred-work))
- `mdroots check --fix`: a CLI has no editor to apply edits, so it would
  be the first mdroots code to write notes; it waits for a user who asks.
