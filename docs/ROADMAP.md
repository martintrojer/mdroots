# Roadmap: what is left, open and unanswered

What is not built, what is built but not validated, and open questions.
Specs describe what exists; [DECISIONS.md](DECISIONS.md) holds the decisions
in force. An item leaves this file when it becomes code plus a spec change,
or a decision.

## 1. Built but not validated

These pass their tests but are unchecked against the real world they target.

1. **Linux.** The gate runs in CI (Linux, macOS, MSRV); the
   [Neovim](https://neovim.io) smoke test does not. Checked by hand on
   Fedora 44 (x86_64, btrfs home, tmpfs `/tmp`): the smoke test, mount
   classification from `/proc/self/mountinfo` (a `fuse.portal` mount is
   virtual, never walked), and the [notify](https://crates.io/crates/notify)
   watcher on inotify (an on-disk change republishes diagnostics in ~7 ms).
   Unchecked: other distributions, NFS homes.
2. **Real-vault differential.** Resolution parity with
   [zk](https://github.com/zk-org/zk) is measured
   ([zk differential](research/zk-differential.md)). Not run with the real
   binary: diagnostics parity with
   [marksman](https://github.com/artempyanykh/marksman) and zk, feature
   smoke (symbols, hover, reference counts), timing against both servers
   ([index spec §5.1](specs/index.md#51-differential-and-churn-tests),
   items 2–4).
3. **Many editors at once.** Target: < 35 MB `phys_footprint` per process
   with 10 editors on one root
   ([D3](DECISIONS.md#d3-in-process-no-daemon-one-writer-per-root)). One
   process measures 14 MB at 1,000 notes and **60 MB at 10,000 notes**,
   above target ([benchmark](research/benchmark.md#memory)). At 10k the live
   heap is 38 MB, 17.8 MB of it note text held in memory
   ([D9](DECISIONS.md#d9-the-db-caches-content-queries-run-in-memory)).
   Unmeasured: ten at once, cross-editor save visibility (p99), a `kill -9`
   loop on the reconciler.
4. **Small repos on a virtual filesystem.** vcs-enumerated mode (a small
   repo in a large virtual-filesystem checkout, listed by the VCS within a
   time budget) is tested only with a fake enumerator.
5. **ramble as an embedder.** ramble's tests drive its in-process backend
   (open, document links, goto, preview, pickers, watcher updates) on a
   fixture notebook with a temp cache dir. Unchecked: real vaults, big roots
   opened from ramble, ramble and `mdroots lsp` sharing one root's cache.
6. **Thresholds.** Chosen, not measured on real vaults. The index-layer list
   is [index spec §5.2](specs/index.md#52-thresholds-still-to-validate);
   there, both testbed vaults (94.7%, 93.5% resolved) land on warning
   without their zk config. Also unvalidated:
   - the loose-root rule and walk budgets: fixtures only
     ([roots spec §2](specs/roots.md#2-loose-roots-no-vcs-or-marker),
     [§7](specs/roots.md#7-fixtures));
   - the 5 ms/dir rate check: tuned on warm APFS, never on a cold cache, a
     cloud folder or NFS;
   - the 500 ms diagnostics debounce and the 1 s progress delay.

7. **Flaky many-process fixtures.** `crates/mdroots-cli/tests/many_process.rs`
   spawns real processes racing on one cache DB. Fixtures 13 (cold herd,
   `many_process.rs:92`) and 15 (corrupt DB rebuilt, "early EOF" at `:498`)
   have each failed in CI and passed on rerun of the same commit
   (385d41e Ubuntu, e634a5d macOS, 1ea3a51 MSRV). Not reproduced locally;
   cause unknown (a child exiting before printing, or a timing assumption).

## 2. Known limitations

Deliberate simplifications and gaps in what the specs describe. §3 orders
the planned ones.

**Server** ([library spec §4](specs/library.md#4-scheduling))
- **One request at a time**, in order, on one thread. A newer `didChange`
  cancels queued requests for that document; a running request is not
  interrupted, and queries are not cancelled by `change_log` advances. No
  worker pool for workspace symbols, full text or large reference queries.
- **One phase of diagnostics**, 500 ms after the last edit; no immediate
  in-document phase, no save-only trigger.
- **One huge note is parsed synchronously** on open (4.8 MB, 200k links: 9 s
  to first diagnostics). Large roots open in the background; a large file
  does not.
- **The root opens on the first `didOpen`**, not during `initialize`.
- **No read semaphore or recent-mtime guard**: reads of a slow
  virtual-filesystem root are uncapped, and there is no cheap mtime scan.
- **LSP commands** are `backlinks`, `anchorLinks`, `info`, `renameFile`:
  no `reindex`, no graph or search commands, and `info` does not show
  settings (planned: phase 2 below).
- **No quick fixes.** The only code action is extract note (planned:
  phase 1 below).

**Index and processes**
- **Peers learn of writes on `refresh`**, not per query: no per-query
  `PRAGMA data_version` or DB-inode check, no `change_log` follower, no hot
  cache. A cache purged between refreshes is noticed on the next `refresh`.
- **No takeover retry.** A peer tries the reconciler lock only on `refresh`,
  not on a timer.
- **No stuck-reconciler warning.** No `meta.reconciled_at`; a peer never
  warns about a stalled reconciler or point-checks its files.
- **No WAL management.** No periodic `wal_checkpoint(PASSIVE)`, no
  `journal_size_limit`, no exit checkpoint above 4 MB; a leaked long-lived
  reader can grow the WAL.
- **A full disk at write time** does not drop the session to memory.
- **No background reconcile or sweeps.** The index updates synchronously on
  open, `refresh` and watcher events. No background priority queue, daily
  verification sweep, lazy working-set sweep or background QoS class; no
  `Freshness::Stale`.
- **Lazy roots don't grow** as files are opened.
- **No `MDROOTS_LAZY`** (forcing lazy mode).
- **Markers inside a root.** Moving rows between root DBs when a marker
  appears in or disappears from a root's subtree is not built
  ([roots spec §4](specs/roots.md#4-nested-roots)).
- **Links into another root** resolve only by `stat` (relative paths), as
  unindexed.
- **No loose-root hysteresis** (re-climb only after 7 days or a 2× change in
  file count) ([roots spec §2](specs/roots.md#2-loose-roots-no-vcs-or-marker)).
- **Memory, two extras** outside the measured baseline: the facade keeps
  each open buffer's text again beside its parsed overlay
  (`Inner.overlays`), and a full `refresh` holds the old store while
  building the new one, briefly doubling peak memory.
- **`mdroots search DIR`** scans in memory; only a file path (opening its
  discovered root) uses the full-text index.

**Dialects and frontmatter** ([index spec §3.1](specs/index.md#31-markers),
[§4](specs/index.md#4-frontmatter))
- **Dialect specifics not built:** Gollum's spaces ↔ dashes, Dendron vaults
  as sub-roots, the Obsidian daily-notes plugin, marksman
  `completion.wiki.style`, zk `[note] extension` and group `paths`,
  [mdBook](https://rust-lang.github.io/mdBook/) `SUMMARY.md` order.
- **Not voted:** piped-wiki order (`[[target|label]]` vs
  `[[label|target]]`), tag syntaxes beyond `#tag`.
- **Schema-free frontmatter features not built:** key completion, per-key
  value completion, hover on a key, references on a value, the missing-key
  hint. The key-collision info diagnostic (`Title:` next to `title:`) does
  not fire.

**Library**
- **No feature flags.** `cargo add mdroots` always builds SQLite, the
  watcher and root discovery; no parser-only, wasm-capable build without
  SQLite, threads or discovery.
- **No `Store`, `LinkResolver` or `Observer` traits**: house conventions
  (e.g. `[[wiki:Page]]`) can't join the ladder without forking.

## 3. Deferred work

Ordered by expected value. (§2) means the gap is described there.

| Item | Why not yet |
|---|---|
| **LSP phase 1, link repair**: quick fixes for broken links, anchors and ambiguous links, create note, convert link, link selection ([library spec, planned](specs/library.md#fixes-phase-1-link-repair)) | designed, not built |
| **LSP phase 2, notebook commands**: orphans, missing backlinks, related, query, search, check, tags, reindex, info with settings, linkTo ([library spec, planned](specs/library.md#notebook-commands-phase-2)) | designed; less value than link repair |
| **LSP phase 3**: document highlight, `willRenameFiles`, inlay hints, tag hover and references, the frontmatter key-collision diagnostic ([library spec, planned](specs/library.md#smaller-requests-phase-3)) | designed; small, and `willRenameFiles` needs a client that sends it (Neovim 0.12 does not) |
| LSP later: fix-all, add a missing heading to the target, link-style and ambiguous-stem diagnostics, linkify unlinked mentions, inline note, extract section, TOC, call hierarchy as link graph, semantic tokens | no user asked; fix-all waits for phase 1 in use |
| `mdroots check --fix` | a CLI has no editor to apply edits; it would be the first mdroots code to write notes ([D10](DECISIONS.md#d10-mdroots-proposes-edits-the-editor-applies-them)) |
| Library heading sections (`Workspace::sections(path)`) | folding is computed in `mdroots-lsp`; in the library, [ramble](https://github.com/martintrojer/ramble), a read-only TUI, could fold by section too (per-heading link counts exist: `anchor_backlinks`, `heading_backlinks`) |
| Embedder API: typed `subscribe` events, `Preview.summary`, unfenced front matter as a workspace option (today only `ParseOptions`), a server builder sharing an embedder's `Workspaces` | ramble works around them ([research/ramble](research/ramble.md)); none blocks it |
| Scheduling (§2, Server) | one request at a time is fast enough on the target vaults ([research/gopls](research/gopls.md)) |
| Peer freshness, takeover retry timer, stale-reconciler warning (§2) | peers refresh on save and watched-file events; no measured staleness |
| WAL management (§2) | no WAL growth seen |
| Background reconcile and sweeps, lazy-root growth (§2) | synchronous reconcile is fast enough on the target vaults |
| Frontmatter features, dialect specifics (§2) | no user asked |
| Derived SQL tables (`keys`, `links`, `frontmatter`) | the remaining memory fix at 10,000 notes ([§1 item 3](#1-built-but-not-validated)); adds a second query layer and a parser version to keep in sync ([D9](DECISIONS.md#d9-the-db-caches-content-queries-run-in-memory)) |
| FSEvents replay (`sinceWhen`) | needs FFI; the workspace forbids unsafe code; the re-list on open already catches offline changes (D9) |
| Cross-root `ATTACH` | no measured need ([roots spec §4](specs/roots.md#4-nested-roots)) |
| [Watchman](https://facebook.github.io/watchman/) clocks | no measured need |
| Feature flags on `mdroots` (§2) | wait for a parser-only or wasm user |

## 4. Open questions

Each ends as a spec change or a decision.

1. **Severity thresholds** (§1, item 6): keep 98% / 80%, or derive per root?
   Measure on more vaults first.
2. **Empty links.** `[t]()` is a broken link (at the root's broken-link
   severity); zk stores nothing
   ([zk differential](research/zk-differential.md)). Should empty
   destinations be silent?
3. **Org-mode depth.** In scope: links, headings, `:ID:`, `#+TITLE`, `#+LINK`.
   Open: agenda, `id:` links across roots, properties beyond `:ID:`.
4. **vcs-enumerated mode** (§1, item 4): is the listing budget right, should
   a filesystem-native glob API replace the VCS child process, and should
   [git](https://git-scm.com)'s `git ls-files` do the same for other lazy
   roots?
5. **Memory at scale** (§1, item 3): up to which vault size must the 35 MB
   target hold, and does that trigger the derived tables? It holds at 1,000
   notes, not at 10,000; the next step down needs note text out of memory.
6. **FSEvents replay cost.** After a day offline, replay may degrade to
   directories only (`MustScanSubDirs`, dropped events, `EventIdsWrapped`).
   Is it cheaper than the re-list on open, and worth an FFI exception to
   `forbid(unsafe_code)`?
7. **`.mdrootsignore` vs `.ignore` plus a key in `.mdroots`.** The dedicated
   file exists because `.gitignore` answers a VCS question, folders without
   VCS lack one, and an empty file means "never index here". Folding it into
   `.ignore` (shared with [ripgrep](https://github.com/BurntSushi/ripgrep) and
   [fd](https://github.com/sharkdp/fd)) drops one magic file name but hides
   those files from those tools.
8. **Public sub-crates.** Should `mdroots-syntax` (a parser-only user) become
   a public crate with its own stability promise, and does that mean leaving
   lockstep versions
   ([D6](DECISIONS.md#d6-crate-graph-and-lockstep-versions))?
9. **Short binary alias.** `mw` and `mdr` are taken on crates.io. Find a
    candidate and run the
    [D1](DECISIONS.md#d1-name-mdroots) namespace checks.
10. **Name checks for `mdroots`.** Unchecked: npm, trademark (USPTO/EUIPO,
    classes 9 and 42). Checked: crates.io, Homebrew, PyPI, Debian, GitHub.
