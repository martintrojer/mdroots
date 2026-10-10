# Roadmap: what is left, open and unanswered

What mdroots does not do yet, what is built but not validated, and the
questions still open. Specs describe what exists; [DECISIONS.md](DECISIONS.md)
holds the decisions in force. An item leaves this file when it becomes code
plus a spec change, or a decision.

## 1. Built but not validated

These exist and pass their tests, but nobody has checked them against the
real world they are meant for.

1. **Linux.** The gate runs in CI (Linux, macOS, MSRV); the
   [Neovim](https://neovim.io) smoke test does not. Checked by hand on
   Fedora 44 (x86_64, btrfs home, tmpfs `/tmp`): the smoke test, mount
   classification from `/proc/self/mountinfo` (a `fuse.portal` mount
   classifies as virtual and is never walked), and the
   [notify](https://crates.io/crates/notify) watcher on inotify (an on-disk
   change republishes diagnostics in about 7 ms). Not checked: other
   distributions, NFS homes.
2. **Real-vault differential.** Resolution parity with
   [zk](https://github.com/zk-org/zk) is measured
   ([zk differential](research/zk-differential.md)). Not run with the real
   binary: diagnostics parity with
   [marksman](https://github.com/artempyanykh/marksman) and zk, feature
   smoke (symbols, hover, reference counts), and timing against both servers
   ([index spec §5.1](specs/index.md#51-differential-and-churn-tests),
   items 2–4).
3. **Many editors at once.** The memory target is < 35 MB `phys_footprint`
   per process with 10 editors on one root
   ([D3](DECISIONS.md#d3-in-process-no-daemon-one-writer-per-root)). Single
   processes measure 23–31 MB on a synthetic 3,000-note notebook, 17 MB at
   1,000 notes and **92 MB at 10,000 notes**, above the target
   ([benchmark](research/benchmark.md)). Ten at once has not been measured,
   nor cross-editor save visibility (p99) or a `kill -9` loop on the
   reconciler.
4. **Small repos on a virtual filesystem.** vcs-enumerated mode (a small
   repo in a large virtual-filesystem checkout, listed by the VCS within a
   time budget) is tested only with a fake enumerator.
5. **ramble as an embedder.** ramble's test suite drives its in-process
   backend (open, document links, goto, preview, pickers, watcher updates)
   on a fixture notebook with a temp cache dir. Not checked: real vaults,
   big roots opened from ramble, or ramble and `mdroots lsp` sharing one
   root's cache.
6. **Thresholds.** Chosen, not measured on real vaults
   ([index spec §5.2](specs/index.md#52-thresholds-still-to-validate)):
   - broken-link severity at > 98% (error) / < 80% (hint) resolved links: both
     testbed vaults (94.7%, 93.5%) land on warning without their zk config;
   - the loose-root rule (≥ 20 notes, ≥ 30% notes) and the walk budgets,
     validated only on the fixtures
     ([roots spec §7](specs/roots.md#7-fixtures));
   - the 5 ms/dir rate check, tuned on warm APFS, never on a cold cache, a
     cloud folder or NFS;
   - reconcile batches of ≤ 200 files / ≤ 50 ms;
   - the vote: `#tag` use at ≥ 3 distinct tags in ≥ 2 files, H1-as-title at
     70% of docs;
   - the 500 ms diagnostics debounce and the 1 s progress delay.

## 2. Known limitations

Deliberate simplifications and gaps in what the specs describe.

**Server**
- **One request at a time.** The server handles requests in order on one
  thread. A newer `didChange` cancels queued requests for that document, but a
  request already running is not interrupted. No worker pool for workspace
  symbols, full text or large reference queries.
- **One phase of diagnostics.** All diagnostics come 500 ms after the last
  edit; there is no immediate in-document phase and no save-only trigger.
- **One huge note is parsed synchronously** when it is opened (a 4.8 MB note
  with 200k links took 9 s to its first diagnostics). Large roots open in the
  background; a large single file does not.
- **The root opens on the first `didOpen`**, not while answering
  `initialize`.
- **No read semaphore or recent-mtime guard**
  ([library spec §4](specs/library.md#4-scheduling)): reads of a slow
  virtual-filesystem root are not capped, and there is no cheap mtime scan to
  guard.
- **No `mdroots.reindex` command.** The LSP commands are `backlinks`,
  `anchorLinks`, `info` and `renameFile`.
- **`info` does not show settings.** `mdroots.info` (`:MdrootsInfo`) shows
  root, mode, reason and file count; the effective settings and their
  sources are printed only by `mdroots roots`.

**Index and processes**
- **Peers learn of writes on `refresh`**, not per query: nothing follows
  `PRAGMA data_version` or `change_log`, and there is no hot cache. A cache
  purged between refreshes is noticed only on the next `refresh`.
- **No takeover retry.** A peer tries the reconciler lock on `refresh` only,
  not on a timer.
- **No stuck-reconciler warning.** There is no `meta.reconciled_at`; a peer
  never warns about a stalled reconciler or point-checks its files.
- **No WAL management.** The reconciler does not checkpoint periodically, set
  `journal_size_limit`, or checkpoint on exit, so a leaked long-lived reader
  can grow the WAL.
- **A full disk at write time** does not drop the session to memory.
- **No background reconcile or sweeps.** The index is brought up to date
  synchronously on open, `refresh` and watcher events; there is no
  background priority queue, no daily verification sweep, no lazy working-set
  sweep, and no background QoS class. `Freshness::Stale` does not exist.
- **Lazy roots don't grow** as files are opened.
- **`MDROOTS_LAZY`** (forcing lazy mode) does not exist.
- **Markers inside a root.** Moving rows between root DBs when a marker
  appears in, or disappears from, a root's subtree is not built
  ([roots spec §4](specs/roots.md#4-nested-roots)).
- **Links into another root** resolve only by `stat` (relative paths) and are
  reported as unindexed, not resolved.
- **The loose-root hysteresis** (re-climb only after 7 days or a 2× change in
  file count) is not applied
  ([roots spec §2](specs/roots.md#2-loose-roots-no-vcs-or-marker)).
- **`mdroots search DIR`** scans in memory; only a file path (which opens its
  discovered root) uses the full-text index.

**Dialects and frontmatter** ([index spec §3.1](specs/index.md#31-markers),
[§4](specs/index.md#4-frontmatter))
- **Dialect specifics not built:** Gollum's spaces ↔ dashes, Dendron vaults
  as sub-roots, the Obsidian daily-notes plugin, marksman
  `completion.wiki.style`, zk `[note] extension` and group `paths`,
  [mdBook](https://rust-lang.github.io/mdBook/) `SUMMARY.md` order.
- **Not voted:** piped-wiki order (`[[target|label]]` vs
  `[[label|target]]`) and tag syntaxes beyond `#tag`.
- **Schema-free frontmatter features:** key completion, value completion
  per key, hover on a key, references on a value, and the missing-key hint
  are not built. The key-collision info diagnostic (`Title:` next to
  `title:`) does not fire.

**Library**
- **No feature flags.** `cargo add mdroots` always builds SQLite, the
  watcher and root discovery (deferred, §3).
- **No `Store`, `LinkResolver` or `Observer` traits**: house conventions
  (e.g. `[[wiki:Page]]`) can't be added to the ladder without forking.

## 3. Deferred work

Ordered by expected value. Each has a reason it is not built yet.

| Item | Why not yet |
|---|---|
| Library heading sections (`Workspace::sections(path)`) | heading-section folding is computed inside `mdroots-lsp` today; moving it into the library would let [ramble](https://github.com/martintrojer/ramble), a read-only TUI, fold by section too (per-heading link counts already exist: `anchor_backlinks`, `heading_backlinks`) |
| `workspace/willRenameFiles`, a full-text LSP request, semantic tokens | small server additions; no client asked yet (Neovim 0.12 never sends `willRenameFiles`) |
| Embedder API: typed `subscribe` events, `Preview.summary`, unfenced front matter as a workspace option (today only `ParseOptions` has it), a server builder that shares an embedder's `Workspaces` | gaps ramble works around ([research/ramble](research/ramble.md)); none blocks it |
| Scheduling: two-phase diagnostics, a worker pool, views cancelled by `change_log` advances, a read semaphore, opening the root during `initialize` (§2) | one request at a time has been fast enough on the target vaults ([research/gopls](research/gopls.md)) |
| Peer freshness: `data_version` and DB-inode check per query, `change_log` follower, hot cache, takeover retry timer, stale-reconciler warning (§2) | peers refresh on save and watched-file events; no measured staleness problem |
| WAL management: periodic `wal_checkpoint(PASSIVE)`, `journal_size_limit`, exit checkpoint above 4 MB | no WAL growth seen yet |
| Background reconcile and sweeps, background QoS, `Freshness::Stale`, working-set growth for lazy roots | the synchronous reconcile is fast enough on the target vaults |
| Frontmatter features and the dialect specifics in §2 | no user asked yet |
| Derived SQL tables (`keys`, `links`, `frontmatter`) | the likely fix for memory above 3,000 notes (92 MB at 10,000, §1.3); adds a second query layer and a parser version to keep in sync ([D9](DECISIONS.md#d9-the-db-caches-content-queries-run-in-memory)) |
| FSEvents replay (`sinceWhen`) | needs FFI and the workspace forbids unsafe code; the re-list on open already catches offline changes (D9) |
| Cross-root `ATTACH` | no measured need ([roots spec §4](specs/roots.md#4-nested-roots)) |
| [Watchman](https://facebook.github.io/watchman/) clocks | no measured need |
| Feature flags on `mdroots` (a parser-only, wasm-capable build without SQLite, threads or discovery) | wait for a parser-only or wasm user |

## 4. Open questions

Each ends as a spec change or a decision.

1. **Severity thresholds** (§1, item 6): keep 98% / 80%, or derive them per
   root? Measure on more vaults first.
2. **Bare paths in prose.** In the research vault 192 of 193 bare path-like
   tokens that exist on disk are frontmatter values; in the zk vault 2 of
   2,423 exist. Resolve bare paths only in frontmatter values, or also in
   prose (gated on existence)?
3. **Empty links.** `[t]()` reports a broken link (at the root's broken-link
   severity); zk stores nothing for it
   ([zk differential](research/zk-differential.md)). Should empty
   destinations be silent?
4. **Org-mode depth.** In scope: links, headings, `:ID:`, `#+TITLE`, `#+LINK`.
   Open: agenda, `id:` links across roots, properties beyond `:ID:`.
5. **vcs-enumerated mode** (§1, item 4): is the listing budget right,
   should a filesystem-native glob API replace the VCS child process, and should
   [git](https://git-scm.com)'s `git ls-files` do the same for other lazy
   roots?
6. **Memory at scale** (§1, item 3): up to which vault size must the 35 MB
   target hold, and is that what triggers the derived tables?
7. **FSEvents replay cost.** After a day offline, replay may degrade to
   directories only (`MustScanSubDirs`, dropped events, `EventIdsWrapped`).
   Is it cheaper than today's re-list on open, and is it worth an FFI
   exception to `forbid(unsafe_code)`?
8. **`.mdrootsignore` vs `.ignore` plus a key in `.mdroots`.** The dedicated
   file exists because `.gitignore` answers a VCS question, folders without
   VCS lack one, and an empty file means "never index here". Folding it into
   `.ignore` (shared with [ripgrep](https://github.com/BurntSushi/ripgrep) and
   [fd](https://github.com/sharkdp/fd)) would drop one magic file name, but
   would also hide those files from those tools.
9. **Public sub-crates.** Should `mdroots-syntax` (a parser-only user) become
   a public crate with its own stability promise, and does that require
   leaving lockstep versions
   ([D6](DECISIONS.md#d6-crate-graph-and-lockstep-versions))?
10. **Short binary alias.** `mw` and `mdr` are taken on crates.io. Find a
    candidate and run the
    [D1](DECISIONS.md#d1-name-mdroots) namespace checks.
11. **Name checks for `mdroots`.** npm and trademark (USPTO/EUIPO, classes 9
    and 42) are still unchecked; crates.io, Homebrew, PyPI, Debian and
    GitHub were checked.
