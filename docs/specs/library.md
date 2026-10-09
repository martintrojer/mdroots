# Spec: library, API, scheduling and editor integration

Related: [roots](roots.md), [index, links, frontmatter](index.md), [decisions](../DECISIONS.md) (D2, D3, D5, D6), [gopls practices](../research/gopls.md), [ramble](../research/ramble.md)

Two requirements:
1. Everything mdroots knows is usable from other Rust programs without speaking LSP.
2. It ships a working [Neovim](https://neovim.io) 0.12+ config ([`editors/nvim/`](../../editors/nvim/), smoke-tested).

## 1. Principle: the LSP server is a thin client of the library

[marksman](https://github.com/artempyanykh/marksman) (F#) and [zk](https://github.com/zk-org/zk) (Go `internal/` packages) put their core inside the server or CLI, so it can't be reused. In mdroots all behaviour lives in library crates that a TUI, CLI, static-site generator, MCP server or another language server can embed (e.g. a flashcard scanner, a semantic-search tool reusing the chunker and link graph, or ramble, a separate TUI markdown reader by the same author). See D2.

**`mdroots-lsp` target: ≤ 3k lines of protocol glue.** It is ~2.1k non-test lines today. For comparison, zk is ≈ 14k non-test lines, and its LSP layer alone is 2.2–2.5k lines for fewer features.

Rule of thumb: if a feature can't be tested without JSON-RPC, it is in the wrong crate.

Process model (D3): every embedder links the library and calls it directly; no daemon, no RPC.

```text
nvim ──LSP── mdroots lsp ── Workspaces ─┐
nvim ──LSP── mdroots lsp ── Workspaces ─┼── roots/<id>.v<s>.db (SQLite WAL)
ramble ──────────────────── Workspaces ─┤     one writer per root: the flock holder
mdroots check ───────────── Workspace ──┘     everyone else: read-only
                                               each process: in-memory index + own overlays
```

## 2. Workspace layout

```
mdroots/
  crates/
    mdroots-syntax/   parse one document → structure + liberal link candidates; LineIndex (no I/O)
    mdroots-resolve/  resolution ladder, dialect detection, convention vote (I/O only via ResolveEnv)
    mdroots-core/     FileSystem, MemStore (in-memory index), markdown walk, diagnostics policy, Cancel
    mdroots-roots/    root discovery, FS classification, walk budgets, list_root (std::fs + statfs via rustix; no SQLite)
    mdroots-index/    cache dir, flock roles, per-root DB, reconcile, SQLite registry (rusqlite)
    mdroots/          facade: Workspace, Workspaces, re-exports   ← what embedders depend on
    mdroots-lsp/      the language server as a library: mdroots_lsp::serve()
    mdroots-cli/      the `mdroots` binary: check, roots, resolve, backlinks, search, lsp (§6)
  editors/nvim/       Neovim 0.12+ example config
  bench/              lspbench.py, mdsurvey.py, mdresolve.py, nvim_smoke.lua; criterion benches
  tests/corpus/       fixtures (synthetic, plus scrubbed shapes of two real vaults)
```

Dependency direction (no cycles, no upward edges):

```
syntax ← resolve ← core ← roots ← index ← mdroots ← mdroots-lsp ← mdroots-cli
```

Each crate may also use crates further left directly; `mdroots-cli` uses `mdroots` and `mdroots-lsp`.

| Crate | Main deps | Direct I/O | wasm32 | Why separate |
|---|---|---|---|---|
| `mdroots-syntax` | `pulldown-cmark` (≈ 1.1 GB/s on the testbeds), `memchr`, small YAML/TOML frontmatter parser | — | ✓ | most reusable piece (linters, formatters, SSGs, browser editors); owns `LineIndex` |
| `mdroots-resolve` | `mdroots-syntax`, `unicode-normalization` | — (via `ResolveEnv`) | ✓ | other tools resolve links the same way over their own file list |
| `mdroots-core` | `mdroots-resolve` | only through `FileSystem` | ✓ (`MemStore`, embedder's `FileSystem`) | the FS seam and the in-memory index every front end queries |
| `mdroots-roots` | `mdroots-core`, `ignore` (gitignore matcher only), [`rustix`](https://github.com/bytecodealliance/rustix) (`statfs`; unix only) | ✓, all through its `Probe` trait | — | safe root finding is useful alone, e.g. to a search tool |
| `mdroots-index` | `mdroots-core`, `mdroots-roots`, `rusqlite` (bundled, with [FTS5](https://sqlite.org/fts5.html)), `rustix` (`getuid`); flock via `std::fs::File::try_lock` (no `fs4`) | ✓ | — | heavy deps behind one crate: cache dir, flock roles, per-root DB with its full-text table, reconcile, registry, GC |
| `mdroots` | all of the above (no features yet), [notify](https://crates.io/crates/notify) 8 (the watcher), `unicode-normalization` | — | — | stable public surface |
| `mdroots-lsp` | `mdroots`, `lsp-server`, `lsp-types`, `serde_json`, `crossbeam-channel` | — | — | protocol only; the only crate naming `lsp-types` |
| `mdroots-cli` | `mdroots`, `mdroots-lsp`, `lsp-server` | — (through the facade) | — | the command line; formatting only |

MSRV is Rust 1.89 (for `File::try_lock`/`lock_shared`). Lock files (`<id>.lock`, `<id>.open`, `discover.lock`) are in [roots](roots.md).

**Publishing:** crates are versioned independently with `cargo-semver-checks` per crate; unstable sub-crates ship as internal `0.x` pinned `=x.y.z` by the facade (D6).

### Feature flags on `mdroots`

The facade has no features yet: it always depends on `mdroots-roots`, `mdroots-index` and `notify`. FTS5 and the notify watcher are built in, unconditionally; `parallel`, `org` as a switch and `serde` do not exist. The table is the target.

| Feature | Default | Pulls in | C code | Threads | wasm32 |
|---|---|---|---|---|---|
| `roots` | ✓ | `mdroots-roots`: `ignore`, `rustix` (`statfs`) | — | none: the discovery walk is sequential | — |
| `index` | ✓ | `mdroots-index`, rusqlite bundled | [SQLite](https://sqlite.org) | none of its own | — |
| `fts` | ✓ | SQLite FTS5 (needs `index`) | SQLite | — | — |
| `watch` | ✓ | `notify` (FSEvents on macOS, inotify on Linux); FSEvents replay would need `sinceWhen`, which `notify` can't set, so FFI (M8) | — | one watcher thread per watching workspace, reconciler only, with `Options::watch(true)` | — |
| `parallel` | ✓ | `rayon` for the cold parse | — | a pool | — |
| `org` | ✓ | [org-mode](https://orgmode.org) parsing in `mdroots-syntax` | — | — | ✓ |
| `serde` | — | `serde` derives on public types | — | — | ✓ |

- `Options::background(true)` adds one background thread; needs `std::thread`, so not on `wasm32-unknown-unknown`.
- Without `parallel`: serial parsing. The discovery walk is sequential either way: it times each `readdir` for the rate check and lists only through the `Probe` ([roots](roots.md) §1 stage 4). Without `roots`: no `open_for(path)`; use `Workspace::open_at(root, opts)`.
- `full_text` without a DB written by this process (a peer, memory mode, `open_at`) uses a naive scan of the in-memory notes with the same matching rules, checking `&Cancel` between notes (§3.2).
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

The facade crate over `mdroots-roots`, `mdroots-index` and `MemStore`. Public paths are absolute and canonical (through the workspace's `FileSystem`); a path outside the root is an `Unsupported` error. Everything is synchronous; the only background thread is the opt-in watcher (`Options::watch`). An embedder that must not block on discovery serves the file with `open_single` and runs `open_for` on its own thread, as `mdroots lsp` does (§3.6).

```rust
use mdroots::{Workspace, Workspaces, Options, IndexMode, Freshness, Cancel};
use mdroots::syntax::PositionEncoding;

let ws = Workspace::open_for(path, Options::default())?;      // discovery first (roots spec), then index
let ws = Workspace::open_at(&root_dir, Options::default())?;  // a directory as the root; no discovery, in memory
let ws = Workspace::open_single(&file, Options::default())?;  // the file alone; no discovery, cache or registry
let r = ws.root();                                             // RootInfo { path, mode, reason, nested_roots }
println!("{} ({:?}, {})", r.path.display(), r.mode, r.reason);
let role = ws.role();                                          // Some(Role::Reconciler | Role::Peer); None = memory
let db = ws.cache();                                           // Some(<cache>/roots/<id>.v2.db); None = memory
ws.refresh(&cancel)?;                                          // pick up changes on disk (§3.4)
let changed = ws.refresh_paths(&paths, &cancel)?;              // Vec<PathBuf>: just these paths; what changed
let on = ws.watching();                                        // a native watcher runs (Options::watch)
let rx = ws.subscribe();                                       // mpsc::Receiver<Vec<PathBuf>>: watcher changes

let files  = ws.files();                                       // Vec<PathBuf>, absolute, sorted
let notes  = ws.notes();                                       // Vec<NoteSummary>, by path
let tagged = ws.notes_with_tag("rust");                         // Vec<NoteSummary>, by path; tag compared case-insensitively
let found  = ws.search_notes("qry", 50);                       // Vec<NoteSummary>, fuzzy, best first
let hits   = ws.full_text("some words", 50, &cancel)?;         // Vec<Hit>, full-text, every word must appear
let tags   = ws.tags();                                        // Vec<(String, usize)>, sorted by name
let target = ws.resolve(&from_path, "[[some-note]]")?;         // Resolution { targets, step, status, hint }
let goto   = ws.goto(&note_path, offset)?;                     // Option<Goto { targets, heading, line }>
let links  = ws.document_links(&note_path)?;                   // Vec<DocLink>, source order
let back   = ws.backlinks(&note_path)?;                        // Vec<Backlink>, sorted by source path
let heads  = ws.outline(&note_path)?;                          // Vec<syntax::Heading>, source order
let prev   = ws.preview(&note_path, 10)?;                      // Preview { title, frontmatter, excerpt }
let text   = ws.text(&note_path)?;                             // current text, overlay wins
let diags  = ws.diagnostics(&note_path, &cancel)?;             // Vec<Diagnostic>, same policy for every front end
let edit   = ws.rename_note(&old, &new, &cancel)?;             // WorkspaceEdit { edits, rename, create }; writes nothing
let edit   = ws.extract_note(&note_path, range, &cancel)?;     // WorkspaceEdit: create a note from the bytes `range`, link it
let style  = ws.link_style();                                  // LinkStyle: how new links are written in this root
let link   = ws.link_to(&from, &target, None)?;                // the link text from `from` to `target` (may not exist yet)
let fm     = ws.frontmatter_range(&note_path)?;                // Option<Range<usize>>: the frontmatter block
let counts = ws.anchor_backlinks(&note_path)?;                 // Vec<(heading index, links naming it by anchor)>
let links  = ws.heading_backlinks(&note_path, 2)?;             // Vec<Backlink>: links naming outline()[2] by anchor
let (line, col) = ws.line_col(&note_path, offset, PositionEncoding::Utf32)?; // 0-based, overlay text wins

match ws.freshness() {                                         // #[non_exhaustive]
    Freshness::Fresh => {}                                     // every note of the root is indexed
    Freshness::Lazy => {}                                      // a working set only (lazy, single-file)
    _ => {}
}

ws.set_overlay(&path, text)?;                                  // unsaved text; adds the note if not indexed
ws.clear_overlay(&path)?;

let wss = Workspaces::new(Options::default());                 // one per process; shares cache dir and registry
let ws = wss.for_path(&file)?;                                 // cached per root; nested roots: the nearest
let ws = wss.get(&file);                                       // Option<Workspace>: cached only, never opens
let opts = wss.options();                                      // the Options every workspace is opened with
let open = wss.all();                                          // every opened workspace, by root path
```

- **`Options`** is a builder with `Default`: `workspace_folders` (bound the marker climb), `enumerator` (lists the markdown of a virtual checkout; default `SlFiles`, `NoEnumerator` turns vcs-enumerated mode off), `cancel` (checked during discovery and indexing), `fs` + `probe`, set together or not at all (default `StdFs` and `StdProbe`; unix only, elsewhere the embedder supplies both), `index(IndexMode)`, `cache_dir(path)` and `watch(bool)` (default off).
- **`IndexMode`** (`#[non_exhaustive]`): `Auto` (default) keeps a per-root DB in the cache dir: `cache_dir` if set, else the user's (D5), else memory. `Auto` with an explicit `fs`/`probe` (in-memory test trees) and no `cache_dir` stays in memory. `Memory` never touches the cache dir. A cache that fails to open falls back to memory.
- **What `open_for` indexes.** The file must exist. Every mode but lazy and single-file: the notes discovery listed (or `list_root` re-lists on a registry hit), plus the opened file (`Fresh`). Lazy roots: a working set, the opened file plus the notes of its directory, one level, no hidden, editor-temp or dataless entries, at most 2,000 files by name with the opened file counted (`Lazy`). Single-file decisions, and a file outside the decided root, index the file alone with its directory as root (`Lazy`). The opened file is always read, even if dataless.
- **With a cache**, `open_for` holds `discover.lock` around discovery with the persistent registry; a registered root then opens its DB and takes its locks. The reconciler reconciles and writes, a peer reads the rows and re-reads changed files in memory ([index](index.md) §1.3). Either way the process hydrates its `MemStore` from the returned bytes, so unchanged notes are not opened. Single-file workspaces, `open_at` and roots without a registry row stay in memory.
- **`refresh(&cancel)`**: a peer first tries to become the reconciler; the reconciler re-lists the root (or the working set) and writes what changed; a peer re-reads the DB and changed files without writing; in memory mode the root is re-listed and re-read. The new index replaces the old one; overlays survive. It also stamps the root as seen (at most hourly) and, when the registry names another DB file than the one held (another process rebuilt a corrupt DB), reopens that file first ([index](index.md) §1.6).
- **`refresh_paths(&paths, &cancel)`**: the same for just `paths` (absolute; outside the root ignored): a note is re-read, added or dropped; an existing directory adds the notes under it (hidden and pruned dirs skipped) and re-checks the indexed ones; a gone path drops every indexed note at or under it. The reconciler writes the changes (a peer first tries to become it). The store is patched in place; overlays survive. A single-file workspace re-checks only its file, a lazy one its working-set directory and indexed notes. Returns the absolute paths whose content changed, sorted. If the DB file was replaced, a full `refresh` runs instead and every indexed note is returned.
- **`Options::watch(true)`** starts a native watcher for a workspace that is the reconciler of a DB-backed marker, VCS or loose root on a local filesystem, never otherwise; `watching()` says whether one runs. It point-refreshes the paths it sees changed ([index](index.md) §1.3) and stops when the last clone of the workspace drops. A peer promoted on `refresh` starts watching then.
- **`subscribe()`**: a channel of the absolute paths each watcher-driven refresh changed (a lost-events rescan sends every indexed note). Explicit `refresh` and `refresh_paths` calls send nothing; their caller already knows. Closed receivers are dropped.
- **`open_single(path, opts)`** indexes one existing file alone, with no discovery, cache dir, registry or discovery lock: root = its directory, mode `SingleFile`, reason `single-file: opened without discovery`, freshness `Lazy`, never watching. It costs the file's own parse, so it can serve a file at once while `open_for` runs elsewhere. Not cached in any `Workspaces`.
- **`open_at`** walks the directory with the M1 markdown walk (no budget, no filesystem classification), always in memory, and reports `RootMode::Marker` with reason `opened at <dir>`. It is for a directory the caller already knows is a bounded root.
- **`resolve`** parses `link_text` with the dialect of `from`, takes its first link and resolves it with goto semantics (the Partial step allowed; `hint` is true for a Partial hit). Text without a link is an `Unsupported` error. `targets` may lie outside the root.
- **`goto(path, offset)`**: the link at a byte offset of the note's current text, in any context (code included), resolved like `resolve`. `None` without a link there or without a target (broken, external). `heading` is the byte range of the heading an anchor names (slug, org `:ID:` or `:CUSTOM_ID:`), in `targets[0]`; `line` is the 1-based line of a code mention `path:LINE`.
- **`search_notes(query, limit)`**: notes whose title, file stem or a frontmatter alias contains the query as a case-insensitive subsequence, best first (exact, prefix, substring, then fewer gaps; ties by path). An empty query lists notes by path.
- **`full_text(query, limit, &cancel)`**: notes whose text contains every term of `query`, at most `limit`, as `Hit`s (§3.3). `query` is plain text: split on whitespace, terms without a letter or digit dropped; matching is case-insensitive, diacritics folded (`cafe` finds `café`), on runs of letters and digits; a term of several tokens (`a-b`) matches as a phrase; the last term also matches as a prefix. A query without terms finds nothing. Two paths with the same rules: the reconciler asks the DB's FTS5 table ([index](index.md) §1.2) and takes its ranked hits for notes without an overlay; a naive scan of the current text covers notes with an overlay, and covers every note for a peer (whose DB may lag the text it serves), in memory mode and after `open_at`. Result order: the DB's hits by rank, then the naive hits by path.
- **`preview`** returns the title, frontmatter entries in document order (lists joined with `", "`) and the first `max_lines` lines after the frontmatter. **`preview`, `text`, `line_col`** are `Unsupported` for a note that is not indexed; **`outline`, `document_links`, `backlinks`, `diagnostics`** return empty for it. `Backlink.line` is 0-based; `from_title` is the frontmatter title, else the first level-1 heading, else the file stem.
- **`rename_note(old, new, &cancel)`**: `old` must be an indexed note and `new` a note path inside the root that neither exists nor is indexed. Links whose best target is `old` are rewritten in the style they were written in (file-relative, root-relative, site-rooted or by stem); links found by id, title, alias, a dialect transform or a partial match are left alone, as are links in code. When `old` moves to another directory its own file-relative links are rewritten too. Anchors are kept. `WorkspaceEdit.edits` is per file (absolute, sorted) with non-overlapping byte-range `TextEdit`s, then `rename = Some((old, new))`; `create` is empty. Nothing is written.
- **`link_style()`**: the root's `LinkStyle` (§3.3) for inserted links: an existing zk or [Obsidian](https://obsidian.md) config wins, else the root's convention vote, else Markdown links relative to the file with `.md` ([index](index.md) §3.2). The vote is cached with the other whole-root caches.
- **`link_to(from, target, label)`**: the link text to insert in `from` pointing at `target` (inside the root; it need not exist) in that style. Without a label a wiki link is `[[target]]` and a Markdown link uses the target's title (the indexed note's, else its stem). `[[stem]]` becomes `[[dir/stem]]` when another note shares the stem (compared case-insensitively). Markdown paths have `%`, space, `(` and `)` percent-encoded; with `md_suffix: false` the extension is dropped. A wiki target containing `|`, `]` or a line break, a wiki label containing `]]` or a line break, and a path outside the root are `Unsupported`.
- **`extract_note(from, range, &cancel)`**: moves the bytes `range` of the indexed Markdown note `from` (current text) into a new note and replaces them with a `link_to` it. The title is the heading's text when the selection starts at the start of a heading's line, else the first non-blank line (trimmed, at most 60 characters), else `Untitled`. The file is `<GitHub slug of the title>.<from's extension>` (`untitled` for an empty slug) in `from`'s directory, with `-2`, `-3`, … appended while the name exists on disk or in the index, checked on every call. Its content is the selection after a `# <title>` line (left out when the selection starts with a heading), ending with a line break. The result has `create = [(new, content)]` and one edit in `from`; `rename` is `None`. Org notes, an empty or invalid range and a note that is not indexed are `Unsupported`. Nothing is written.
- **`frontmatter_range`** is the frontmatter block's byte range, `None` without one. **`anchor_backlinks`** counts, per heading (its index in `outline`), the links from other notes whose anchor names it, matched as `goto` matches (slug, GitHub slug, heading id or custom id; as written and percent-decoded; the first matching heading); headings without any are left out. **`heading_backlinks`** lists those links for one heading, sorted by source path.
- **Diagnostics** are computed per call under the root's `DiagnosticPolicy` (the policy is in [index](index.md) §3.3), with `lazy` set when freshness is `Lazy`. The policy is computed once per store (`MemStore::policy`) and dropped on the next content change, as is the backlink index.
- **`Workspaces`** maps files to workspaces, opening each root once and sharing one cache dir and registry. A file is served by the cached workspace with the longest root containing it and not under one of its nested roots; a single-file or rootless lazy workspace serves only its own file. `get(file)` is the same lookup without opening anything (`None` if nothing cached serves the file); `options()` returns the shared options, e.g. for `open_single`. `Clone + Send + Sync`.
- The working set never grows after open. Only the cache dir is written.
- Re-exports: `Hit`, `LinkStyle`, `FsStat` and `MountInfo` (for a custom `Probe`), `Cancel`, `Error`, `ErrorKind`, `FileSystem`, `StdFs`, `Diagnostic`, `DiagCode`, `Severity`, `RootMode`, `Probe`, `StdProbe`, `Enumerator`, `NoEnumerator`, `ResolveStep`, `LinkStatus`, `Role`, `mdroots_index` as `mdroots::index`, `mdroots_syntax` as `mdroots::syntax`; `mdroots::names` gives the lowercase-hyphenated names of modes, severities, steps and statuses that the CLI and the server print.

Planned (M8): `Freshness::Stale { pending }`, background reconcile, feature flags, and working-set growth as files are opened. A library-level non-blocking open is not planned separately: `open_single` plus `open_for` on the embedder's thread covers it.

### 3.3 Embedder additions

Driven by ramble's needs ([ramble](../research/ramble.md)). Synchronous, byte offsets and paths, `&Cancel` on slow calls.

- **Exists now** on `Workspace` (one root): `document_links`, `backlinks`, `notes`, `notes_with_tag`, `search_notes`, `full_text`, `preview`, `outline`, `text`, `goto`, `rename_note`, `extract_note`, `link_style`, `link_to`, `frontmatter_range`, `anchor_backlinks`, `heading_backlinks`, `refresh`, `refresh_paths`, `watching`, `subscribe`, and `Workspace::open_single`; and `Workspaces::for_path` / `get` / `all` to get the workspace of any file. `NoteSummary.modified` is the file's mtime (`None` for an unsaved note); `Preview` has no `summary`. `refresh_paths` is what a read-only embedder calls for "this file changed on disk".
- **Planned:** `Preview.summary` and typed `subscribe` events (today it sends changed paths only).

```rust
#[non_exhaustive] pub struct Hit {
    pub path: PathBuf,             // absolute
    pub line: u32,                 // 0-based: the first line containing the first query term in the current text; 0 if none
    pub snippet: String,           // matching text on one line, whitespace runs collapsed, at most 120 characters
}

#[non_exhaustive] pub struct DocLink {
    pub range: Range<usize>, pub text_range: Range<usize>,
    pub kind: LinkKind,            // Markdown | Reference | Autolink | Image | Wiki | WikiEmbed | Org | BarePath | Url | CodeMention | Html | Templating | Footnote
    pub context: Context,          // Prose | Heading | Frontmatter | Html | CodeBlock | InlineCode | Comment
    pub target: Option<PathBuf>,   // best target when Resolved, Ambiguous or Unindexed
    pub anchor: Option<String>,    // text after the first `#` as written (`^b` for a block); None if empty
    pub line: Option<u32>,         // `path:LINE` of a code mention
    pub status: LinkStatus,        // Resolved | Ambiguous | Unindexed | Broken | External | Unchecked
}
#[non_exhaustive] pub struct Backlink { pub from: PathBuf, pub from_title: String,
    pub range: Range<usize>, pub line: u32, pub in_code: bool }
#[non_exhaustive] pub struct NoteSummary { pub path: PathBuf, pub title: String, pub tags: Vec<String>,
    pub modified: Option<SystemTime> }   // file mtime; None for an overlay-only note or a failed stat
#[non_exhaustive] pub struct Preview { pub title: String,
    pub frontmatter: Vec<(String, String)>, pub excerpt: String }
#[non_exhaustive] pub struct Goto { pub targets: Vec<PathBuf>, pub heading: Option<Range<usize>>, pub line: Option<u32> }
#[non_exhaustive] pub struct WorkspaceEdit {
    pub edits: Vec<(PathBuf, Vec<TextEdit>)>,   // per file, absolute, sorted; ranges sorted, not overlapping
    pub rename: Option<(PathBuf, PathBuf)>,     // the file rename after the edits
    pub create: Vec<(PathBuf, String)>,         // files to create (absolute, not existing) with content, before the edits
}
pub struct TextEdit { pub range: Range<usize>, pub new_text: String }
#[non_exhaustive] pub enum LinkStyle {   // mdroots_resolve::dialect, re-exported
    WikiStem,                            // [[stem]]
    WikiPath,                            // [[dir/stem]], root-relative
    MarkdownRelative { md_suffix: bool },     // [label](../dir/note.md), relative to the linking file
    MarkdownRootRelative { md_suffix: bool }, // [label](dir/note.md), relative to the root
}
```

- `LinkStatus::Ambiguous` carries no candidates; `Resolution.targets`, `Goto.targets` and the diagnostic's `related` list them, best first.
- Code mentions (`` `src/main.rs:12` ``): `:LINE[:COL]` is stripped into `DocLink.line`. Planned: extra search dirs through `Options` (ramble passes page dir, VCS root, tree root).
- `Options::index(IndexMode::Memory)`: never touch the cache dir; in-memory index for the session (browsing someone else's tree).

### 3.4 Design rules

- **Synchronous core, no async runtime.** Queries are µs–ms lookups in the in-memory index (D9); async callers use `spawn_blocking`. Neither tokio nor rayon is forced on embedders.
- **Cancellation.** `refresh`, `refresh_paths`, `full_text`, `diagnostics`, `rename_note`, `extract_note` (and `Options::cancel` for `open_for`) take a `Cancel`: a clonable `Arc<AtomicBool>` plus optional deadline, checked between files and batches. Cancelled calls return `ErrorKind::Cancelled` with no partial state (the DB keeps only whole committed batches). The server's use of it is in §3.6.
- **Consistent reads.** Each query reads the workspace's `MemStore` under one read lock, overlays included. `refresh` builds a new `MemStore` outside the store lock and swaps it in; `refresh_paths` patches it under the write lock. A query sees the old or the new index, never half of each. Both hold the workspace's index lock until the store is updated, so they never interleave (lock order: index, overlays, store).
- **Roles (D3).** The holder of `<id>.lock` is the reconciler, the root's only writer; it reconciles on open, `refresh` and `refresh_paths`, and only it may watch. Peers never write; they serve the DB's content, files they re-read themselves, and their own unsaved buffers. `Workspace::role()` tells which.
- **Background work is opt-in.** Without `Options::watch(true)` nothing runs between calls: changes on disk arrive through `refresh` and `refresh_paths`. With it, a watching workspace runs one thread (`mdroots-watch`) that calls `refresh_paths`; it holds only a weak reference, so it never keeps a workspace or the process alive. `mdroots lsp` sets it; libraries default to off. The server's background open (§3.6) is its own thread, not the library's. Planned (M8): `Options::background` with one thread at `QOS_CLASS_BACKGROUND` (Linux: `nice` + idle `ioprio`) for sweeps, and `PRAGMA data_version` / `meta.generation` checks per query.
- **Cold start.** With no DB for the root, the first `open_for` indexes synchronously and writes the DB in committed batches; an existing DB means only changed files are read ([index](index.md) §1.5).
- **Diagnostics are point-fresh.** `diagnostics(path)` checks the document and its link targets in this process; whole-index freshness isn't required. A link target is `stat`ed before reporting broken: an existing gitignored or unindexed file is not broken. In lazy roots only `stat`-checkable links are diagnosed.
- **Never writes user files.** Mutations are returned as `WorkspaceEdit`. Only the cache dir (D5) is written; `Options::index(IndexMode::Memory)` turns that off too.
- **`Send + Sync`, cheap to clone** (`Arc` inside); many threads query while one refreshes. `rusqlite::Connection` is `!Sync`, so a workspace keeps its one DB connection and its locks behind a mutex, used by open, `refresh`, `refresh_paths` and a reconciler's `full_text` (every write `BEGIN IMMEDIATE`). Target: < 35 MB `phys_footprint` per process with 10 concurrent instances; private memory, not RSS, because macOS counts mmap'd DB pages in every process that touched them. Measured peak footprint 23–31 MB on a synthetic 3,000-note notebook (D9, [ROADMAP](../ROADMAP.md)).
- **Multiple roots.** `Workspace` is one root; `Workspaces` maps paths to roots, handles nesting and caches handles. The LSP uses `Workspaces`.
- **Errors.** One `mdroots::Error` (kind + message) with `#[non_exhaustive] ErrorKind` (`Io`, `Cancelled`, `Unsupported`, `Corrupt`, …). Missing files and broken links are data, not errors. SQLite `BUSY` waits up to `busy_timeout` (2 s); a DB of another schema or a corrupt one is `Corrupt` inside the index layer; the facade rebuilds a corrupt DB into a new generation (reconciler) or serves from memory (peer), so `open_for` does not fail on it ([index](index.md) §1.6).
- **Semver hygiene.** Public structs `#[non_exhaustive]`, builder for `Options`, no `pub` fields on types expected to grow.

`#[non_exhaustive]` enums (callers need a `_` arm): `Element`, `LinkKind`, `Context`, `Dialect`, `Freshness`, `RootMode`, `ResolveStep`, `IndexMode`, `ErrorKind`. Exhaustive because the set is part of the model: `Confidence` (explicit / implicit / external, see [index](index.md)), `Role` (reconciler / peer), `PositionEncoding` (UTF-8 / 16 / 32).

### 3.5 Extension traits

```rust
pub trait FileSystem: Send + Sync {          // mdroots-core; default StdFs; embedders: VFS, git tree, zip, test fakes
    /// The file's bytes and its metadata, taken from the same open file
    /// (the version reconcile stores, index spec §1.2).
    fn read(&self, p: &Path) -> io::Result<(Arc<[u8]>, Meta)>;
    fn stat(&self, p: &Path) -> io::Result<Meta>;          // ino, ctime_ns, mtime_ns, size, is_dir, is_file, dataless
    fn read_dir(&self, p: &Path) -> io::Result<Vec<(String, Meta)>>;
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf>;
    fn case_sensitive(&self, dir: &Path) -> bool;
    fn fs_kind(&self, dir: &Path) -> FsKind;               // Local | Virtual | Remote | Cloud | Unknown
}
pub trait ResolveEnv: Send + Sync {          // mdroots-resolve; core implements it over MemStore + FileSystem
    fn exists(&self, root_rel: &str) -> bool;
    fn is_file(&self, root_rel: &str) -> bool;
    fn case_sensitive(&self) -> bool;
    fn home_dir(&self) -> Option<&Path>;
    fn read_config(&self, root_rel: &str) -> Option<String>;   // small tool configs, e.g. .zk/config.toml
}
pub trait Probe: Send + Sync { /* stat, lstat, mount, read_dir, read_link, read_small, … */ }  // mdroots-roots: discovery I/O
pub trait Enumerator: Send + Sync {          // mdroots-roots: lists a vcs-enumerated root (SlFiles, NoEnumerator)
    fn md_paths(&self, root: &Path, budget: Duration, cap: usize) -> Option<Vec<String>>;
}
```

- `ResolveEnv` lives in `resolve` because `resolve` sits below `core`.
- `FileSystem` returns bytes, not `str`, so invalid UTF-8 is indexed lossily instead of failing the batch.
- `FileSystem` and `Probe` make the roots safety rules testable: "no readdir on a large monorepo checkout" runs against a counting fake.
- Planned: a `Store` trait once the derived tables (D9) give a second store, a `LinkResolver` trait to add house conventions (e.g. `[[wiki:Page]]`) to the ladder without forking, and an `Observer` for progress and file events.

### 3.6 Embedding the server

```rust
let (conn, io) = lsp_server::Connection::stdio();      // or Connection::memory() in tests
mdroots_lsp::serve(conn)?;                             // default Options
// mdroots_lsp::serve_with(conn, Options::default().cache_dir(dir))?;  // explicit options
io.join()?;
```

`serve` runs until `exit` (an `exit` without `shutdown` is an `Err`) or disconnect. `mdroots lsp` (§6) is `serve_with` on stdio with the CLI's options. Workspace folders from `initialize` are added to the options (they bound the marker climb). Planned: a builder to share an embedder's `Workspaces` and add commands.

Server behaviour as built (LSP layer, not library):
- **One thread, in order.** Requests are answered one at a time in arrival order, synchronously, on the message-loop thread, so each sees the edits before it. Root discovery and indexing never run on it (background open, below). Before running the next request the server reads every message already sent: a `$/cancelRequest` for a queued request makes it answer `RequestCanceled`, and a `didChange` cancels the queued requests on that document (they would answer on stale text). A running request is not interrupted.
- **Handshake.** `positionEncoding` is `utf-8` when the client offers it, else UTF-16. `lsp-server`'s `initialize_finish` consumes the client's `initialized`, so the server registers its watcher right after `initialize`: `client/registerCapability` for `workspace/didChangeWatchedFiles` on `**/*.{md,markdown,org}`, only when the client offers dynamic registration for it.
- **Document sync.** Full sync. `didOpen`/`didChange` set the document's text as its workspace's overlay; `didClose` clears the overlay and the diagnostics. A buffer with no file on disk gets no workspace and no diagnostics.
- **Background open.** On `didOpen` of a file no open workspace serves (`Workspaces::get`), the server gives the document a single-file workspace at once (`Workspace::open_single`, with the server's options), publishes its diagnostics from it, and queues `Workspaces::for_path` for the file on one opener thread (`mdroots-lsp-open`; serial, one open at a time, a file queued once until done). When the open finishes, every document still served single-file whose root is now open moves onto that workspace (its text re-set as the overlay there), the diagnostics of the open documents of that root are re-published, and code lenses are refreshed. A failed open leaves the document single-file. Requests for a note that is not open use the open workspace serving it, else `open_single`; they never open a root. Measured on a cold synthetic 3,000-note root: first diagnostics (single-file) after 9 ms, the root's after 2.8 s. A single huge note still costs its own parse on the loop thread.
- **Progress.** An open still running after 1 s is announced: with `window.workDoneProgress` the server sends `window/workDoneProgress/create` and a `$/progress` begin titled `mdroots: indexing <dir>`, where `<dir>` is the opened file's directory (the root is unknown until discovery finishes), and the end when the open finishes. Clients without progress get one `window/showMessage` per session instead. Nothing is announced after `shutdown`.
- **Refresh and the native watcher.** `mdroots lsp` passes `Options::watch(true)`; `serve_with` uses the options as given (in-process embedders and tests choose). `didSave` refreshes the saved document's workspace (a single-file one is refreshed and its document re-published); `didChangeWatchedFiles` refreshes every workspace containing a changed path that is not watching (a watching workspace already follows the disk, so the disk has one owner). Each process refreshes on its own events: the reconciler writes what changed, a peer only re-reads. Then the diagnostics of every open document in those workspaces are re-published and code lenses refreshed.
- **Watcher changes.** On `didOpen`/`didChange` the server subscribes once to each watching workspace; a forwarding thread per root turns its notifications into the root path on a channel that the message loop selects next to the client connection. Each notification re-publishes the open documents of that root, so a link target created by another program clears its broken-link diagnostic without a save (about 226 ms measured). A peer does not watch; it follows the disk on save and on the client's watched-file events, as before.
- **Diagnostics** are published on `didOpen` and after a refresh at once, and 500 ms after the last `didChange` of a document; nothing after `shutdown`. The `mdroots.diagnostics` setting (`auto`, `off`, `hint`, `warn`, `error`; from `workspace/didChangeConfiguration`) turns them off or sets the severity of broken links and anchors.
- **Requests.** `definition` (`Workspace::goto`; an anchor jumps to its heading, a code mention to its line), `references` (backlinks of the link's target, or of this note off a link), `hover` (title and first 10 lines of the target, or "broken link"), `documentSymbol` (headings nested by level), `workspace/symbol` (`search_notes` over every open workspace, at most 100), `completion`, `prepareRename`/`rename`, `foldingRange`, `codeLens`, `codeAction`, `documentLink` (one per link with a target path, over the whole link: the target's `file:` URI with `#anchor` as written, or `#L<line>` for a code mention with a line; broken links and external URLs get none; no `documentLink/resolve`), `executeCommand`.
- **Folding ranges.** One per heading section, from the heading's line to the line before the next heading of the same or a higher level (or the end), and the frontmatter block as kind `region`; one-line ranges are left out. Fenced code blocks are not folded (the parse does not expose block ranges).
- **Code lenses** come resolved (no `codeLens/resolve`): `N backlinks` on the title line (the first level-1 heading, else line 0) running `mdroots.backlinks`, and `N links` on every other heading other notes name by anchor (`Workspace::anchor_backlinks`) running `mdroots.anchorLinks`. No lens for a count of 0. After a refresh, a watcher-driven re-publish or a finished background open the server sends `workspace/codeLens/refresh`, only to clients that advertise `workspace.codeLens.refreshSupport`.
- **Code action: extract note.** On a non-empty selection in a Markdown note, `textDocument/codeAction` returns one action of kind `refactor.extract.note`, titled `Extract to new note: <file name>`, from `Workspace::extract_note`. Its edit is `documentChanges`, in order: `CreateFile` (`overwrite: false`, `ignoreIfExists: false`), a `TextDocumentEdit` inserting the new note's content, and a `TextDocumentEdit` replacing the selection with the link. The server writes nothing; the client applies the edit. The name is re-checked against the disk on every request, because Neovim truncates an existing file when it applies a `CreateFile`, whatever its options say. `context.only` is honoured (`refactor` and `refactor.extract` match); an empty selection, an Org note, an unknown file or another kind gets `[]`.
- **Completion** triggers: `[`, `(`, `#`, `:`. After `[[`: notes by fuzzy search (stem inserted, title as detail, at most 50). After `[[note#` (or `[[#` for this note): its headings, without the H1 title. After `](`: relative paths to notes. After a blank and `#`: tags with note counts. A `#` as first non-blank character of a line starts a heading: the server returns an empty list, so no popup.
- **Rename.** `textDocument/rename` on a link to a note, or on this note's H1, renames that note: the new name is a file stem (no directory or extension); the result is the link edits plus a `RenameFile` op (as marksman does). On the H1 it renames the file only, not the heading text.
- **Commands.** `mdroots.backlinks <uri>`: `Location[]` of links to the note from other notes (self-links excluded, one per line). `mdroots.anchorLinks <uri> <slug>`: `Location[]` of links from other notes naming the note's heading with that slug by anchor (the `N links` lens). `mdroots.info <uri>`: root, mode, reason and file count, also sent as `window/showMessage`. `mdroots.renameFile <from-uri> <to-uri>`: the rename edit, sent to the client as `workspace/applyEdit`.
- Planned (M8): `workspace/willRenameFiles`, semantic tokens, a "new note" command using `link_style`, and a full-text request (the library has `full_text`; the server does not expose it).

## 4. Scheduling

gopls practices applied in-process ([gopls](../research/gopls.md)). Built: in-order requests (without the worker pool), cancellation of queued requests, debounced diagnostics (one phase, 500 ms after the last edit), single-file service while a root opens on one background opener thread, and progress for opens over 1 s (§3.6). The rest is the target.

| Rule | Behaviour |
|---|---|
| Views | each overlay change and each observed `change_log` advance makes a new view (read txn + overlay map); queries on older views are cancelled via `Cancel` |
| In-order requests | requests from one editor run in arrival order, so a query sees the preceding `didChange`. Workspace symbols, full text, large reference queries and pull diagnostics opt out and run on a small worker pool |
| Two-phase diagnostics | phase 1 at once on the edited document (syntax, in-document anchors, links checkable from overlay and hot key cache); phase 2 cross-file, ~500 ms after the last edit, cancelled by the next edit. `diagnostics.trigger = "save"` skips phase 2 on change |
| Recent-mtime guard | in the cheap mtime scan, a file modified < 2 s ago is "maybe changed" even if mtime and size match |
| Read semaphore | ≤ 64 concurrent file reads per process, smaller budget per lazy/virtual root, so a root on [EdenFS](https://github.com/facebook/sapling) (the virtual filesystem from the [Sapling](https://sapling-scm.com/) project) can't starve a local vault |
| Early open | start opening the root's DB while answering `initialize` (as built, a root starts opening, in the background, on the first `didOpen` of a file in it) |
| No roots for navigation targets | goto/hover into a tree with no root yet is served single-file; discovery runs on `didOpen`. Built: requests never open a root, and an opened document is served single-file until its root is open |
| Progress | `workDoneProgress` (fallback one `showMessage`) for a background open > 1 s. Built |

The daemon question (D3) is reopened only with numbers: total `phys_footprint` with 10 editors plus ramble, CPU spent on duplicate overlay parsing, and p99 save-to-visible latency between peers.

## 5. Cache hygiene

- **Namespacing:** schema-versioned names (`roots/<id>.v<schema>.db`, `roots.v<k>.db`). Built.
- **Errors:** a cache dir or registry that does not open falls back to an in-memory index. A corrupt per-root DB is rebuilt into a new generation file by the reconciler; a peer serves from memory until the rebuild is recorded ([index](index.md) §1.6). Built. Planned: any later cache read error is a miss (parse the file, never fail the request).
- **GC** (`mdroots_index::gc`): at most daily per cache dir, claimed through `meta.gc_at` in the registry; `open_for` runs it on the calling thread when it is due, after a successful open over the real filesystem. It deletes old generations, old-schema files 7 days old, roots unseen for 30 days or whose path is gone (with their registry row), and, over a 1 GiB budget, the least recently seen roots. Every deletion needs `LOCK_EX|LOCK_NB` on the root's `.open`, so a DB some process has open is skipped. One lock scope per root and schema covers all its generations; lock files are never deleted. Details: [index](index.md) §1.7. Built.
- **Last-seen:** every process stamps the root's `last_seen_ms` on open and on refresh, at most hourly per root. Built.

## 6. CLI

The `mdroots` binary (crate `mdroots-cli`) depends on the `mdroots` facade, and on `mdroots-lsp` for `mdroots lsp`; each command is formatting over §3.2 and holds no logic of its own. As the first embedder, it keeps the API honest. Arguments are parsed by hand.

| Command | Does | Exit |
|---|---|---|
| `check [--quiet] [PATH...]` | diagnostics of the notes under each PATH (default `.`): a directory is opened with `open_at` and all its notes checked, a file with `open_for` and only that file. A file covered by several PATHs is reported once, under the first. Files are printed sorted by canonical path, each file's diagnostics by position, as `path:line:col: severity: message` (1-based; columns in characters). A summary `N files, E errors, W warnings, I info, H hints` goes to stderr unless `--quiet` | 1 on any error or warning, else 0 |
| `roots PATH` | the root chosen for PATH: `root:` (absolute), `mode:`, `why:` (discovery's one-line reason), `files:` (indexed count), `cache:` (the root's DB path, or `memory`), `role:` (`reconciler`, `peer`, or `none` in memory), one `nested:` line per nested root | 0 |
| `resolve FROM LINK` | each target, then `step:` and `status:` | 1 without a target |
| `backlinks NOTE` | `path:line: title` per linking note | 0 |
| `search [--] QUERY [PATH]` | notes containing every word of QUERY (`Workspace::full_text`; the last word also as a prefix), one `path:line: snippet` per hit (1-based line), at most 1,000. A file PATH opens its discovered root with `open_for`: as the reconciler it answers from the DB's FTS5 table. A directory PATH (default `.`) is opened with `open_at`, in memory, and scanned naively, even when it is a discovered root. `--` ends options, so a query may start with `-` | 1 without a hit, else 0 |
| `lsp [--stdio] [--log FILE]` | the language server (§3.6) on stdin/stdout until `exit` or EOF; `--stdio` is accepted and ignored (stdio is the only transport; many editor configs pass it); `--log` appends one line per message to FILE: time, direction (`<-` from the client, `->` to it), method or `response`, and id | 0 (2 on a protocol error) |

Paths under the current directory print relative to it, others absolute. Usage errors, unknown arguments, `--help` and a bare `mdroots` print usage to stderr and exit 2, so a server never starts implicitly (it would hang under CI or cron); errors print `mdroots: <message>` and exit 2.

Every command uses the persistent cache (D5, D9): `check` on a file, `roots`, `resolve`, `backlinks`, `search` on a file and `lsp` open discovered roots through it; `check` and `search` on a directory use `open_at`, which stays in memory. Only `lsp` turns on the native watcher; the one-shot commands never start a thread. `MDROOTS_CACHE_DIR`, when set and non-empty, replaces the cache dir for every command, `lsp` included (`Options::cache_dir`); tests point it at a temp dir. Hidden test commands drive the many-process fixtures: `__open PATH --hold-ms N [--refresh-every MS]` prints `role:`, `files:` and `db:` (the DB file, or `memory`), holds the workspace open, and after each refresh prints `files:` and `db:` again; `__gc --now-ms N --force` runs a forced GC on `MDROOTS_CACHE_DIR` (required, so it never touches the user's cache) and prints `deleted:` and `skipped:` lines.

On the corpus: `check tests/corpus/zkvault` reports 5 hints and exits 0 (78% of its explicit links resolve, under the 80% hint threshold); `check tests/corpus/zk-min` reports 1 warning and exits 1. On an 11-note vault, `check` takes 0.32 s with no cache and under 0.01 s with the DB present (release build). On a synthetic 3,000-note notebook, diagnostics for every file take 46–65 ms (26 s before the diagnostics policy was cached, [index](index.md) §3.3). `roots` on a file in a large EdenFS checkout decides lazy on the monorepo marker `.buckconfig` in about 0.6 s cold, without enumerating the tree.

`cargo doc` warns that the binary and the library share the name `mdroots`; harmless, the binary has `doc = false`.

## 7. Work order

| Milestone | Scope |
|---|---|
| M1 (done) | `mdroots-syntax` with `LineIndex`, fuzzing, insta snapshot tests on the shapes of a ~730-note zk vault and a ~210-note research vault; `mdroots-core` + `mdroots-resolve` with the offline differential against zk's `notebook.db` and marksman ([M1 differential](../research/m1-differential.md)) |
| M2 (done) | `mdroots-roots`: stages 1–4 of [roots](roots.md) §1, loose roots, nested-root registry rules, the registry trait with an in-memory `MemRegistry` and `discover.lock`; safety tests on a counting probe with NFS and EdenFS fakes; discovery fixtures 1–11 |
| M3 (done) | the `mdroots` facade (`Workspace` over `MemStore` and discovery, §3.2) and the `mdroots` CLI (`check`, `roots`, `resolve`, `backlinks`, §6); the diagnostics policy of [index](index.md) §3.3 in `mdroots-core` |
| M4 (done) | `mdroots-index`: cache dir choice, flock roles, the per-root DB caching content and stat, reconcile, change log, the SQLite root registry; `list_root` in `mdroots-roots`; the facade serves discovered roots from the DB (D9) with `Workspace::refresh`; many-process fixtures 12, 13, 16, 17, 18 of [roots](roots.md) §7, adapted (no watcher) |
| M5 (done) | the facade's editor queries (`goto`, `outline`, `search_notes`, `preview`, `text`, `rename_note`, `Workspaces`); `mdroots-lsp` (§3.6) and `mdroots lsp`; the Neovim smoke test on the real binary |
| M6 (done) | whole-root queries without the O(n²) cost (cached diagnostics policy and backlink index in `MemStore`, no text copy in `LineIndex`); the opt-in native watcher with `refresh_paths` and `subscribe`, used by `mdroots lsp`; schema v2 with an FTS5 table, `Workspace::full_text` and `mdroots search`; cache GC and corruption rebuild into generation files; fixtures 14 and 15 of [roots](roots.md) §7 |
| M7 (done) | the editor features the Neovim plugin advertised: folding ranges, backlink code lenses (`mdroots.anchorLinks`), the extract-note code action (`Workspace::extract_note`, `WorkspaceEdit.create`); the convention vote run over the `MemStore` and cached, with `link_style` / `link_to` ([index](index.md) §3.2); a background open in `mdroots lsp` over `Workspace::open_single` and `Workspaces::get`, with work-done progress |
| Next | see [ROADMAP](../ROADMAP.md): Linux CI and real-vault validation first, then the deferred work of D9 and the embedder API |

## Neovim 0.12+ example

Files in [`editors/nvim/`](../../editors/nvim/) (drop into your Neovim config dir):

| File | What |
|---|---|
| `lsp/mdroots.lua` | config auto-discovered by `vim.lsp.config`: `cmd = {'mdroots','lsp'}`, `filetypes = {markdown, org}`, a `reuse_client` predicate, `workspace_required = false`, a commented `settings = { mdroots = { diagnostics = … } }` block |
| `plugin/mdroots.lua` | `vim.lsp.enable('mdroots')` plus optional `LspAttach` extras: `gd`, `gO` (LSP symbols), guarded autotrigger completion, codelens, LSP folding, `<leader>ns` search, `<leader>nb` backlinks to loclist, `<leader>nr` rename note, `<leader>nn` new note from visual selection, `:MdrootsInfo`. Codelens, folding and `<leader>nn` are turned on only when the server offers code lenses, folding ranges and the extract-note code action (it does, §3.6) |

Choices (checked against the 0.12.5 runtime):

| Choice | Why |
|---|---|
| No `root_markers` | 0.12 starts a client for a matching filetype even with no root (root_dir nil; `workspace_required` defaults false). Root logic lives only in mdroots. The client then sends `workspaceFolders = null`, so the server never depends on workspace folders |
| `reuse_client` | with root_dir nil the default already reuses the client; the predicate matters only if something sets root_dir, keeping one process. A reused client sends no `didChangeWorkspaceFolders`, which is fine because mdroots finds roots from file paths. One process per Neovim; instances share the root's SQLite cache (D3) |
| `cmd = {'mdroots','lsp'}` | the subcommand is required (§6) |
| `settings`, not `init_options` | Neovim sends `settings` via `workspace/didChangeConfiguration` (the server reads `mdroots.diagnostics` from it, §3.6); `init_options` is sent once, so can't carry changing settings |
| `gO` remap | the markdown ftplugin maps `gO` to a treesitter outline; mapping `vim.lsp.buf.document_symbol` in `LspAttach` runs after the ftplugin |
| Backlinks handler | `Client:exec_cmd` drops the result without a handler; ours feeds `Location[]` to the loclist via `vim.lsp.util.locations_to_items(result, client.offset_encoding)` |
| Rename | `grn` on a link or H1 uses `textDocument/rename`; `<leader>nr` calls `mdroots.renameFile` |
| Completion | enabled only if `supports_method('textDocument/completion')`; nvim-cmp/blink.cmp users set `vim.g.mdroots_autocomplete = false` |
| Position encoding | Neovim offers `utf-8` first; mdroots picks it |
| Filetypes | `markdown`, `org`. Not `mdx` (no default filetype), `quarto` (`.qmd`) or `rmd` (`.Rmd`) for now |

Other built-in 0.12 mappings cover the rest: `K`, `grr`, `grn`, `]d`/`[d`, `<C-]>` via `tagfunc`, `gra` (code actions: extract note on a visual selection).

### Smoke test

```
nvim --clean --headless -u NONE -c 'luafile bench/nvim_smoke.lua'   # from the repo root
```

[`bench/nvim_smoke.lua`](../../bench/nvim_smoke.lua) loads `editors/nvim` against the real `mdroots lsp` server (`$MDROOTS_BIN`, else `$CARGO_TARGET_DIR/debug/mdroots`, else `target/debug/mdroots`; build it first with `cargo build -p mdroots-cli`). It is read-only on the corpus vaults: edits go to scratch notes under `/tmp/mdroots-smoke/`, wiped at the start, and the server's `XDG_CACHE_HOME` points there too. It waits on conditions rather than sleeps; every line is an assertion and failure gives a non-zero exit. It checks one client with root_dir nil and settings sent, utf-8 position encoding, maps and `:MdrootsInfo` (its `window/showMessage` names the root), `gO` overriding the ftplugin, definition, symbols, backlinks via `exec_cmd` into the loclist (self-links excluded), `[[#` heading completion (the H1 title excluded), no completion for `#` at line start, a diagnostic for a broken link, cross-file goto from a vault README (backlinks and this goto are polled until the root's background open finishes), the LSP `foldexpr` on a note, the `6 backlinks` code lens on a vault A note, extract note through `<leader>nn` on a visual selection (the created file is the next free name, the selection becomes the link, nothing is written outside the scratch dir), `mdroots.renameFile` (the `workspace/applyEdit` fixes the linking note and moves the file, the buffer follows) and unmodified vault buffers. It passes on NVIM 0.12.5 (37 checks; attach ≈ 25–500 ms). Not covered: the fold ranges themselves, anchor `N links` lenses, the progress notification.
