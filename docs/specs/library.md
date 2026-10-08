# Spec: library, API, scheduling and editor integration

Related: [roots](roots.md), [index, links, frontmatter](index.md), [decisions](../DECISIONS.md) (D2, D3, D5, D6), [gopls practices](../research/gopls.md), [ramble](../research/ramble.md)

Two requirements:
1. Everything mdroots knows is usable from other Rust programs without speaking LSP.
2. It ships a working [Neovim](https://neovim.io) 0.12+ config ([`editors/nvim/`](../../editors/nvim/), smoke-tested).

## 1. Principle: the LSP server is a thin client of the library

[marksman](https://github.com/artempyanykh/marksman) (F#) and [zk](https://github.com/zk-org/zk) (Go `internal/` packages) put their core inside the server or CLI, so it can't be reused. In mdroots all behaviour lives in library crates that a TUI, CLI, static-site generator, MCP server or another language server can embed (e.g. a flashcard scanner, a semantic-search tool reusing the chunker and link graph, or ramble, a separate TUI markdown reader by the same author). See D2.

**`mdroots-lsp` target: ≤ 3k lines of protocol glue.** An estimate: zk is ≈ 14k non-test lines, and its LSP layer alone is 2.2–2.5k lines for fewer features.

Rule of thumb: if a feature can't be tested without JSON-RPC, it is in the wrong crate.

Process model (D3): every embedder links the library and calls it directly; no daemon, no RPC.

```text
nvim ──LSP── mdroots lsp ── Workspaces ─┐
nvim ──LSP── mdroots lsp ── Workspaces ─┼── roots/<id>.v<s>.db (SQLite WAL)
ramble ──────────────────── Workspaces ─┤     one writer per root: the flock holder
mdroots check ───────────── Workspaces ─┘     everyone else: read-only + own overlays
```

## 2. Workspace layout

```
mdroots/
  crates/
    mdroots-syntax/   parse one document → structure + liberal link candidates; LineIndex (no I/O)
    mdroots-resolve/  resolution ladder, dialect detection, convention vote (I/O only via ResolveEnv)
    mdroots-core/     traits (Store, FileSystem, ResolveEnv, …), MemStore, reconcile logic, Cancel
    mdroots-roots/    root discovery, FS classification, walk budgets (std::fs + statfs via rustix; no SQLite)
    mdroots-index/    SqliteStore + flock roles (rusqlite)
    mdroots/          facade: Workspace API, re-exports, feature flags   ← what embedders depend on
    mdroots-lsp/      LSP server binary + mdroots_lsp::serve() for embedding the server
  editors/nvim/       Neovim 0.12+ example config
  bench/              lspbench.py, mdsurvey.py, mdresolve.py, nvim_smoke.lua; criterion benches
  tests/corpus/       fixtures (synthetic, plus scrubbed shapes of two real vaults)
```

Dependency direction (no cycles, no upward edges):

```
syntax ← resolve ← core ← index ← mdroots ← mdroots-lsp
                    ↑                ↑
                    roots ───────────┘  (optional in the facade)
```

| Crate | Main deps | Direct I/O | wasm32 | Why separate |
|---|---|---|---|---|
| `mdroots-syntax` | `pulldown-cmark` (≈ 1.1 GB/s on the testbeds), `memchr`, small YAML/TOML frontmatter parser | — | ✓ | most reusable piece (linters, formatters, SSGs, browser editors); owns `LineIndex` |
| `mdroots-resolve` | `mdroots-syntax`, `unicode-normalization` | — (via `ResolveEnv`) | ✓ | other tools resolve links the same way over their own file list |
| `mdroots-core` | `mdroots-resolve` | only through `FileSystem` | ✓ (`MemStore`, embedder's `FileSystem`) | the traits every store and FS plugs into; `MemStore` and the reconcile queue written once |
| `mdroots-roots` | `mdroots-core`, `ignore` (gitignore matcher only), [`rustix`](https://github.com/bytecodealliance/rustix) (`statfs`; unix only) | ✓, all through its `Probe` trait | — | safe root finding is useful alone, e.g. to a search tool |
| `mdroots-index` | `mdroots-core`, `rusqlite` (bundled); flock via `std::fs::File::try_lock` (no `fs4`) | ✓ | — | heavy deps behind one crate; adds only `SqliteStore` and flock roles |
| `mdroots` | all of the above, behind features | — | partial | stable public surface |
| `mdroots-lsp` | `mdroots`, `lsp-server`, `lsp-types` | — | — | protocol only; the only crate naming `lsp-types` |

MSRV is Rust 1.89 (for `File::try_lock`/`lock_shared`). Lock files (`<id>.lock`, `<id>.open`, `discover.lock`) are in [roots](roots.md).

**Publishing:** crates are versioned independently with `cargo-semver-checks` per crate; unstable sub-crates ship as internal `0.x` pinned `=x.y.z` by the facade (D6).

### Feature flags on `mdroots`

| Feature | Default | Pulls in | C code | Threads | wasm32 |
|---|---|---|---|---|---|
| `roots` | ✓ | `mdroots-roots`: `ignore`, `rustix` (`statfs`) | — | none: the discovery walk is sequential | — |
| `index` | ✓ | `mdroots-index`, rusqlite bundled | SQLite | none of its own | — |
| `fts` | ✓ | SQLite FTS5 (needs `index`) | SQLite | — | — |
| `watch` | ✓ | macOS: `fsevent-sys` FFI (replay needs `sinceWhen`, which `notify` can't set); Linux: inotify via `notify` | — | one watcher thread, reconciler only | — |
| `parallel` | ✓ | `rayon` for the cold parse | — | a pool | — |
| `org` | ✓ | [org-mode](https://orgmode.org) parsing in `mdroots-syntax` | — | — | ✓ |
| `serde` | — | `serde` derives on public types | — | — | ✓ |

- `Options::background(true)` adds one background thread; needs `std::thread`, so not on `wasm32-unknown-unknown`.
- Without `parallel`: serial parsing. The discovery walk is sequential either way: it times each `readdir` for the rate check and lists only through the `Probe` ([roots](roots.md) §1 stage 4). Without `roots`: no `open_for(path)`; use `Workspace::open_at(root, opts)`.
- `full_text` without `fts` or on `MemStore` falls back to a naive case-insensitive scan through `FileSystem`, checking `&Cancel` between files; `ErrorKind::Unsupported` if the FS can't enumerate. For small vaults and tests only.
- `default-features = false` = `syntax + resolve + core + MemStore`: pure Rust, no `statfs` bindings, threads or discovery. That set plus `org` and `serde` builds for wasm32; the embedder supplies `FileSystem`.
- No `lsp-types` feature: `From` impls would tie `mdroots`'s major version to `lsp-types`'s. They live in `mdroots-lsp`; other servers convert from byte ranges and `LineIndex`.

## 3. Public API (sketch)

### 3.1 One document (`mdroots-syntax`)

```rust
use mdroots::syntax::{parse, Dialect, Element, Context};

let doc = parse(text, Dialect::detect_from_path(path));   // Markdown | Org
for el in doc.elements() {
    match el {
        Element::Heading(h) => println!("{} {}", h.level, h.text),
        Element::Link(l) if l.context == Context::Prose => println!("{:?} -> {}", l.kind, l.target.raw),
        Element::Link(l) => { /* in code/comment: mention, not a link */ }
        Element::Tag(t) => println!("#{}", t.name),
        _ => {}                                        // #[non_exhaustive]
    }
}
let fm = doc.frontmatter();            // Option<&Frontmatter>
fm.and_then(|f| f.title());            // standard keys normalised (title/aliases/id/tags/dates/…)
fm.map(|f| f.get("stage"));            // raw access for any key
```

- **Positions have one owner.** Ranges are byte offsets. `LineIndex` (lazy per document) converts to UTF-8/16/32 line/column. `mdroots-lsp` has no position code and no `line-index` dependency.
- The server negotiates `positionEncoding: "utf-8"` when offered (Neovim 0.12 lists it first); UTF-16 is the required fallback.
- `parse` is total: never fails or panics (fuzzed). Bad frontmatter becomes `Frontmatter::Invalid { range, error }`.
- Zero-copy where possible (`&str` slices; `into_owned()` for owned variants).

### 3.2 A workspace (`mdroots`)

```rust
use mdroots::{Workspace, Options, Freshness, Cancel};

let ws = Workspace::open_for(path, Options::default())?;      // classifies before any walk (roots spec)
println!("{} ({:?}, {})", ws.root().path.display(), ws.root().mode, ws.root().reason);
let ws = Workspace::open_for_nonblocking(path, opts);         // infallible; Lazy until discovery; Observer gets RootDecided

let notes  = ws.search_notes("verif", 20)?;                    // fuzzy title/stem/alias
let target = ws.resolve(&from_path, "[[some-note]]")?;         // Resolution { targets, step, ambiguous }
let back   = ws.backlinks(&note_path)?;                        // Vec<LinkRef { from, range, context }>
let tags   = ws.tags()?;                                       // Vec<(Tag, count)>
let broken = ws.diagnostics(&note_path, &cancel)?;             // point-fresh; same policy as the LSP
let hits   = ws.full_text("incorrectness logic", 10, &cancel)?;

match ws.freshness() {                                         // explicit; #[non_exhaustive]
    Freshness::Fresh => {}
    Freshness::Stale { pending } => {}
    Freshness::Lazy => {}
    _ => {}
}
ws.reconcile(Budget::time(Duration::from_millis(50)), &cancel)?; // a slice of work, when the embedder chooses
ws.wait_fresh(Duration::from_secs(2), &cancel)?;                // CLIs wanting complete answers

ws.set_overlay(&path, text);                                   // unsaved text; never written to the DB
ws.clear_overlay(&path);
let edits: WorkspaceEdit = ws.rename_note(&old, &new, &cancel)?; // link edits + RenameFile op
```

### 3.3 Embedder additions (`Workspaces`)

Driven by ramble's needs ([ramble](../research/ramble.md)). Synchronous, byte offsets and paths, `&Cancel` on slow calls.

```rust
impl Workspaces {
    /// Every link in the document (disk or overlay), resolved, in source order.
    pub fn document_links(&self, path: &Path, c: &Cancel) -> Result<Vec<DocLink>>;
    pub fn preview(&self, target: &Path, max_lines: usize) -> Result<Preview>;
    pub fn backlinks(&self, path: &Path, c: &Cancel) -> Result<Vec<Backlink>>;
    pub fn notes(&self, root: &Path) -> Result<Vec<NoteSummary>>;            // unranked, for local fuzzy filters
    pub fn notes_with_tag(&self, root: &Path, tag: &str) -> Result<Vec<NoteSummary>>;
    pub fn full_text(&self, root: &Path, q: &str, n: usize, c: &Cancel) -> Result<Vec<Hit>>;
    /// Read-only embedders: "this file changed on disk", re-read into the overlay.
    pub fn touched(&self, path: &Path);
    /// LinksChanged / Diagnostics / Freshness / Progress / FileChanged.
    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<Event>;
}

#[non_exhaustive] pub struct DocLink {
    pub range: Range<usize>, pub text_range: Range<usize>,
    pub kind: LinkKind,            // Md | Wiki | Org | RefLink | BarePath | Url | CodeMention
    pub context: Context,          // Prose | Heading | Frontmatter | Html | Code | Comment
    pub target: Option<PathBuf>, pub anchor: Option<String>, pub line: Option<u32>,
    pub status: LinkStatus,        // Resolved | Broken | Ambiguous(Vec<PathBuf>) | Unchecked | External
}
#[non_exhaustive] pub struct Backlink { pub from: PathBuf, pub from_title: String,
    pub range: Range<usize>, pub line: u32, pub in_code: bool }
#[non_exhaustive] pub struct NoteSummary { pub path: PathBuf, pub title: String,
    pub tags: Vec<String>, pub modified: Option<SystemTime> }
#[non_exhaustive] pub struct Preview { pub title: String, pub summary: Option<String>,
    pub frontmatter: Vec<(String, String)>, pub excerpt: String }
#[non_exhaustive] pub struct Hit { pub path: PathBuf, pub line: u32, pub snippet: String }
```

- Code mentions (`` `src/main.rs:12` ``): `:LINE[:COL]` is stripped into `DocLink.line`. Extra search dirs come through `Options` (ramble passes page dir, VCS root, tree root).
- `Options::write_cache(false)`: never touch the cache dir; in-memory index for the session (browsing someone else's tree).

### 3.4 Design rules

- **Synchronous core, no async runtime.** Queries are µs–ms SQLite lookups; async callers use `spawn_blocking`. Neither tokio nor rayon is forced on embedders.
- **Cancellation.** `reconcile`, `wait_fresh`, `full_text`, `diagnostics`, `rename_note` take `&Cancel`: a clonable `Arc<AtomicBool>` plus optional deadline, checked between files and batches. Cancelled calls return `ErrorKind::Cancelled` with no partial state (reconcile batches are committed transactions). The LSP cancels on `$/cancelRequest` and on a newer `didChange` for the same document.
- **Snapshot reads.** Each query uses one read transaction on one pooled connection and the overlay map as of call start (an `Arc` swapped on `set_overlay`). Concurrent commits or overlay changes are seen by the next query, never halfway.
- **Background work and roles (D3).** `Options::background` defaults to `false` for libraries, `true` in `mdroots-lsp`. The thread runs at `QOS_CLASS_BACKGROUND` (E-cores; Linux: `nice` + idle `ioprio`); only the link-target queue for open buffers runs at `UTILITY`. The holder of `<id>.lock` is the reconciler and does sweep, watcher and FTS; it is the root's only writer. Peers never write and serve their own unsaved and just-saved files from overlays.
- **`data_version` polling with `background(false)`.** No timer. Every query and `reconcile` checks `PRAGMA data_version` (≈ 1.2 µs), the DB inode and `meta.generation`, and reopens if changed. `reconcile(budget)` tries the flock with `try_lock`: on success it runs a sweep slice; otherwise it point-checks open files and their link targets in memory.
- **Cold start.** With no DB for the root, the first `open_for` indexes synchronously, open buffer first. The ~300 ms delay before background work applies only to sweeps of an existing DB ([index](index.md)).
- **Diagnostics are point-fresh.** `diagnostics(path)` checks the document and its link targets in this process (stat, re-parse if needed); whole-index freshness isn't required. A link target is `stat`ed before reporting broken: an existing gitignored or unindexed file is not broken. In lazy roots only `stat`-checkable links are diagnosed.
- **Never writes user files.** Mutations are returned as `WorkspaceEdit`. Only the cache dir (D5) is written; `Options::index(IndexMode::Memory)` (Neovim setting `index = 'memory'`) turns that off too.
- **`Send + Sync`, cheap to clone** (`Arc` inside); many threads query while one reconciles. `rusqlite::Connection` is `!Sync`, so `SqliteStore` holds a writer connection only while reconciler (mutex; every write `BEGIN IMMEDIATE`) and a capped read pool (default 2, bounded page cache each). Target: < 35 MB `phys_footprint` per process with 10 concurrent instances; private memory, not RSS, because macOS counts mmap'd DB pages in every process that touched them.
- **Multiple roots.** `Workspace` is one root; `Workspaces` maps paths to roots, handles nesting and caches handles. The LSP uses `Workspaces`.
- **Errors.** One `mdroots::Error` (thiserror) with `#[non_exhaustive] ErrorKind` (`Io`, `Cancelled`, `Unsupported`, `Corrupt`, …). Missing files and broken links are data, not errors. SQLite `BUSY` during WAL recovery is retried internally.
- **Semver hygiene.** Public structs `#[non_exhaustive]`, builder for `Options`, no `pub` fields on types expected to grow.

`#[non_exhaustive]` enums (callers need a `_` arm): `Element`, `LinkKind`, `Context`, `Dialect`, `Freshness`, `RootMode`, `ResolveStep`, `IndexMode`, `ChangeEvent`, `ErrorKind`. Exhaustive because the set is part of the model: `Confidence` (explicit / implicit / external, see [index](index.md)), `Role` (reconciler / peer), `PositionEncoding` (UTF-8 / 16 / 32).

### 3.5 Extension traits (`mdroots-core`)

```rust
pub trait FileSystem: Send + Sync {          // default StdFs; embedders: VFS, git tree, zip, test fakes
    /// Bytes plus the stat of the same fd (read, then fstat; retried if it differs
    /// from the pre-read stat). That stat is the row's version for the freshness fence.
    fn read(&self, p: &Path) -> io::Result<(Arc<[u8]>, Meta)>;
    fn stat(&self, p: &Path) -> io::Result<Meta>;          // ino, ctime_ns, mtime_ns, size, st_flags
    fn read_dir(&self, p: &Path) -> io::Result<Vec<DirEntry>>;
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf>;
    fn case_sensitive(&self, dir: &Path) -> bool;
    fn fs_kind(&self, dir: &Path) -> FsKind;               // Local | Virtual(EdenFS, …) | Remote, from statfs
}
pub trait ResolveEnv: Send + Sync {          // declared in resolve; core re-exports and implements it over Store + FileSystem
    fn exists(&self, p: &Path) -> bool;
    fn case_sensitive(&self, dir: &Path) -> bool;
    fn home_dir(&self) -> Option<&Path>;
}
pub trait Store: Send + Sync { /* doc facts, normalised-key lookups, change log */ }  // SqliteStore | MemStore
pub trait LinkResolver: Send + Sync {        // add a dialect step to the ladder
    fn resolve(&self, ctx: &ResolveCtx, target: &LinkTarget) -> Option<Resolution>;
}
pub trait Observer: Send + Sync { fn on_change(&self, ev: &ChangeEvent) {} }   // progress, file events, RootDecided
```

- `ResolveEnv` lives in `resolve` because `resolve` sits below `core`; embedders see it as `mdroots::core::ResolveEnv`.
- `FileSystem` returns bytes, not `str`, so invalid UTF-8 is indexed lossily instead of failing the batch.
- `FileSystem` makes the roots safety rules testable: "no readdir on a large monorepo checkout" runs against a counting fake.
- A custom `LinkResolver` adds house conventions (e.g. `[[wiki:Page]]`) without forking.

### 3.6 Embedding the server

```rust
mdroots_lsp::Server::builder()
    .workspaces(my_workspaces)                  // share library state with the host
    .command("myapp.publish", |ctx, args| { … })
    .serve(stdin(), stdout())?;                 // or any Read/Write pair, or crossbeam channels
```

Server behaviour (LSP layer, not library):
- **File events have one owner.** Only the reconciler acts on watcher events; a peer acts only on its own `didOpen`/`didChange`/`didSave`, so N editors don't mean N parses per save. `workspace/didChangeWatchedFiles` is registered only when reconciler and no native watcher runs (`watch` off, or lazy root).
- **Diagnostics** are published per document once it and its link targets were checked in this process (point-fresh). Peers and lazy roots publish too.
- **Completion** triggers: `[`, `(`, `#`, `:`. A `#` as first non-blank character of a line starts a heading: the server returns an empty list, so no popup.
- **Rename.** `textDocument/rename` on a note link or the note's H1 returns link edits plus a `RenameFile` op (as marksman does). `mdroots.renameFile <from-uri> <to-uri>` does the same and sends `workspace/applyEdit`. `workspace/willRenameFiles` is supported but not relied on (Neovim 0.12 never sends it).

## 4. Scheduling

gopls practices applied in-process ([gopls](../research/gopls.md)).

| Rule | Behaviour |
|---|---|
| Views | each overlay change and each observed `change_log` advance makes a new view (read txn + overlay map); queries on older views are cancelled via `Cancel` |
| In-order requests | requests from one editor run in arrival order, so a query sees the preceding `didChange`. Workspace symbols, full text, large reference queries and pull diagnostics opt out and run on a small worker pool |
| Two-phase diagnostics | phase 1 at once on the edited document (syntax, in-document anchors, links checkable from overlay and hot key cache); phase 2 cross-file, ~500 ms after the last edit, cancelled by the next edit. `diagnostics.trigger = "save"` skips phase 2 on change |
| Recent-mtime guard | in the cheap mtime scan, a file modified < 2 s ago is "maybe changed" even if mtime and size match |
| Read semaphore | ≤ 64 concurrent file reads per process, smaller budget per lazy/virtual root, so a root on [EdenFS](https://github.com/facebook/sapling) (the virtual filesystem from the Sapling project) can't starve a local vault |
| Early open | start opening the root's DB while answering `initialize` |
| No roots for navigation targets | goto/hover into a tree with no root yet is served single-file; discovery runs on `didOpen` |
| Progress | `workDoneProgress` (fallback `showMessage`) for discovery and indexing > 1 s |

The daemon question (D3) is reopened only with numbers: total `phys_footprint` with 10 editors plus ramble, CPU spent on duplicate overlay parsing, and p99 save-to-visible latency between peers.

## 5. Cache hygiene

- **Namespacing:** schema-versioned names (`roots/<id>.v<schema>.db`, `roots.v<k>.db`).
- **GC:** any process, at most hourly per base dir, under the `.open` lock rules. Deletes roots unseen for 30 days and old schema generations nobody holds open, keeps total size under a budget (default 1 GB), throttles stats.
- **Last-seen:** stamped by the reconciler at most hourly per root.
- **Errors:** a cache read error is a miss; parse the file, never fail the request.

## 6. CLI

`mdroots check [path]` (broken links, CI exit code), `mdroots roots` (what was chosen and why), `mdroots resolve <from> <link>`, `mdroots backlinks <note>`, `mdroots search <q>`, `mdroots lsp`. The server starts only via `lsp`; a bare `mdroots` with non-TTY stdin prints usage and exits, because an implicit server would hang under CI or cron. Each subcommand is ~20 lines over §3.2; as the first embedder, the CLI keeps the API honest. Single-file mode is `Workspace` over `MemStore` with one overlay, not a special path.

## 7. Work order

| Milestone | Scope |
|---|---|
| M1 (done) | `mdroots-syntax` with `LineIndex`, fuzzing, insta snapshot tests on the shapes of a ~730-note zk vault and a ~210-note research vault; `mdroots-core` + `mdroots-resolve` with the offline differential against zk's `notebook.db` and marksman ([M1 differential](../research/m1-differential.md)) |
| M2 (done) | `mdroots-roots`: stages 1–4 of [roots](roots.md) §1, loose roots, nested-root registry rules, the registry trait with an in-memory `MemRegistry` and `discover.lock`; safety tests on a counting probe with NFS and EdenFS fakes; discovery fixtures 1–11 |
| M3 | facade over `MemStore` + CLI (`check`, `resolve`, `roots`) |
| M4 | `mdroots-index` (SQLite, the SQLite root registry, reconcile, change log, flock roles) + churn tests and many-process fixtures 12–19 of [roots](roots.md) §7 |
| M5 | `mdroots-lsp`; the Neovim smoke test switches from the marksman stand-in to the real binary |

## Neovim 0.12+ example

Files in [`editors/nvim/`](../../editors/nvim/) (drop into your Neovim config dir):

| File | What |
|---|---|
| `lsp/mdroots.lua` | config auto-discovered by `vim.lsp.config`: `cmd = {'mdroots','lsp'}`, `filetypes = {markdown, org}`, a `reuse_client` predicate, `workspace_required = false`, a commented `settings = { mdroots = {…} }` block |
| `plugin/mdroots.lua` | `vim.lsp.enable('mdroots')` plus optional `LspAttach` extras: `gd`, `gO` (LSP symbols), guarded autotrigger completion, codelens, LSP folding, `<leader>ns` search, `<leader>nb` backlinks to loclist, `<leader>nr` rename note, `<leader>nn` new note from visual selection, `:MdrootsInfo` |

Choices (checked against the 0.12.5 runtime):

| Choice | Why |
|---|---|
| No `root_markers` | 0.12 starts a client for a matching filetype even with no root (root_dir nil; `workspace_required` defaults false). Root logic lives only in mdroots. The client then sends `workspaceFolders = null`, so the server never depends on workspace folders |
| `reuse_client` | with root_dir nil the default already reuses the client; the predicate matters only if something sets root_dir, keeping one process. A reused client sends no `didChangeWorkspaceFolders`, which is fine because mdroots finds roots from file paths. One process per Neovim; instances share the root's SQLite cache (D3) |
| `cmd = {'mdroots','lsp'}` | the subcommand is required (§6) |
| `settings`, not `init_options` | Neovim sends `settings` via `workspace/didChangeConfiguration` and answers `workspace/configuration` from it; `init_options` is sent once, so can't carry changing settings |
| `gO` remap | the markdown ftplugin maps `gO` to a treesitter outline; mapping `vim.lsp.buf.document_symbol` in `LspAttach` runs after the ftplugin |
| Backlinks handler | `Client:exec_cmd` drops the result without a handler; ours feeds `Location[]` to the loclist via `vim.lsp.util.locations_to_items(result, client.offset_encoding)` |
| Rename | `grn` on a link or H1 uses `textDocument/rename`; `<leader>nr` calls `mdroots.renameFile` |
| Completion | enabled only if `supports_method('textDocument/completion')`; nvim-cmp/blink.cmp users set `vim.g.mdroots_autocomplete = false` |
| Position encoding | Neovim offers `utf-8` first; mdroots picks it |
| Filetypes | `markdown`, `org`. Not `mdx` (no default filetype), `quarto` (`.qmd`) or `rmd` (`.Rmd`) for now |

Other built-in 0.12 mappings cover the rest: `K`, `grr`, `grn`, `gra`, `]d`/`[d`, `<C-]>` via `tagfunc`.

### Smoke test

```
nvim --clean --headless -u NONE -c 'luafile bench/nvim_smoke.lua'   # from the repo root
```

[`bench/nvim_smoke.lua`](../../bench/nvim_smoke.lua) loads `editors/nvim` with marksman standing in for the `mdroots` binary, read-only on the corpus vaults (edits go to a scratch note under `/tmp/mdroots-smoke/`), waiting on conditions rather than sleeps; every line is an assertion and failure gives a non-zero exit. It checks one client with root_dir nil and settings sent, maps and `:MdrootsInfo`, `gO` overriding the ftplugin, definition, symbols, backlinks via `exec_cmd` into the loclist (routed to `textDocument/references`), `[[#` heading completion, an applied code action, and unmodified vault buffers. It passes on NVIM 0.12.5 (attach ≈ 270–340 ms). Not covered until M5: cross-file resolution (marksman runs single-file without a folder), folding, rename, `mdroots.renameFile`, utf-8 negotiation, the `#`-at-line-start rule.
