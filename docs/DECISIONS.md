# Decisions

The design choices behind mdroots that are hard to reverse: the rule, why,
and what was rejected. Mechanics live in the specs:
[roots](specs/roots.md), [index](specs/index.md), [library](specs/library.md).
What is not built, not validated or still open is in
[ROADMAP.md](ROADMAP.md).

Measurements below come from two testbed vaults: a ~730-note
[zk](https://github.com/zk-org/zk) vault and a ~210-note research vault
([index spec §0](specs/index.md#0-testbed)).

## D1. Name mdroots

**Decision**
- The name is `mdroots`, after the core idea: zero-config discovery of note roots.
- Crates: `mdroots`, `mdroots-cli`, `mdroots-syntax`, `mdroots-resolve`,
  `mdroots-core`, `mdroots-roots`, `mdroots-index`, `mdroots-lsp`. The cache
  dir and the `.mdroots` and `.mdrootsignore` file names follow the name.
- No short alias until a candidate passes the checks below (`mw` and `mdr`
  are taken; [ROADMAP §4](ROADMAP.md#4-open-questions)).

**Why**
- Requirements: contains md/mark/down, free on crates.io, no clash with common commands.
- Namespace check: crates.io free for every crate name; no PATH command,
  Homebrew formula, PyPI or Debian package; no GitHub repo or org; web
  search finds no software. `mdroots.com` is registered, `mdroots.org` is
  free. The checks still open are in [ROADMAP §4](ROADMAP.md#4-open-questions).

**Rejected**
- `mdls`: clashes with macOS `/usr/bin/mdls` (Spotlight), on every Mac's PATH.
- `markweft`: already used by a markdown editor and a Rust markdown converter.
- `markloom`, `markwell`, `markroot`, `markmesh`, `markway`: existing apps or markdown tools.
- `markhound`: live US trademark.

## D2. Library first, LSP is a thin adapter

**Decision**
- mdroots is a Rust library; `mdroots lsp`, the CLI and other embedders
  (TUIs, SSGs, MCP servers, other language servers) call the same API
  (`mdroots::Workspaces`) directly. The CLI is the first embedder.
- Synchronous core, no async runtime. By default no background thread:
  nothing polls, and the embedder calls `Workspace::refresh` or
  `refresh_paths` to pick up changes on disk. The only background thread is
  the opt-in native watcher (`Options::watch(true)`, set by `mdroots lsp`),
  which calls the same `refresh_paths`.
- Slow calls (`refresh`, `refresh_paths`, `full_text`, `diagnostics`,
  `rename_note`, `extract_note`) take `&Cancel`.
- Each query sees one consistent state: the process's in-memory index (D9)
  with overlays applied at call time. A refresh builds a new index and swaps
  it in, so a concurrent change is never seen halfway.
- `open_for` is synchronous and discovery is bounded by its budgets. An
  embedder that must not block serves the file alone with
  `Workspace::open_single` (no discovery, cache or registry) and runs
  `open_for` on its own thread, as `mdroots lsp` does
  ([library spec §3.6](specs/library.md#36-embedding-the-server)).
- The library never writes user files. Refactors return a `WorkspaceEdit`.
- Extension traits (`FileSystem`, `Probe`, `Enumerator`, `ResolveEnv`) are
  `Send + Sync`. A workspace holds its root's DB connection behind a mutex,
  because [rusqlite](https://github.com/rusqlite/rusqlite)'s `Connection` is
  `!Sync`; queries never touch it (D9).
- A feature that can't be tested without JSON-RPC is in the wrong crate.

**Why**
- [marksman](https://github.com/artempyanykh/marksman) (F#, exe-only) and
  zk (Go `internal/` packages) can't be reused as libraries; embedders need
  one.
- Queries are microsecond-to-millisecond lookups; async adds nothing and
  would force a runtime on every embedder. Async callers use `spawn_blocking`.

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
  **reconciler**: it runs the native watcher and keeps the FTS table. It is
  the only process that writes the root's DB.
- Every other process is a **read-only peer**: it serves queries from the DB
  plus its own overlays (unsaved buffers, and files it just saved, parsed in
  memory). Its saves reach the DB through the reconciler's watcher or next
  refresh, and it picks up the reconciler's commits on its own next
  `refresh` (D9).
- A peer tries the lock again on every `refresh` and becomes the reconciler
  if it is free.
- Crash recovery is the kernel releasing the flock (also on SIGKILL). No
  heartbeats, PID files or stale-lock cleanup.
- Only the reconciler acts on watcher events; peers act only on their own
  `didOpen`/`didChange`/`didSave`, so a save is not parsed N times.
- Lock files are never unlinked, not even by GC of a whole root, because a
  process waiting on an unlinked lock file would elect a second reconciler.
  A shared lock on the root's `.open` file guards every DB file a process
  has open: GC, schema cleanup and corruption rebuild take it exclusively
  first, so they never unlink a DB another process uses (a documented
  [SQLite](https://sqlite.org) corruption path). File names and roles:
  [roots spec §5](specs/roots.md#5-many-processes-on-one-root).
- Locks use `std::fs::File::try_lock` (Rust ≥ 1.89). Rust opens files
  `O_CLOEXEC`, so children don't inherit locks; a second fd in the same
  process is refused (that process becomes a peer); flock does not conflict
  with SQLite's fcntl locks.
- The reconciler's writes are `BEGIN IMMEDIATE` with `busy_timeout`,
  because a deferred read-then-write gets `SQLITE_BUSY` at once and ignores
  the timeout.
- A stuck reconciler (alive but SIGSTOPed, or hung on a network or virtual
  FS `stat`) keeps the lock; peers answer from the DB and their own reads
  without waiting on it. There is no liveness detection.
- Changes made while no reconciler watched are found by the re-list and
  re-stat the new reconciler runs on promotion (and every process runs on
  open).
- Herd start (ten editors at once): one process walks under `discover.lock`,
  one wins the root flock, nine serve from the existing index. A "lazy"
  verdict from the walk-rate check alone is not saved until a second
  measurement agrees, so a herd-slowed walk doesn't stick.

**Why**
- Many editors run at once (tmux, session restore); many processes are
  short-lived or killed.
- A single writer removes write races and the freshness-fence problems that
  come with them (see D4). A peer's own edits are already fresh in its
  overlay, so peer writes buy nothing visible.
- A daemon would save the per-process in-memory index (D9) and duplicate
  parsing, but the persistent state already lives in SQLite.
  [gopls](https://go.dev/gopls)'s daemon saves hundreds of MB and is still
  off by default.
- What makes gopls fast (snapshots, cancellation, in-order requests, a
  persistent cache) needs no daemon ([research/gopls.md](research/gopls.md)).
- A daemon would not remove schema-versioned DB files or GC locks (old and
  new daemons overlap during upgrades), and embedders would need a running
  daemon or a second backend.

**Costs accepted**
- Other processes' saves reach a peer on its next `refresh`.
- Memory scales with editors. Target: < 35 MB `phys_footprint` per process,
  measured with 10 running ([ROADMAP §1](ROADMAP.md#1-built-but-not-validated)).

**Revisit only with numbers.** If a 10-editor measurement shows
per-process memory or duplicate work is a real problem, write a new
decision. Until then no daemon code exists.

**Rejected**
- Per-user daemon (gopls `-remote=auto` style): socket lifecycle, versioned
  private RPC mirroring the whole API, environment handover, one crash
  domain for all editors; small gain.
- Daemon with in-process fallback: carries every mechanism of both designs.
- Any process may write point updates: write races and fence bugs for no
  user-visible gain.
- Every process indexes independently: wasted CPU and N copies in memory.

## D4. One disposable SQLite DB per root

**Decision**
- What a row holds, and how processes use it, is D9.
- One SQLite DB (WAL) per root, `roots/<id>.v<schema>.db`, plus a registry
  `roots.v<k>.db`. Schema versions are in the filenames, so old and new
  binaries never share a file or fight over migrations.
- The DB is a **cache**: correctness never depends on it. It is rebuilt on
  schema change, corruption or OS purge. `IndexMode::Memory` stays
  available for embedders.
- Links are resolved at query time and never stored (D9), so adding or
  renaming a doc never rewrites its referrers' rows.
- Writes go in small committed batches; changes are published through
  `change_log`.
- Corruption: never rename or unlink the file. The reconciler
  (`quick_check` on open and on promotion, or a `Corrupt` error) builds a
  new generation file, then repoints the registry's `db_file`; GC deletes
  the old file once nobody has it open. A peer of a corrupt file serves
  from memory meanwhile, so `open_for` never fails on a corrupt cache
  ([index spec §1.6](specs/index.md#16-failures-and-races)).
- Every process re-reads the registry row on `refresh` and `refresh_paths`
  and reopens when `db_file` names another file.
- Root moves: `files.path` is root-relative. On a registry miss, a DB whose
  stored volume id and marker inode match the new root is re-keyed instead
  of rebuilt.
- The [FTS5](https://sqlite.org/fts5.html) table mirrors each note's content
  in the same transaction that writes it, reconciler only.
- Change detection by `(ino, ctime_ns, size)` plus content equality, not
  mtime ([index spec §1.2](specs/index.md#12-schema)).
- GC runs at most daily per cache dir and never deletes lock files or a DB
  someone has open ([index spec §1.7](specs/index.md#17-files-and-connections)).

**Why**
- Warm start < 30 ms to first result: an in-memory server must open every
  file at start, ~100 µs per open on macOS, so ~2 s at 20k files before
  parsing. Parse speed alone would not justify a DB.
- N editors share one cache instead of N cold reads. marksman re-parses on
  every start; zk answers from its DB
  ([index spec §0](specs/index.md#0-testbed) has both baselines).
- Mtime is not compared because `cp -p`, `rsync -a`, `tar x` and restores
  set old mtimes, while `utimes` cannot set ctime.
- FTS is too large to rebuild per process. A peer, whose DB may lag the
  text it serves, scans its in-memory notes with the same matching rules
  instead.

**Rejected**
- In-memory only (marksman, iwe,
  [markdown-oxide](https://github.com/Feel-ix-343/markdown-oxide)): cold
  every start, memory per process. iwe's "20k notes < 1 s" is a vendor
  claim, not reproduced.
- Index inside the user tree (zk `.zk/notebook.db`): pollutes repos, needs
  an init step, breaks on read-only or virtual trees.
- One global DB: WAL allows one writer per DB, so all roots serialise;
  `ATTACH` is capped at 10, so splitting later is costly.
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
  fallback (same test as root classification). flock and WAL are used only
  there, because both are unsafe on NFS/SMB.
- A cache dir or registry that cannot be opened leaves the session in
  memory instead of failing; a corrupt per-root DB is rebuilt (D4).
- OS purges and cleared runtime/tmp dirs are treated like deletion: the next
  reconciler rebuilds. Cost: one cold start.
- `Options::index(IndexMode::Memory)` never touches the cache dir, for
  read-only embedders. `Options::cache_dir(path)` (the CLI's
  `MDROOTS_CACHE_DIR`) replaces the chain, for tests.
- Processes with different environments may pick different base dirs; each
  gets its own reconciler. `mdroots roots` prints the root's DB path (so the
  base dir) and the process's role; `cache_dir` also returns why each
  candidate was taken or rejected.

**Why**
- The DB is rebuildable, so by XDG it is cache, not state.
- `~/Library/Caches` is excluded from Time Machine; `~/.local/state` is
  not, so every reconcile would churn backups.
- The [etcetera](https://crates.io/crates/etcetera) crate's Apple strategy
  returns no state dir.
- NFS homes, read-only dirs and full disks must still work, just without a
  persistent index on the remote FS.

**Rejected**
- `$XDG_STATE_HOME`: backed up, wrong XDG category, no macOS mapping.
- `~/.cache` on macOS: not excluded from backups, not where macOS tools
  look. Users who want it set `XDG_CACHE_HOME`.
- In-tree (like zk): see D4.

## D6. Crate graph and lockstep versions

**Decision**

```
syntax ← resolve ← core ← roots ← index ← mdroots ← mdroots-lsp ← mdroots-cli
```

Each crate may also use any crate to its left directly (`mdroots-cli` uses
`mdroots` and `mdroots-lsp`, the latter for `mdroots lsp`).

| Crate | Holds |
|---|---|
| `mdroots-syntax` | parse one document → structure + link candidates, `LineIndex`; no I/O |
| `mdroots-resolve` | resolution ladder, dialect detection, convention vote; defines `ResolveEnv` (existence, case sensitivity, home dir, config reads), so no direct I/O |
| `mdroots-core` | `FileSystem`, `MemStore` (the in-memory index), the markdown walk, the diagnostics policy, `Cancel`; implements `ResolveEnv` |
| `mdroots-roots` | root discovery, the `Registry` trait, `list_root` |
| `mdroots-index` | cache dir choice, flock roles, the per-root DB, reconcile, the SQLite root registry |
| `mdroots` | the facade embedders depend on |
| `mdroots-lsp` | protocol only; the only crate that names [lsp-types](https://crates.io/crates/lsp-types) |
| `mdroots-cli` | the `mdroots` binary, including `mdroots lsp` |

- **One version for all crates**, set once in the workspace
  (`[workspace.package] version`); every release publishes every crate.
- **Two public crates**: `mdroots` (the library) and `mdroots-cli` (the
  binary). The other `mdroots-*` crates are internal: each README says "no
  stability promise, depend on `mdroots`", and every dependent pins them
  to the exact version (`=x.y.z`). They are published only because
  crates.io refuses path-only dependencies.
- No feature flags: `mdroots` always builds discovery, the SQLite index
  (with FTS5) and the watcher. A lighter build is deferred
  ([ROADMAP §3](ROADMAP.md#3-deferred-work)).
- No `lsp-types` feature on the facade; public enums such as `Freshness`
  are `#[non_exhaustive]`, so examples need a `_` arm.

**Why**
- One version gives one changelog and one compatibility statement; an
  embedder tracks one crate.
- Internal crates have no outside users, so their APIs change in any
  release without a semver process of their own; the exact pins make sure
  a published `mdroots` only ever builds against the sub-crates it was
  tested with.
- The split still keeps the parser free of SQLite and I/O, so a
  parser-only offer stays cheap to add later.
- `ResolveEnv` lives in `resolve` because `resolve` sits below `core`.
- `lsp-types` conversions in the facade would tie `mdroots`'s major version
  to `lsp-types`'s.

**Rejected**
- Independent semver per crate: a release and compatibility check per crate
  for APIs nobody outside depends on (whether a sub-crate should become
  public is open: [ROADMAP §4](ROADMAP.md#4-open-questions)).
- One crate with feature-gated modules: recreates the crate split in `cfg`
  attributes, and parser-only users still pull SQLite unless every module
  is gated.

## D7. Liberal link model

Context decides meaning; a link in code is a mention and never diagnosed.

**Decision**
- Two stages: [pulldown-cmark](https://github.com/pulldown-cmark/pulldown-cmark)
  classifies byte ranges (prose, heading, code, HTML, comment, math,
  frontmatter); a single-pass hand-written scanner then finds wiki, org (in
  `.md` too), md, ref, bare-path, URL, tag, footnote and templating links.
- Every candidate records its context. In **code** (fenced, indented,
  inline, math) a link is a *mention*: indexed and goto-able if it
  resolves, but never a backlink or reference, **never diagnosed**, and
  never rewritten by rename. Comments behave the same.
- Confidence levels: *explicit* forms (md, wiki, org, ref, HTML href) may be
  diagnosed; *implicit* forms (bare paths, frontmatter plain values) become
  links only if the target exists and are otherwise dropped silently;
  *external* (URLs, `file:` outside the root, org `#+LINK` abbreviations,
  unknown `X:` prefixes) are never diagnosed.
- An explicit link whose target is not indexed but exists on disk
  (gitignored, hidden, pruned) is "unindexed": goto and hover work, no
  diagnostic.
- One resolution ladder for every feature; the first step with any hit
  stops it; ties go to the closest path and are flagged ambiguous. Partial
  (zk-style) matching is never used for diagnostics
  ([index spec §2.4](specs/index.md#24-resolution-ladder)).
- Broken explicit links: warning by default; error if the root's existing
  config says so or > 98% of explicit links already resolve; hint if < 80%
  resolve ([index spec §3.3](specs/index.md#33-diagnostics); the thresholds
  are unvalidated, [ROADMAP §1](ROADMAP.md#1-built-but-not-validated)).

**Why**
- Users link in every dialect, often mixed in one vault; the resolver
  accepts all of them instead of asking which.
- Links in code are useful to jump to (`[[note]]` in a README fence) but are
  often not links (`[[{{filename-stem}}]]`, Lean's `[[]]`).
- Bare paths only count if they resolve: in the ~730-note vault, 2,421 of
  2,423 path-like tokens were not files.
- A vault full of red errors is the main reason people uninstall a
  zero-config tool.

**Rejected**
- Diagnosing everything that looks like a link: false errors in code,
  templates and generated folders.
- An mdroots link-format setting (like marksman `completion.wiki.style`): an
  existing tool config (zk `link-format`, [Obsidian](https://obsidian.md)
  `useMarkdownLinks`/`newLinkFormat`) is read and wins for inserted links;
  otherwise the vote decides (D8).

## D8. Zero config

Conventions are detected and voted; existing tool configs are read, never
required or written.

**Decision**
- No mdroots config file and no init step. Roots are discovered from
  markers ([roots spec §1](specs/roots.md#1-discovery-stages-cheap-to-expensive)).
- Dialects are detected from markers (`.zk/`, `.obsidian/`,
  `.marksman.toml`, `.foam/`, `dendron.yml`, `logseq/config.edn`, org
  files, configs of the static site generators, ...;
  [index spec §3.1](specs/index.md#31-markers)). Several markers in one
  tree are merged, not ranked.
- A tool's config is read only when its marker is present and a setting in
  it changes an answer: zk's link format, tag syntaxes and `dead-link`
  severity, Obsidian's link style, the docs dir of a
  [MkDocs](https://www.mkdocs.org) site. It is never required: a missing or
  malformed config falls back to the marker's defaults and the vote. mdroots
  never writes a tool config, and never reads or writes zk's `notebook.db`.
- [git](https://git-scm.com)'s own files (`.git/config`) and ignore files
  (`.gitignore`, `.ignore`, `.mdrootsignore`) are read as always; they are
  not tool configs.
- Settings are visible: `mdroots roots` prints the link style, tag
  syntaxes and broken-link severity (and a docs dir when set), each with its
  source (a tool config key, a marker's default, the vote or mdroots'
  default), so a stale config is easy to spot.
- A corpus vote over the in-memory index, computed per process on first use
  and dropped on any content change, measures the insert style, wiki vs md,
  the `.md` suffix, `#tag` use and H1-as-title
  ([index spec §3.2](specs/index.md#32-vote-and-link-style)).
- Inserted links (`Workspace::link_to`, extract-note) follow the root: an
  existing zk config wins (over Obsidian too), then an Obsidian config, then
  the vote; a root without explicit links gets file-relative Markdown links
  with `.md`.

**Why**
- Goal: install and forget. zk needs `zk init`; marksman needs a VCS marker
  or `.marksman.toml` and offers config to pick a style.
- Reading an existing config is still zero config for the user: settings
  they already made are honoured. Writing one would touch user trees, often
  git repos.
- The vote picks the style the vault already uses: root-relative paths in
  the ~730-note vault, stems in the ~210-note vault.

**Rejected**
- A required config file or init step (zk): the main friction zero config
  removes.
- Ignoring tool configs entirely: a setting the user made (say zk
  `dead-link = "none"`) would silently not apply.
- Writing detected settings back to tool configs: modifies user files (see D2).

## D9. The DB caches content; queries run in memory

**Decision**
- Each `files` row of the per-root DB (D4) holds a note's root-relative
  path, its stat (`ino`, `ctime_ns`, `mtime_ns`, `size`) and its **bytes**;
  an FTS5 table mirrors the bytes for full-text search. There are no
  derived SQL tables (keys, links, frontmatter).
- Every process hydrates an in-memory `MemStore` from the DB's bytes
  (`MemStore::from_contents`) and answers every query from memory, except
  the reconciler's `full_text`, which reads the FTS table. Whole-root
  results (the diagnostics policy's resolved share, the backlink index) are
  computed once per store and dropped on the next content change. An
  unchanged note is never opened: it is re-read only when its
  `(ino, ctime_ns, size)` differs from its row, or it has no row.
- Content equality instead of a hash: a read with new bytes is an upsert
  plus a `change_log` entry; the same bytes with a new stat (`touch`)
  update only the stat columns. A parser upgrade needs no re-index, because
  every process parses the stored content.
- The reconciler (D3) re-lists the root per its mode on open and on
  `Workspace::refresh` (budgeted walk, git index scan, enumerator, or the
  lazy working set), re-stats every file and writes what changed in
  `BEGIN IMMEDIATE` batches of at most 200 files or 50 ms. A peer never
  lists: it uses the DB's file set, re-stats it and reads changed files into
  memory only.
- Changes on disk arrive through `refresh` (which `mdroots lsp` calls on
  `didSave`, and on `didChangeWatchedFiles` for workspaces that do not
  watch) and through the reconciler's opt-in native watcher, which
  point-refreshes changed paths with `refresh_paths` and patches the store
  in place.
- `Workspace::open_at` (a directory chosen by the caller) and single-file
  decisions stay in memory; only discovered, registered roots get a DB.

**Why**
- It gives the warm-start win of D4 (no file opens for unchanged notes)
  with a small schema and no query layer to keep in sync with the parser.
- Storing bytes makes change detection exact without a hash and makes
  parser changes free.
- Measured: on a synthetic 3,000-note notebook, `mdroots check` on one note
  drops from 0.6–0.7 s with a fresh cache to 0.11 s with the DB present
  ([index spec §1.5](specs/index.md#15-short-lived-instances)).

**Costs accepted**
- Every process holds every note's bytes and parse in memory, so memory
  grows with the vault. The 3,000-note notebook stays under the 35 MB
  per-process target (D3) with little margin; at which size that stops
  holding is open ([ROADMAP §4](ROADMAP.md#4-open-questions)).
- Hydrating parses every note at start; a peer sees another process's
  writes only on its next `refresh`.

**Deferred** ([ROADMAP §3](ROADMAP.md#3-deferred-work)):
- **Derived tables** (`keys`, `links`, `frontmatter` with indexed lookups).
  Every measured path stays under the 35 MB target on 3,000 notes, and the
  target vaults are far smaller, so a second query layer kept in sync with
  the parser is not worth it yet.
- **FSEvents replay** (`sinceWhen`). [notify](https://crates.io/crates/notify)
  cannot set it, and the alternatives (`fsevent-sys`, direct FFI) need
  unsafe code, which the workspace forbids (`unsafe_code = "forbid"`).
  Offline changes are already caught by the re-list and re-stat on open.
- **Cross-root `ATTACH`, [Watchman](https://facebook.github.io/watchman/)
  clocks**: no measured need yet.

**Rejected**
- Derived tables before the in-memory layer is measured to fall short.
- Hash column next to the content: redundant with stored bytes.
