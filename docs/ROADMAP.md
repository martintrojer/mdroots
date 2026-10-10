# Roadmap: what is left, open and unanswered

What mdroots does not do yet, what is built but not validated, and the
questions still open. Specs describe what exists; [DECISIONS.md](DECISIONS.md)
holds the decisions in force. An item leaves this file when it becomes code
plus a spec change, or a decision.

Status: 0.2.5. Milestones M1–M7 are built: parsing and resolution, root
discovery, the `mdroots` facade and CLI, the per-root
[SQLite](https://sqlite.org) cache, the language server, the file watcher,
full-text search, cache GC, folding, code lenses, extract-note and a
background open ([library spec §7](specs/library.md)).

## 1. Built but not validated

These exist and pass their tests, but nobody has checked them against the
real world they are meant for.

1. **Linux.** The full gate runs in CI on Linux and macOS (GitHub Actions,
   plus the MSRV). By hand on Fedora 44 (x86_64, btrfs home, tmpfs `/tmp`):
   the [Neovim](https://neovim.io) smoke test, mount detection from
   `/proc/self/mountinfo` (a `fuse.portal` mount classifies as virtual and is
   never walked), and the [notify](https://crates.io/crates/notify) watcher on
   inotify (an on-disk change republishes diagnostics in about 7 ms). Not in
   CI: the Neovim smoke test.
2. **The real-vault differential since M1.** Resolution parity with
   [zk](https://github.com/zk-org/zk) was measured in M1
   ([m1-differential](research/m1-differential.md)). Not re-run with the real
   binary: diagnostics parity with
   [marksman](https://github.com/artempyanykh/marksman) and zk, feature
   smoke (symbols, hover, reference counts), and timing against both servers
   ([index spec §5.1](specs/index.md), items 2–4). `bench/lspbench.py` first
   needs `--skip`, per-request timeouts, a capability check and
   `phys_footprint`.
3. **Many editors at once.** The memory target is < 35 MB `phys_footprint`
   per process with 10 editors on one root. Single processes measure 23–31 MB
   on a synthetic 3,000-note notebook; ten at once has not been measured, nor
   cross-editor save visibility (p99) or a `kill -9` loop on the reconciler.
4. **Small repos on a virtual filesystem.** vcs-enumerated mode (a small
   [EdenFS](https://github.com/facebook/sapling) repo listed with `sl files`
   within 500 ms) is tested only with a fake enumerator.
5. **ramble as an embedder.** ramble's test suite drives its in-process
   backend (open, document links, goto, preview, pickers, watcher updates)
   on a fixture notebook with a temp cache dir. Not checked: real vaults,
   big roots opened from ramble, or ramble and `mdroots lsp` sharing one
   root's cache.
6. **Thresholds.** Chosen, not measured on real vaults
   ([index spec §5.2](specs/index.md)):
   - broken-link severity at > 98% (error) / < 80% (hint) resolved links: both
     testbed vaults (94.7%, 93.5%) land on warning without their zk config;
   - the loose-root rule (≥ 20 notes, ≥ 30% notes) and the walk budgets,
     validated only on the fixtures ([roots spec §7](specs/roots.md));
   - the 5 ms/dir rate check, tuned on warm APFS, never on a cold cache, a
     cloud folder or NFS;
   - reconcile batches of ≤ 200 files / ≤ 50 ms;
   - the 500 ms diagnostics debounce and the 1 s progress delay.

## 2. Known limitations

Deliberate simplifications; each is documented where it lives.

- **One request at a time.** The server handles requests in order on one
  thread. A newer `didChange` cancels queued requests for that document, but a
  request already running is not interrupted.
- **One huge note is parsed synchronously** when it is opened (a 4.8 MB note
  with 200k links took 9 s to its first diagnostics). Large roots open in the
  background; a large single file does not.
- **`mdroots search DIR`** scans in memory; only a file path (which opens its
  discovered root) uses the full-text index.
- **Links into another root** resolve only by `stat` (relative paths) and are
  reported as unindexed, not resolved.
- **No feature flags.** The feature table in the [library spec](specs/library.md)
  describes a plan; `cargo add mdroots` always builds SQLite, the watcher and
  root discovery.
- **The loose-root hysteresis** (re-climb only after 7 days or a 2× change in
  file count) is not applied ([roots spec §2](specs/roots.md)).
- **Peers learn of writes on `refresh`**, not per query: there is no
  `data_version` check or hot cache yet.
- **Missing-key frontmatter hints, key and value completion** from the
  [index spec §4.3](specs/index.md) are not built.

## 3. Deferred work (next milestones)

Ordered by expected value. Each has a reason it is not built yet.

| Item | Why not yet |
|---|---|
| Library heading sections (`Workspace::sections(path)`) | heading-section folding is computed inside `mdroots-lsp` today; moving it into the library would let [ramble](https://github.com/martintrojer/ramble), a read-only TUI, fold by section too (per-heading link counts already exist: `anchor_backlinks`, `heading_backlinks`) |
| "New note" command and the filename scheme vote | the link style exists (`Workspace::link_style`); the filename scheme (slug, id prefix, date) is not voted yet |
| `workspace/willRenameFiles`, a full-text LSP request, semantic tokens | small server additions; no client asked yet ([Neovim](https://neovim.io) 0.12 never sends `willRenameFiles`) |
| Embedder API: typed `subscribe` events, `Preview.summary`, unfenced front matter as a workspace option (today only `ParseOptions` has it), a server builder that shares an embedder's `Workspaces` | gaps ramble works around ([research/ramble](research/ramble.md)); none blocks it |
| Derived SQL tables (`keys`, `links`, `frontmatter`) | memory is under target at 3,000 notes; adds a second query layer and a parser version to keep in sync (D9) |
| FSEvents replay (`sinceWhen`) | needs FFI and the workspace forbids unsafe code; the re-list on open already catches offline changes (D9) |
| Cross-root `ATTACH` | no measured need ([roots spec §4](specs/roots.md)) |
| [Watchman](https://facebook.github.io/watchman/) clocks, background sweeps, `Freshness::Stale` | no measured need |
| Feature flags on `mdroots` | wait for a parser-only or wasm user |

## 4. Open questions

Each ends as a spec change or a decision.

1. **Severity thresholds** (§1.6): keep 98% / 80%, or derive them per root?
   Measure on more vaults first.
2. **Bare paths in prose.** In the research vault 192 of 193 bare path-like
   tokens that exist on disk are frontmatter values; in the zk vault 2 of
   2,423 exist. Resolve bare paths only in frontmatter values, or also in
   prose (gated on existence)?
3. **Org-mode depth.** In scope: links, headings, `:ID:`, `#+TITLE`, `#+LINK`.
   Open: agenda, `id:` links across roots, properties beyond `:ID:`.
4. **vcs-enumerated mode** (§1.4): is 500 ms the right budget, should the
   EdenFS glob API replace the `sl files` child process, and should
   `git ls-files` do the same for other lazy roots?
5. **Memory at scale** (§1.3): up to which vault size must the 35 MB target
   hold, and is that what triggers the derived tables?
6. **FSEvents replay cost.** After a day offline, replay may degrade to
   directories only (`MustScanSubDirs`, dropped events, `EventIdsWrapped`).
   Is it cheaper than today's re-list on open, and is it worth an FFI
   exception to `forbid(unsafe_code)`?
7. **`.mdrootsignore` vs `.ignore` plus a key in `.mdroots`.** The dedicated
   file exists because `.gitignore` answers a VCS question, folders without
   VCS lack one, and an empty file means "never index here". Folding it into
   `.ignore` (shared with [ripgrep](https://github.com/BurntSushi/ripgrep) and
   [fd](https://github.com/sharkdp/fd)) would drop one magic file name, but
   would also hide those files from those tools.
8. **Short binary alias.** `mw` and `mdr` are taken on crates.io. Find a
   candidate and run the D1 namespace checks.
9. **Name checks for `mdroots`.** npm and trademark (USPTO/EUIPO, classes 9
   and 42) are still unchecked (D1); crates.io, Homebrew, PyPI, Debian and
   GitHub were checked.
