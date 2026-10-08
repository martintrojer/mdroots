# Decisions

The design choices behind mdroots that are hard to reverse. Mechanics live in the specs: [roots](specs/roots.md), [index](specs/index.md), [library](specs/library.md). Unresolved points are in [OPEN-QUESTIONS.md](OPEN-QUESTIONS.md).

Measurements below come from two testbed vaults: a ~730-note zk vault and a ~210-note research vault.

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
- Synchronous core, no async runtime. Background work is opt-in (`Options::background`). With `background(false)` nothing polls; the embedder calls `reconcile(budget)` or any query.
- Slow calls (`reconcile`, `wait_fresh`, full-text search, workspace-wide diagnostics) take `&Cancel`; the LSP sets it on `$/cancelRequest` or a newer `didChange`.
- Each query reads one SQLite snapshot (one read transaction) with overlays applied at call time, so a concurrent commit or overlay change is never seen halfway.
- A non-blocking open returns a `Lazy` workspace at once and finishes discovery in the background, so an editor never blocks on a slow filesystem.
- The library never writes user files. Refactors return a `WorkspaceEdit`.
- Extension traits (`FileSystem`, `Store`, `LinkResolver`, `Observer`, `ResolveEnv`) are `Send + Sync`. `SqliteStore` holds one writer connection plus a capped pool of read connections, because `rusqlite::Connection` is `!Sync`.
- A feature that can't be tested without JSON-RPC is in the wrong crate.

**Why**
- marksman (F#, exe-only) and zk (Go `internal/` packages) can't be reused as libraries; embedding was a goal from the start.
- Queries are microsecond-to-millisecond SQLite lookups; async adds nothing and would force a runtime on every embedder. Async callers use `spawn_blocking`.

**Rejected**
- Build on iwe's `liwe`: in-memory arena (cold every start, one copy per process), needs `.iwe` config, API explicitly unstable.
- Async core on tower-lsp: forces a runtime on embedders for no gain.
- (One crate with feature-gated modules: see D6.)

## D3. In-process only, no daemon; one writer per root (flock reconciler), readers never write

**Decision**
- mdroots always runs inside the embedding process. No daemon, RPC, sockets or remote backend.
- Per root, `flock(LOCK_EX|LOCK_NB)` on `<id>.lock` elects a **reconciler**: background sweep, watcher, FTS. It is the only process that writes the root's DB.
- Every other process is a **read-only peer**: it serves queries from the DB plus its own overlays (unsaved buffers, and files it just saved, parsed in memory until the DB catches up). Its saves reach the DB through the reconciler's watcher or next sweep.
- A peer that wants freshness while no one holds the lock tries the lock and becomes the reconciler.
- Peers follow commits via `PRAGMA data_version` (~1.2 µs per call; changes only for other connections' commits) plus `change_log`.
- Crash recovery is the kernel releasing the flock (also on SIGKILL). No heartbeats, PID files or stale-lock cleanup.
- Only the reconciler acts on watcher events; peers act only on their own `didOpen`/`didChange`/`didSave`, so a save is not parsed N times.
- Use `std::fs::File::try_lock` / `try_lock_shared` (Rust ≥ 1.89). Rust opens files `O_CLOEXEC`, so children don't inherit locks; a second fd in the same process is refused (that process becomes a peer); flock does not conflict with SQLite's fcntl locks.

**Lock files** (in the base dir of D5; full rules in [specs/roots.md](specs/roots.md))

| File | Locked by | Purpose |
|---|---|---|
| `<id>.lock` | reconciler, `LOCK_EX\|LOCK_NB` | elects the reconciler. **Never unlinked**, because a process waiting on an unlinked lock file would elect a second reconciler |
| `<id>.open` | every process, `LOCK_SH`, while the DB is open | GC, schema cleanup and corruption rebuild need `LOCK_EX\|LOCK_NB` on it, so they never unlink a DB another process has open (a documented SQLite corruption path) |
| `discover.lock` (global) | the discovering process, during discovery stages 2–4 | serialises discovery: one walk per user at a time; others re-check the registry after taking it |

`.lock` and `.open` are removed only when the whole root is GC-ed, under `LOCK_EX` on both. A process that takes either lock re-checks that the path still names the locked inode (`stat` vs `fstat`).

**Transactions and failure modes**
- The reconciler's writes are `BEGIN IMMEDIATE` with `busy_timeout`, because a deferred read-then-write gets `SQLITE_BUSY` at once and ignores the timeout. Readers retry on `SQLITE_BUSY` during WAL recovery after a crash.
- The reconciler checkpoints periodically and caps WAL size, because a leaked long-lived reader blocks checkpoints.
- Stuck reconciler (alive but SIGSTOPed or hung on a network/virtual FS `stat`): no liveness detection. A peer that sees a stale `meta.reconciled_at` logs one warning and point-checks its own open files and their link targets in memory. It still writes nothing.
- Watcher gap on takeover (up to the 5 s lock retry): on macOS the new reconciler replays FSEvents from the last committed ID; on Linux it runs the `dir_state` diff once before starting inotify.
- Herd start (ten editors at once): one process walks under `discover.lock`, one wins the root flock, nine serve from the existing index. A "lazy" verdict from the walk-rate check alone is not saved until a second measurement agrees, so a herd-slowed walk doesn't stick.

**Why**
- Many editors run at once (tmux, session restore); many processes are short-lived or killed.
- A single writer removes write races and the freshness-fence problems that come with them (see D4). A peer's own edits are already fresh in its overlay, so peer writes buy nothing visible.
- A daemon would save ~1–3 MB private memory per extra editor plus duplicate hot caches; mdroots' state lives in SQLite, not RAM. gopls's daemon saves hundreds of MB and is still off by default.
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
- One SQLite DB (WAL) per root, `roots/<id>.v<schema>.db`, plus a registry `roots.v<k>.db`. Schema versions are in the filenames, so old and new binaries never share a file or fight over migrations.
- The DB is a **cache**: correctness never depends on it. It is rebuilt on schema change, corruption or OS purge. `IndexMode::Memory` stays available for embedders.
- Links are stored unresolved, as normalised keys, and resolved at query time, so adding or renaming a doc never rewrites its referrers' rows.
- Writes go in small committed batches; changes are published through `change_log`.
- Corruption: never rename the file. The reconciler builds a new generation `<db>-<gen8>.db` with a new `meta.generation` (UUID) and repoints the registry; GC deletes the old file once `<id>.open` can be taken exclusively.
- Peers compare the DB path's inode and `meta.generation` on every `data_version` check and reopen when either changed (covers rebuilds and cache purges). `change_log.seq` restarts per generation, so a reopening peer drops its cursor and hot cache.
- Root moves: `files.path` is root-relative. On a registry miss, a DB whose stored marker inode (or volume UUID + marker inode) matches the new root is re-keyed instead of rebuilt.
- Rebuild order: metadata first in small batches, so links and diagnostics work early; FTS last, throttled, reconciler only. Batch size and page cache are bounded to stay under the RSS target.
- GC (at most daily): roots unseen for 30 days or gone, old schema versions older than 7 days, stale generations. Skips any DB whose `.open` is held.

**Freshness rule** (single writer, so no fence between writers; details in [specs/index.md](specs/index.md))
- Version = the `fstat` of the fd the content was read from: stat, read, `fstat`; retry if the stats differ.
- Change detection by `(ino, ctime_ns, size)`. Mtime is only a "maybe changed" hint for the cheap scan, because `cp -p`, `rsync -a`, `tar x` and restores set old mtimes, while `utimes` cannot set ctime.
- Re-parse headings, links and tags only when `hash` changed or `parser_ver` increased; otherwise update only the stat columns.
- Same ctime but different hash (coarse-ctime filesystems): mark the row `dirty`.

**Why**
- Parse speed alone does not justify a DB: walk + parse takes 104–129 ms on the ~730-note vault and 12–36 ms on the ~210-note vault, under the 250 ms cold target (pulldown-cmark ~1.1 GB/s).
- Warm start < 30 ms to first result: an in-memory server must open every file at start, ~100 µs per open on macOS, so ~2 s at 20k files before parsing.
- N editors share one index instead of N parses and N in-memory copies. marksman re-parses every start: ~610–840 ms to first result, 134–145 MB RSS. zk answers in 22–43 ms at 32–35 MB from its DB.
- FTS is too large to rebuild per process.
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
| 4 | in memory for the session | nothing usable; log why to `window/logMessage` |

- "Local" = `statfs` reports `MNT_LOCAL`, with fs-type name matching as fallback (same test as root classification). flock and WAL are used only there, because both are unsafe on NFS/SMB.
- A full disk at write time drops the session to in-memory instead of failing requests.
- OS purges and cleared runtime/tmp dirs are treated like deletion: peers reopen (D4), the next reconciler rebuilds. Cost: one cold start.
- `Options::write_cache(false)` never touches the cache dir (in-memory index), for read-only embedders.
- Processes with different environments may pick different base dirs; each gets its own reconciler. `mdroots roots` prints the base dir in use and why.

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
syntax ← resolve ← core ← index ← mdroots ← mdroots-lsp
                    ↑
                  roots (behind a feature)
```

| Crate | Holds |
|---|---|
| `mdroots-syntax` | parse one document → structure + link candidates, `LineIndex`; no I/O; builds for wasm |
| `mdroots-resolve` | resolution ladder, dialect detection, convention vote; defines `ResolveEnv` (existence, case sensitivity, home dir), so no direct I/O; builds for wasm |
| `mdroots-core` | `Store`, `FileSystem`, `MemStore`, reconcile logic, `Cancel`; re-exports and implements `ResolveEnv` |
| `mdroots-index` | `SqliteStore` and the flock roles only |
| `mdroots-roots` | root discovery; optional in the facade |
| `mdroots` | the facade embedders depend on |
| `mdroots-lsp` | protocol only; the only crate that names `lsp-types` |

- **Independent semver** per crate, `cargo-semver-checks` per crate in CI.
- crates.io refuses path-only dependencies, so every `mdroots` release publishes its sub-crates. Until a sub-crate's API settles it is an **internal 0.x crate** (README: "no stability promise, depend on `mdroots`") and the facade pins it with `=x.y.z`. `mdroots-syntax` and `mdroots` are the first public crates.
- Feature flags on `mdroots` (`roots`, `index`, `fts`, `watch`, `parallel`, `org`, `serde`) each document what they pull in: C code, threads, `libc`. `default-features = false` gives `syntax + resolve + core + MemStore`: pure Rust, no threads, no discovery, wasm-capable. Without `index` there is no FTS (a naive scan fallback exists for small vaults). Table in [specs/library.md](specs/library.md).
- No `lsp-types` feature on the facade; public enums such as `Freshness` are `#[non_exhaustive]`, so examples need a `_` arm.

**Why**
- Wasm and linter users who want only the parser must not compile SQLite.
- `ResolveEnv` lives in `resolve` because `resolve` sits below `core`; putting `Store` in the facade while `index` implements it would be a cycle.
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
- Existing tool configs are **read** for hints (zk link format, tag syntaxes and `dead-link` severity; Obsidian link style and attachment folder; marksman title settings). mdroots never writes them, and never reads or writes zk's `notebook.db`.
- A corpus vote, stored in `meta` and recomputed after each full reconcile, decides: completion insert style (share of links per ladder step), piped-wiki order, wiki vs md, `.md` suffix, tag syntaxes (≥ 3 distinct tags in ≥ 2 files), H1-as-title (≥ 70% of docs have exactly one H1), filename scheme.

**Why**
- Goal: install and forget. zk needs `zk init`; marksman needs a VCS marker or `.marksman.toml` and offers config to pick a style.
- Reading an existing config is still zero config for the user; writing one would touch user trees, often git repos.
- The vote picks the style the vault already uses: root-relative paths in the ~730-note vault, stems in the ~210-note vault.

**Rejected**
- A required config file or init step (zk): the main friction zero config removes.
- Writing detected settings back to tool configs: modifies user files (see D2).
