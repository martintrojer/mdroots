# Spec: library, API, scheduling and editor integration

Related: [roots](roots.md), [index, links, frontmatter](index.md), [decisions](../DECISIONS.md) (D2, D3, D5, D6), [gopls](https://go.dev/gopls) practices ([notes](../research/gopls.md)), [ramble](../research/ramble.md)

Two requirements:
1. Everything mdroots knows is usable from other Rust programs without speaking LSP.
2. It ships a working [Neovim](https://neovim.io) 0.12+ config ([`editors/nvim/`](../../editors/nvim/), smoke-tested).

## 1. Principle: the LSP server is a thin client of the library

[marksman](https://github.com/artempyanykh/marksman) (F#) and [zk](https://github.com/zk-org/zk) (Go `internal/` packages) keep their core inside the server or CLI, where it can't be reused. mdroots keeps all behaviour in library crates that a TUI, CLI, static-site generator, MCP server or another language server can embed: a flashcard scanner, a semantic-search tool reusing the chunker and link graph, or ramble, a TUI markdown reader by the same author (D2).

**`mdroots-lsp` target: ≤ 3k lines of protocol glue.** It is ~2.1k non-test lines; zk is ≈ 14k, its LSP layer alone 2.2–2.5k for fewer features.

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
    mdroots-cli/      the `mdroots` binary: check, notes, tags, roots, resolve, backlinks, search, lsp (§6)
  editors/nvim/       Neovim 0.12+ example config
  bench/              lspbench.py, mdsurvey.py, mdresolve.py, nvim_smoke.lua
  tools/zkdiff/       the offline differential against zk's notebook.db
  tests/corpus/       fixtures (synthetic, plus scrubbed shapes of two real vaults)
```

Dependency direction (no cycles, no upward edges):

```
syntax ← resolve ← core ← roots ← index ← mdroots ← mdroots-lsp ← mdroots-cli
```

A crate may also use crates further left directly; `mdroots-cli` uses `mdroots` and `mdroots-lsp`. Timing benches are crate examples (`crates/mdroots/examples/bench_root.rs`, `crates/mdroots-syntax/examples/parse_bench.rs`), run with `cargo run --release --example`.

| Crate | Main deps | Direct I/O | wasm32 | Why separate |
|---|---|---|---|---|
| `mdroots-syntax` | [`pulldown-cmark`](https://github.com/pulldown-cmark/pulldown-cmark) (≈ 1.1 GB/s on the testbeds), [`saphyr`](https://crates.io/crates/saphyr) (YAML) and [`toml`](https://crates.io/crates/toml) for frontmatter, `unicode-general-category` | — | ✓ | most reusable piece (linters, formatters, SSGs, browser editors); owns `LineIndex` |
| `mdroots-resolve` | `mdroots-syntax`, `unicode-normalization`, `serde_json` and `toml` (tool configs) | — (via `ResolveEnv`) | ✓ | other tools resolve links the same way over their own file list |
| `mdroots-core` | `mdroots-resolve` | only through `FileSystem` | ✓ (`MemStore`, embedder's `FileSystem`) | the FS seam and the in-memory index every front end queries |
| `mdroots-roots` | `mdroots-core`, `ignore` (gitignore matcher only), [`rustix`](https://github.com/bytecodealliance/rustix) (`statfs`; unix only) | ✓, all through its `Probe` trait | — | safe root finding is useful alone, e.g. to a search tool |
| `mdroots-index` | `mdroots-core`, `mdroots-roots`, [`rusqlite`](https://github.com/rusqlite/rusqlite) (bundled [SQLite](https://sqlite.org), with [FTS5](https://sqlite.org/fts5.html)), `rustix` (`getuid`); flock via `std::fs::File::try_lock` (no `fs4`) | ✓ | — | heavy deps behind one crate: cache dir, flock roles, per-root DB with its full-text table, reconcile, registry, GC |
| `mdroots` | all of the above (no features), [notify](https://crates.io/crates/notify) 8 (the watcher), `unicode-normalization` | — | — | stable public surface |
| `mdroots-lsp` | `mdroots`, [`lsp-server`](https://crates.io/crates/lsp-server), [`lsp-types`](https://crates.io/crates/lsp-types), `serde_json`, `crossbeam-channel` | — | — | protocol only; the only crate naming `lsp-types` |
| `mdroots-cli` | `mdroots`, `mdroots-lsp`, `lsp-server` | — (through the facade) | — | the command line; formatting only |

MSRV is Rust 1.89 (`File::try_lock`/`lock_shared`). Lock files (`<id>.lock`, `<id>.open`, `discover.lock`): [roots §5](roots.md#files).

**Publishing:** all crates share one version, set in the workspace `Cargo.toml` (`[workspace.package]`). `mdroots` (library) and `mdroots-cli` (binary) are public; the other `mdroots-*` crates are internal, and every dependent pins them to the exact version (`=x.y.z`), so they move in lockstep (D6, [CHANGELOG](../../CHANGELOG.md)).

### Feature flags on `mdroots`

The facade has no features: it always builds `mdroots-roots`, `mdroots-index` (with FTS5) and `notify`. The discovery walk is sequential: it times each `readdir` for the rate check and lists only through the `Probe` ([roots §1 stage 4](roots.md#stage-4-budgeted-walk)). Feature flags are deferred ([ROADMAP §3](../ROADMAP.md#3-deferred-work)). Target design:

- Default features `roots` (`ignore`, `rustix`), `index` (bundled SQLite), `fts` (FTS5, needs `index`), `watch` (`notify`), `parallel` (a `rayon` pool for the cold parse) and `org` ([org-mode](https://orgmode.org) parsing in `mdroots-syntax`); optional `serde` (derives on public types). Without `roots`, use `Workspace::open_at(root, opts)` instead of `open_for(path)`.
- `default-features = false` = `syntax + resolve + core + MemStore`: pure Rust, no `statfs` bindings, threads or discovery. With `org` and `serde` it builds for wasm32; the embedder supplies `FileSystem`. A planned `Options::background(true)` thread needs `std::thread`, so not on `wasm32-unknown-unknown`.
- No `lsp-types` feature: `From` impls would tie `mdroots`'s major version to `lsp-types`'s. They live in `mdroots-lsp`; other servers convert from byte ranges and `LineIndex`.

## 3. Public API (sketch)

### 3.1 One document (`mdroots-syntax`)

```rust
use mdroots::syntax::{parse, Dialect, Element, Context};

let doc = parse(text, Dialect::detect_from_path(path));   // Markdown | Org
for el in doc.elements() {
    match el {
        Element::Heading(h) => println!("{} {}", h.level, h.text),
        Element::Link(l) if l.context == Context::Prose => println!("{:?} -> {}", l.kind, l.target().raw()),
        Element::Link(l) => { /* in code/comment: mention, not a link */ }
        Element::Tag(t) => println!("#{}", t.name),
        _ => {}                                        // #[non_exhaustive]
    }
}
let fm = doc.frontmatter();            // Option<&Frontmatter>
fm.and_then(|f| f.title());            // standard keys normalised (title/aliases/id/tags/dates/…)
fm.map(|f| f.get("stage"));            // raw access for any key
```

- **Links are read through accessors.** `Link` has public `kind`, `context`, `confidence`, `range`, `text_range`; `target()`, `label()` and `group()` are methods. `LinkTarget` exposes `raw()`, `path()` (a part of `raw`), `anchor()` and `line()`; `LinkTarget::new(raw, path_range, anchor)` and `Link::with_target` / `with_label` / `with_anchor` / `with_line` / `with_group` build them. Label, anchor, line and group share one box, absent on most links (a `LinkTarget` is 32 bytes, a `Link` 72).
- **Heading ids are read through accessors.** `Heading` has public `level`, `text` and `slug` (both `Box<str>`) and `range`; `id()` (markdown `{#id}` or org `:ID:`) and `custom_id()` (org `:CUSTOM_ID:`) return `Option<&str>` and share one box, absent on most headings (a `Heading` is 64 bytes, an `Element` 72).
- **Positions have one owner.** Ranges are byte offsets. `LineIndex` (lazy per document) converts to UTF-8/16/32 line/column. `mdroots-lsp` has no position code and no `line-index` dependency.
- The server negotiates `positionEncoding: "utf-8"` when offered (Neovim 0.12 lists it first); UTF-16 is the required fallback.
- `parse` is total: never fails or panics (fuzzed). Bad frontmatter still yields a `Frontmatter` (its `range` and whatever entries parsed) with `error: Some(message)`.
- Zero-copy where possible (`&str` slices; `into_owned()` for owned variants).

### 3.2 A workspace (`mdroots`)

The facade over `mdroots-roots`, `mdroots-index` and `MemStore`. Public paths are absolute and canonical (through the workspace's `FileSystem`); a path outside the root is `Unsupported`. Everything is synchronous; the only background thread is the opt-in watcher (`Options::watch`). An embedder that must not block on discovery serves the file with `open_single` and runs `open_for` on its own thread, as `mdroots lsp` does (§3.6).

```rust
use mdroots::{Workspace, Workspaces, Options, IndexMode, Freshness, Cancel};
use mdroots::syntax::PositionEncoding;
use mdroots::query::{NoteQuery, TagExpr, SortKey, parse_date, parse_sort, day_range};

let ws = Workspace::open_for(path, Options::default())?;      // discovery first (roots spec), then index
let ws = Workspace::open_at(&root_dir, Options::default())?;  // a directory as the root; no discovery, in memory
let ws = Workspace::open_single(&file, Options::default())?;  // the file alone; no discovery, cache or registry
let ws = Workspace::open_dir(&dir, Options::default())?;      // the notebook containing a directory (discovery from its first note)
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
let tags   = ws.tags();                                        // Vec<(String, usize)>, case-folded, sorted by lowercase name
let tags   = ws.tags_under(&[dir]);                            // the same over the notes under some paths
let mut q = NoteQuery::default();                              // #[non_exhaustive]: build from default, then set fields
q.tag = vec![TagExpr::parse("a, NOT b")?];
let found  = ws.query(&q)?;                                    // Vec<NoteSummary>, zk list filters
let lone   = ws.orphans()?;                                    // Vec<NoteSummary>: no other note links to them
let pairs  = ws.missing_backlinks()?;                          // Vec<(from, to)>: from links to, to does not link back
let near   = ws.related(&note_path)?;                          // Vec<(NoteSummary, intermediaries)>: two directed hops, not linked
let out    = ws.links_from(&note_path)?;                       // Vec<NoteSummary>: the notes it links to
let inc    = ws.links_to(&note_path)?;                         // Vec<NoteSummary>: the notes linking to it
let sets   = ws.settings();                                    // Vec<Setting { name, value, source }>: effective root settings
let target = ws.resolve(&from_path, "[[some-note]]")?;         // Resolution { targets, step, status, hint }
let goto   = ws.goto(&note_path, offset)?;                     // Option<Goto { targets, heading, line }>
let links  = ws.document_links(&note_path)?;                   // Vec<DocLink>, source order
let back   = ws.backlinks(&note_path)?;                        // Vec<Backlink>, sorted by source path
let heads  = ws.outline(&note_path)?;                          // Vec<syntax::Heading>, source order
let same   = ws.same_target_links(&note_path, offset)?;        // Vec<Range<usize>>: links going where the link at offset goes
let titles = ws.link_titles(&note_path, range)?;               // Vec<(offset of the link's end, title)>: inlay hints
let tag    = ws.tag_at(&note_path, offset)?;                   // Option<(name as written, Range<usize>)>
let prev   = ws.preview(&note_path, 10)?;                      // Preview { title, frontmatter, excerpt }
let text   = ws.text(&note_path)?;                             // current text, overlay wins
let diags  = ws.diagnostics(&note_path, &cancel)?;             // Vec<Diagnostic>, same policy for every front end
let edit   = ws.rename_note(&old, &new, &cancel)?;             // WorkspaceEdit { edits, rename, create }; writes nothing
let edit   = ws.rename_notes(&[(old, new), ...], &cancel)?;    // the link edits for several moves (files, folders) planned together; rename: None
let edit   = ws.extract_note(&note_path, range, &cancel)?;     // WorkspaceEdit: create a note from the bytes `range`, link it
let style  = ws.link_style();                                  // LinkStyle: how new links are written in this root
let link   = ws.link_to(&from, &target, None)?;                // the link text from `from` to `target` (may not exist yet)
let link   = ws.link_text(&from, &target, style, Some("h"), None)?; // the same in an explicit LinkStyle, with an anchor
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

- **`Options`**: a builder with `Default`: `workspace_folders` (bound the marker climb), `code_dirs` (extra absolute dirs that code mentions resolve against, §3.3), `enumerator` (lists the markdown of a virtual checkout; default `SlFiles`, `NoEnumerator` turns vcs-enumerated mode off), `cancel` (checked during discovery and indexing), `fs` + `probe` (set both or neither; default `StdFs` and `StdProbe`, unix only, elsewhere the embedder supplies both), `index(IndexMode)`, `cache_dir(path)` and `watch(bool)` (default off).
- **`IndexMode`** (`#[non_exhaustive]`): `Auto` (default) keeps a per-root DB in `cache_dir` if set, else the user's cache dir (D5), else memory. `Auto` with an explicit `fs`/`probe` (in-memory test trees) and no `cache_dir` stays in memory. `Memory` never touches the cache dir. A cache that fails to open falls back to memory.
- **What `open_for` indexes.** The file must exist and is always read, even if dataless. Every mode but lazy and single-file: the notes discovery listed (or `list_root` re-lists on a registry hit), plus the opened file (`Fresh`). Lazy roots: a working set of the opened file plus the notes of its directory, one level, no hidden, editor-temp or dataless entries, at most 2,000 files by name, the opened file counted (`Lazy`). Single-file decisions, and a file outside the decided root, index the file alone with its directory as root (`Lazy`).
- **With a cache**, `open_for` holds `discover.lock` around discovery with the persistent registry; a registered root then opens its DB and takes its locks. The reconciler reconciles and writes; a peer reads the rows and re-reads changed files in memory ([index §1.3](index.md#13-finding-what-changed)). Either way the process hydrates its `MemStore` from the returned bytes, so unchanged notes are not opened. Single-file workspaces, `open_at` and roots without a registry row stay in memory.
- **`refresh(&cancel)`** re-syncs the root (or working set) with the disk ([index §1.3](index.md#13-finding-what-changed)): a peer first tries to become the reconciler; only the reconciler writes; memory mode re-lists and re-reads. The new index replaces the old; overlays survive. It also stamps the root as seen (at most hourly) and, if the registry names a different DB file than the one held (another process rebuilt a corrupt DB), reopens that file first ([index §1.6](index.md#16-failures-and-races)).
- **`refresh_paths(&paths, &cancel)`**: the same for just `paths` (absolute; outside the root ignored), as a point refresh (a peer first tries to become the reconciler); the store is patched in place, overlays survive. A single-file workspace re-checks only its file, a lazy one its working-set directory and indexed notes. Returns the absolute paths whose content changed, sorted. If the DB file was replaced, a full `refresh` runs instead and every indexed note is returned.
- **`Options::watch(true)`** starts a native watcher only for the reconciler of a DB-backed marker, VCS or loose root on a local filesystem ([index §1.3](index.md#13-finding-what-changed)); `watching()` says whether one runs. It point-refreshes changed paths and stops when the last clone of the workspace drops. A peer promoted on `refresh` starts watching then.
- **`subscribe()`**: a channel of the absolute paths each watcher-driven refresh changed (a lost-events rescan sends every indexed note). Explicit `refresh` and `refresh_paths` send nothing; their caller already knows. Closed receivers are dropped.
- **`open_single(path, opts)`** indexes one existing file alone, with no discovery, cache dir, registry or discovery lock: root = its directory, mode `SingleFile`, reason `single-file: opened without discovery`, freshness `Lazy`, never watching. It costs only the file's parse, so it can serve a file at once while `open_for` runs elsewhere. Not cached in any `Workspaces`.
- **`open_dir(dir, opts)`** opens the notebook containing `dir` (zk treats a path argument as a filter inside the notebook): discovery as in `open_for` from the first note under `dir` (breadth first, by name, hidden entries skipped). With no note there, or when the root found does not contain `dir` (a nested root, a single-file decision), `dir` itself is opened with `open_at`. The root always contains `dir`.
- **`open_at`** walks the directory with the plain markdown walk (no budget, no filesystem classification), always in memory, and reports `RootMode::Marker` with reason `opened at <dir>`. It is for a directory the caller knows is a bounded root.
- **`query(&NoteQuery)`** (`mdroots::query`): notes matching every filter set, as `zk list` does: `paths` and `exclude` (prefixes), `tag` (each a `TagExpr`, zk syntax: `,` / ` AND ` and, ` OR ` / `|` or (tighter), `NOT ` / `-` not, `( )` groups, `*` `?` globs, case-insensitive, `#` dropped; a syntax error names its column), `tagless`, `matching` (as `full_text`), created and modified bounds (after inclusive, before exclusive; `parse_date` and `day_range` read `YYYY-MM-DD`, RFC 3339 and zk's relative forms, UTC), `orphan`, `missing_backlink`, `link_to`, `linked_by`, `related`, `sort` (`parse_sort`: `KEY[+|-]` over title, path, created, modified; default title A–Z) and `limit`. Graph filters on a `Lazy` root are an `Unsupported` error, not a partial answer.
- **Graph queries** (`orphans`, `missing_backlinks`, `related`, `links_from`, `links_to`): an edge `a → b` is a link that `backlinks(b)` reports from `a`: prose, heading, HTML or frontmatter, not code or a footnote, resolved or ambiguous (an ambiguous link is an edge to every candidate). Self-links never count (zk counts them). A bare path in prose (`see b.md`, `Confidence::Implicit`) is an edge when the note exists; zk does not count it, so mdroots finds more links by design. A bare path to a missing file is plain text, not a broken link. `related(a)`: notes `m` on a directed two-hop path `a → k → m` or `m → k → a`, not `a` and not linked with `a` either way (zk 0.15.6's `--related`: two hops at most; `a → k ← m` and `a ← k → m` do not count), scored by the number of distinct intermediaries `k`. All are `Unsupported` on a `Lazy` root.
- **`settings()`**: the root's link style, tag syntaxes, broken-link severity and docs dir, each with its `Source`: a tool config key, a marker's default, the vote or mdroots' default (`mdroots_resolve::dialect::explain`). `mdroots roots` prints them.
- **`resolve`** parses `link_text` with the dialect of `from`, takes its first link and resolves it with goto semantics (the Partial step allowed; `hint` is true for a Partial hit). Text without a link is an `Unsupported` error. `targets` may lie outside the root.
- **`goto(path, offset)`**: the link at a byte offset of the note's current text, in any context (code included), resolved like `resolve`. `None` without a link there or without a target (broken, external). `heading` is the byte range of the heading an anchor names (slug, org `:ID:` or `:CUSTOM_ID:`), in `targets[0]`; `line` is the 1-based line of a code mention `path:LINE`.
- **`same_target_links(path, offset)`**: on a counting link (prose, heading, HTML or frontmatter; not code or footnotes) with a resolved best target, the ranges of every counting link of the note with that same target and the same anchor as written (both absent or equal), itself included, in source order. A link without a resolved target (broken, external) groups with nothing: `[]`. Not on a link but inside a heading: the counting links of this note whose target is this note and whose anchor names that heading, matched as `goto` matches (slug, GitHub slug, id or custom id). Otherwise `[]`. **`tag_at(path, offset)`**: the tag whose range contains `offset`, frontmatter tags included, with its name as written; read from the stored document, so it follows the root's tag syntaxes as `notes_with_tag` does. `None` off a tag.
- **`link_titles(path, range)`**: for each counting wiki link overlapping the byte `range` whose written path has no `/` and no label, resolved to one note (not ambiguous, not broken) whose title differs case-insensitively from the written stem, the byte offset of the link's end and the title, in source order. The stem is the written path without a note extension (`md`, `markdown`, `org`, in any case); `[[#heading]]` has no stem and gets none.
- **`search_notes(query, limit)`**: notes whose title, file stem or a frontmatter alias contains the query as a case-insensitive subsequence, best first (exact, prefix, substring, then fewer gaps; ties by path). An empty query lists notes by path.
- **`full_text(query, limit, &cancel)`**: notes whose text contains every term of `query`, at most `limit`, as `Hit`s (§3.3). `query` is plain text split on whitespace; terms without a letter or digit are dropped. Matching is case-insensitive, diacritics folded (`cafe` finds `café`), on runs of letters and digits; a term of several tokens (`a-b`) matches as a phrase; the last term also matches as a prefix. A query without terms finds nothing. Two paths, same rules: the reconciler takes the DB's FTS5 table's ranked hits ([index §1.2](index.md#12-schema)) for notes without an overlay; a naive scan of the current text covers notes with an overlay, and every note for a peer (whose DB may lag the text it serves), in memory mode and after `open_at`, checking `&Cancel` between notes. Order: DB hits by rank, then naive hits by path.
- **`preview`** returns the title, frontmatter entries in document order (lists joined with `", "`) and the first `max_lines` lines after the frontmatter. For a note that is not indexed, **`preview`, `text`, `line_col`** are `Unsupported` and **`outline`, `document_links`, `backlinks`, `diagnostics`** return empty. `Backlink.line` is 0-based; `from_title` is the frontmatter title, else the first level-1 heading, else the file stem.
- **`rename_note(old, new, &cancel)`**: `old` must be an indexed note and `new` a note path inside the root that neither exists nor is indexed. Links whose best target is `old` are rewritten in their own style (file-relative, root-relative, site-rooted or by stem); links found by id, title, alias, a dialect transform or a partial match are left alone, as are links in code. When `old` moves to another directory its own file-relative links are rewritten too. A bare stem that another note carries after the move (by the resolver's stem key) is not written: the link gets the root-relative path instead. Every rewritten link is checked by running the resolver against the layout after the move: a store of every note's current text (overlay wins) under its new path, over a view of the filesystem with the moves applied, so site paths, existing files and directories, and key normalisation (percent-decoding, NFC, case folding) count as they will. The candidate is put in place of the link's path and the link's text re-parsed in the note's dialect: it must still be one link with exactly that path (a `#`, `|`, `]]` or `::` the syntax would read otherwise rejects it), and must reach its moved target first and not be ambiguous. A target that is not a note must exist after the moves. When the preferred text would reach another note, the `/`-prefixed root-relative path is tried, then the file-relative path, and a link that none of them lands is left unedited. Building that store re-parses every note once per call. Anchors are kept. `WorkspaceEdit.edits` is per file (absolute, sorted) with non-overlapping byte-range `TextEdit`s, then `rename = Some((old, new))`; `create` is empty. Nothing is written.
- **`rename_notes(&[(old, new), ...], &cancel)`**: the link edits for every move in the list, planned together; the caller moves the files (D10). Every moved note's path after the moves is known when any link is rewritten, so links between notes that move together stay right (A and B linking each other, both moved into `new/`: no edit). A pair whose old path is an indexed note moves it; a pair whose old path is a folder moves every indexed note under it to the same sub-path under `new`, and the whole folder moves with it: links to any path under it (images, other files, subfolders) follow it to the same sub-path under `new`, and the landing check sees that layout. When folder pairs nest, the deepest folder decides. A pair whose old path is neither a note nor a folder holding one, or whose new path is outside the root (or, for a note, not a note extension), is skipped without error; a note moved by two pairs is `ErrorKind::Unsupported`. Rewrites follow `rename_note`; the stem test and the landing check run over the layout after all the moves. One call takes the store lock once. `rename` is `None` and `create` empty. `rename_note(old, new)` is `rename_notes(&[(old, new)])` plus `rename = Some((old, new))`, after its own checks.
- **`link_style()`**: the root's `LinkStyle` (§3.3) for inserted links: a zk or [Obsidian](https://obsidian.md) config, else the convention vote, else the marker's default (precedence: [index §3.2](index.md#32-vote-and-link-style)). The vote is cached with the other whole-root caches.
- **`link_to(from, target, label)`**: the link text to insert in `from` pointing at `target` (inside the root; need not exist) in that style. Without a label a wiki link is `[[target]]` and a Markdown link uses the target's title (the indexed note's, else its stem). `[[stem]]` becomes `[[dir/stem]]` when another note shares the stem (case-insensitively). Markdown paths percent-encode `%`, space, ASCII control characters, `(`, `)`, `#`, `<`, `>` and `&` (`markdown_destination`; rename and path completion encode the same way); `md_suffix: false` drops the extension. A link that would resolve from `from` to another note (a same-named note in `from`'s directory, or a shared stem) is written another way: a root-relative path gets a leading `/` (else is written file-relative); a wiki path is written file-relative (else with a leading `/`). `wiki_target(from, target)` returns the text inside `[[…]]` that the stem style writes (LSP note completion inserts it). `Unsupported`: a wiki target containing `|`, `]` or a line break, a wiki label containing `]]` or a line break, a path outside the root.
- **`link_text(from, target, style, anchor, label)`**: `link_to` in an explicit `LinkStyle`, with an optional anchor (as written in the source, without `#`: `my-heading`, `^block`). `link_to(from, target, label)` is `link_text` with the root's style and no anchor. Wiki styles write `[[target#anchor]]` or `[[target#anchor|label]]`; Markdown styles write `[label](dest#anchor)`, the anchor appended after the path is encoded (`markdown_destination` encodes `#`). The anchor is inserted verbatim. The landing check that picks the path form is made on the link without the anchor. `Unsupported`, besides the `link_to` cases: a wiki anchor containing `[`, `]`, `|` or a line break; a Markdown anchor containing whitespace, `(`, `)`, `\`, `<`, `>` or a line break.
- **`extract_note(from, range, &cancel)`**: moves the bytes `range` of the indexed Markdown note `from` (current text) into a new note and replaces them with a `link_to` it. Title: the heading's text when the selection starts at the start of a heading's line, else the first non-blank line (trimmed, at most 60 characters), else `Untitled`. File: `<GitHub slug of the title>.<from's extension>` (`untitled` for an empty slug) in `from`'s directory, with `-2`, `-3`, … appended while the name exists on disk or in the index, checked on every call. Content: the selection after a `# <title>` line (omitted when the selection starts with a heading), ending with a line break. Result: `create = [(new, content)]`, one edit in `from`, `rename = None`. Org notes, an empty or invalid range and a note that is not indexed are `Unsupported`. Nothing is written.
- **`frontmatter_range`**: the frontmatter block's byte range, `None` without one. **`anchor_backlinks`** counts, per heading (its index in `outline`), links from other notes whose anchor names it, matched as `goto` matches (slug, GitHub slug, heading id or custom id; as written and percent-decoded; the first matching heading); headings with none are left out. **`heading_backlinks`** lists those links for one heading, sorted by source path.
- **Diagnostics** are computed per call under the root's `DiagnosticPolicy` ([index §3.3](index.md#33-diagnostics)), with `lazy` set when freshness is `Lazy`. The policy is computed once per store (`MemStore::policy`) and dropped on the next content change, as is the backlink index.
- **`Workspaces`** maps files to workspaces, opening each root once and sharing one cache dir and registry. A file is served by the cached workspace with the longest root containing it, unless it is under one of that root's nested roots; a single-file or rootless lazy workspace serves only its own file. `get(file)` is the same lookup without opening anything (`None` if nothing cached serves the file); `options()` returns the shared options, e.g. for `open_single`. `Clone + Send + Sync`.
- Re-exports: `Hit`, `LinkStyle`, `Setting`, `Source`, `DialectMarker`, `FsStat` and `MountInfo` (for a custom `Probe`), `Cancel`, `Error`, `ErrorKind`, `FileSystem`, `StdFs`, `Diagnostic`, `DiagCode`, `Severity`, `RootMode`, `Probe`, `StdProbe`, `Enumerator`, `NoEnumerator`, `ResolveStep`, `LinkStatus`, `Role`, `mdroots_index` as `mdroots::index`, `mdroots_syntax` as `mdroots::syntax`; `mdroots::names` gives the lowercase-hyphenated names of modes, severities, steps and statuses that the CLI and the server print.

Not built ([ROADMAP](../ROADMAP.md)): `Freshness::Stale { pending }`, background reconcile, feature flags, working-set growth as files are opened. No separate library-level non-blocking open is planned: `open_single` plus `open_for` on the embedder's thread covers it.

### 3.3 Types

The types of §3.2, shaped by an embedder's needs ([ramble](../research/ramble.md)): synchronous, byte offsets and paths, `&Cancel` on slow calls. A read-only embedder calls `refresh_paths` for "this file changed on disk". Not built ([ROADMAP](../ROADMAP.md)): `Preview.summary` and typed `subscribe` events (it sends changed paths only).

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
    pub modified: Option<SystemTime>,    // file mtime; None for an overlay-only note or a failed stat
    pub created: Option<SystemTime> }    // frontmatter `date` (else `created`), else the file's birth time
#[non_exhaustive] pub struct Preview { pub title: String,
    pub frontmatter: Vec<(String, String)>, pub excerpt: String }
#[non_exhaustive] pub struct Goto { pub targets: Vec<PathBuf>, pub heading: Option<Range<usize>>, pub line: Option<u32> }
#[non_exhaustive] pub struct WorkspaceEdit {
    pub edits: Vec<(PathBuf, Vec<TextEdit>)>,   // per file, absolute, sorted; ranges sorted, not overlapping
    pub rename: Option<(PathBuf, PathBuf)>,     // the file rename after the edits
    pub create: Vec<(PathBuf, String)>,         // files to create (absolute, not existing) with content, before the edits
}
pub struct TextEdit { pub range: Range<usize>, pub new_text: String }
#[non_exhaustive] #[derive(Default)] pub struct NoteQuery {   // mdroots::query; every filter set must hold
    pub paths: Vec<PathBuf>, pub exclude: Vec<PathBuf>,       // prefixes: keep / drop the notes under them
    pub tag: Vec<TagExpr>, pub tagless: bool,
    pub matching: Option<String>,                             // as full_text
    pub created_after: Option<SystemTime>, pub created_before: Option<SystemTime>,
    pub modified_after: Option<SystemTime>, pub modified_before: Option<SystemTime>,
    pub orphan: bool, pub missing_backlink: bool,
    pub link_to: Vec<PathBuf>, pub linked_by: Vec<PathBuf>, pub related: Vec<PathBuf>,
    pub sort: Option<(SortKey, bool)>,                        // None: title A–Z; bool: ascending
    pub limit: Option<usize>,
}
pub struct TagExpr { /* private */ }     // TagExpr::parse(s) -> Result<TagExpr, String>; .matches(&tags)
#[non_exhaustive] pub enum SortKey { Title, Path, Created, Modified }   // default_ascending(): title and path
pub fn parse_sort(s: &str) -> Result<(SortKey, bool), String>;                       // `KEY[+|-]`, shortcuts t/p/c/m
pub fn parse_date(s: &str, now: SystemTime) -> Result<SystemTime, String>;           // a date bound, UTC
pub fn day_range(s: &str, now: SystemTime) -> Result<(SystemTime, SystemTime), String>; // `--created DAY`: [start, end)
#[non_exhaustive] pub enum LinkStyle {   // mdroots_resolve::dialect, re-exported
    WikiStem,                            // [[stem]]
    WikiPath,                            // [[dir/stem]], root-relative
    MarkdownRelative { md_suffix: bool },     // [label](../dir/note.md), relative to the linking file
    MarkdownRootRelative { md_suffix: bool }, // [label](dir/note.md), relative to the root
}
```

- `LinkStatus::Ambiguous` carries no candidates; `Resolution.targets`, `Goto.targets` and the diagnostic's `related` list them, best first.
- Code mentions (`` `src/main.rs:12` ``): `:LINE[:COL]` is stripped into `DocLink.line`. A relative mention tries the linking note's dir, then each of `Options::code_dirs` in order (absolute, canonicalised on open; others ignored, duplicates dropped), then the root; a hit outside the root is `Unindexed`.

### 3.4 Design rules

- **Synchronous core, no async runtime.** Queries are µs–ms lookups in the in-memory index (D9); async callers use `spawn_blocking`. Neither tokio nor rayon is forced on embedders.
- **Cancellation.** `refresh`, `refresh_paths`, `full_text`, `diagnostics`, `rename_note`, `rename_notes`, `extract_note` (and `Options::cancel` for `open_for`) take a `Cancel`: a clonable `Arc<AtomicBool>` plus optional deadline, checked between files and batches. A cancelled call returns `ErrorKind::Cancelled` with no partial state (the DB keeps only whole committed batches). Server use: §3.6.
- **Consistent reads.** Each query reads the workspace's `MemStore` under one read lock, overlays included. `refresh` builds a new `MemStore` outside the store lock and swaps it in; `refresh_paths` patches it under the write lock. A query sees the old or the new index, never a mix. Both hold the workspace's index lock until the store is updated, so they never interleave (lock order: index, overlays, store).
- **Roles ([D3](../DECISIONS.md#d3-in-process-no-daemon-one-writer-per-root)).** The holder of `<id>.lock` is the reconciler: the only writer and the only one that may watch. `Workspace::role()` tells which.
- **Background work is opt-in.** Without `Options::watch(true)` nothing runs between calls; disk changes arrive through `refresh` and `refresh_paths`. The watcher thread (`mdroots-watch`) is described in [index §1.3](index.md#13-finding-what-changed). The server's background open (§3.6) is its own thread, not the library's. Not built ([ROADMAP](../ROADMAP.md)): `Options::background` with one low-priority thread for sweeps, and per-query `PRAGMA data_version` / `meta.generation` checks.
- **Cold start.** The first `open_for` indexes synchronously; later opens read only changed files ([index §1.5](index.md#15-short-lived-instances)).
- **Diagnostics are point-fresh.** `diagnostics(path)` checks the document and its link targets in this process; whole-index freshness isn't required. A link target is `stat`ed before it is reported broken: an existing gitignored or unindexed file is not broken. In lazy roots only `stat`-checkable links are diagnosed.
- **Never writes user files.** Mutations are returned as `WorkspaceEdit`. Only the cache dir (D5) is written; `Options::index(IndexMode::Memory)` turns that off too. The working set never grows after open.
- **`Send + Sync`, cheap to clone** (`Arc` inside); many threads query while one refreshes. `rusqlite::Connection` is `!Sync`, so a workspace keeps its one DB connection and its locks behind a mutex, used by open, `refresh`, `refresh_paths` and a reconciler's `full_text` (every write `BEGIN IMMEDIATE`). Memory target and measurements: D3 and [ROADMAP](../ROADMAP.md).
- **Multiple roots.** `Workspace` is one root; `Workspaces` maps paths to roots, handles nesting and caches handles. The LSP uses `Workspaces`.
- **Errors.** One `mdroots::Error` (kind + message) with `#[non_exhaustive] ErrorKind` (`Io`, `Cancelled`, `Unsupported`, `Corrupt`, …). Missing files and broken links are data, not errors. SQLite `BUSY` waits up to `busy_timeout` (2 s). Inside the index layer a DB of another schema or a corrupt one is `Corrupt`; the facade rebuilds a corrupt DB into a new generation (reconciler) or serves from memory (peer), so `open_for` does not fail on it ([index §1.6](index.md#16-failures-and-races)).
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
    fn created(&self, p: &Path) -> Option<SystemTime> { None } // birth time; StdFs asks the OS, default None
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
- `FileSystem::created` is a default method, not a `Meta` field, so existing implementations keep compiling. A clone or checkout resets birth times, so a frontmatter date wins for `NoteSummary.created`.
- `FileSystem` returns bytes, not `str`, so invalid UTF-8 is indexed lossily instead of failing the batch.
- `FileSystem` and `Probe` make the roots safety rules testable: "no readdir on a large monorepo checkout" runs against a counting fake.
- Planned: a `Store` trait once the derived tables (D9) give a second store; a `LinkResolver` trait to add house conventions (e.g. `[[wiki:Page]]`) to the ladder without forking; an `Observer` for progress and file events.

### 3.6 Embedding the server

```rust
let (conn, io) = lsp_server::Connection::stdio();      // or Connection::memory() in tests
mdroots_lsp::serve(conn)?;                             // default Options
// mdroots_lsp::serve_with(conn, Options::default().cache_dir(dir))?;  // explicit options
io.join()?;
```

`serve` runs until `exit` (an `exit` without `shutdown` is an `Err`) or disconnect. `mdroots lsp` (§6) is `serve_with` on stdio with the CLI's options. Workspace folders from `initialize` are added to the options (they bound the marker climb). Planned: a builder to share an embedder's `Workspaces` and add commands.

Server behaviour (LSP layer, not library):
- **One thread, in order.** Requests are answered one at a time, in arrival order, synchronously on the message-loop thread, so each sees the edits before it. Root discovery and indexing never run on it (background open, below). Before each request the server reads every message already sent: a `$/cancelRequest` for a queued request makes it answer `RequestCanceled`, and a `didChange` cancels the queued requests on that document (they would answer on stale text). A running request is not interrupted.
- **Handshake.** `positionEncoding` is `utf-8` when the client offers it, else UTF-16. `lsp-server`'s `initialize_finish` consumes the client's `initialized`, so the server registers its watcher right after `initialize`: `client/registerCapability` for `workspace/didChangeWatchedFiles` on `**/*.{md,markdown,org}`, only when the client offers dynamic registration for it.
- **Document sync.** Full sync. `didOpen`/`didChange` set the document's text as its workspace's overlay; `didClose` clears the overlay and the diagnostics. A buffer with no file on disk gets no workspace and no diagnostics.
- **Background open.** On `didOpen` of a file no open workspace serves (`Workspaces::get`), the server gives the document a single-file workspace at once (`Workspace::open_single`, server's options), publishes its diagnostics, and queues `Workspaces::for_path` for the file on one opener thread (`mdroots-lsp-open`; serial, one open at a time, a file queued once until done). When it finishes, single-file documents under the now-open root move onto it (text re-set as overlay), that root's open documents get re-published diagnostics, and code lenses refresh. A failed open leaves the document single-file. Requests for a note that is not open use the workspace serving it, else `open_single`; never a root open. On a cold synthetic 3,000-note root: first diagnostics (single-file) after 9 ms, the root's after 2.8 s. A single huge note still costs its own parse on the loop thread.
- **Progress.** An open still running after 1 s is announced. With `window.workDoneProgress`: `window/workDoneProgress/create`, a `$/progress` begin titled `mdroots: indexing <dir>` (the opened file's directory; the root is unknown until discovery finishes), and an end when the open finishes. Otherwise one `window/showMessage` per session. Nothing after `shutdown`.
- **Refresh and the native watcher.** `mdroots lsp` passes `Options::watch(true)`; `serve_with` uses the options as given. `didSave` refreshes the saved document's workspace (a single-file one too, re-publishing its document); `didChangeWatchedFiles` refreshes every non-watching workspace containing a changed path, so the disk has one owner ([index §1.3](index.md#13-finding-what-changed)). Then the open documents in those workspaces get re-published diagnostics and refreshed code lenses.
- **Watcher changes.** On `didOpen`/`didChange` the server subscribes once to each watching workspace; a forwarding thread per root turns its notifications into the root path on a channel the message loop selects next to the client connection. Each notification re-publishes the open documents of that root, so a link target created by another program clears its broken-link diagnostic without a save (latency in [index §1.3](index.md#13-finding-what-changed)). A peer follows the disk on save and on the client's watched-file events.
- **Diagnostics** are published at once on `didOpen` and after a refresh, and 500 ms after a document's last `didChange`; nothing after `shutdown`. The `mdroots.diagnostics` setting (`auto`, `off`, `hint`, `warn`, `error`; from `workspace/didChangeConfiguration`) turns them off or sets the severity of broken links and anchors.
- **Requests.** `definition` (`Workspace::goto`; an anchor jumps to its heading, a code mention to its line), `references` (backlinks of the link's target, or of this note off a link; on a tag, every note carrying it, at line 0, in `notes_with_tag` order), `hover` (title and first 10 lines of the target, or "broken link"; on a tag, `**#name**: N notes` and up to 10 titles as a bullet list, range the tag), `documentHighlight` (`same_target_links`, kind text; `null` when empty), `inlayHint` (below), `documentSymbol` (headings nested by level), `workspace/symbol` (`search_notes` over every open workspace, at most 100), `completion`, `prepareRename`/`rename`, `workspace/willRenameFiles`, `foldingRange`, `codeLens`, `codeAction`, `documentLink` (one per link with a target path, over the whole link: the target's `file:` URI with `#anchor` as written, or `#L<line>` for a code mention with a line; none for broken links and external URLs; no `documentLink/resolve`), `executeCommand`.
- **Inlay hints.** After each link `link_titles` returns for the requested range: ` <title>` (leading space, padding left, no kind). Always answered; display is the client's switch (off by default in Neovim, `vim.lsp.inlay_hint.enable()`).
- **Folding ranges.** One per heading section, from the heading's line to the line before the next heading of the same or a higher level (or the end), and the frontmatter block as kind `region`; one-line ranges are left out. Fenced code blocks are not folded (the parse does not expose block ranges).
- **Code lenses** come resolved (no `codeLens/resolve`): `N backlinks` on the title line (the first level-1 heading, else line 0) running `mdroots.backlinks`, and `N links` on every other heading that other notes name by anchor (`Workspace::anchor_backlinks`) running `mdroots.anchorLinks`. No lens for a count of 0. After a refresh, a watcher-driven re-publish or a finished background open the server sends `workspace/codeLens/refresh`, only to clients that advertise `workspace.codeLens.refreshSupport`.
- **Code action: extract note.** On a non-empty selection in a Markdown note, `textDocument/codeAction` returns one action of kind `refactor.extract.note`, titled `Extract to new note: <file name>`, from `Workspace::extract_note`. Its edit is `documentChanges`, in order: `CreateFile` (`overwrite: false`, `ignoreIfExists: false`), a `TextDocumentEdit` inserting the new note's content, and a `TextDocumentEdit` replacing the selection with the link. The client applies it. The name is re-checked against the disk on every request, because Neovim truncates an existing file on `CreateFile` whatever its options say. `context.only` is honoured (`refactor` and `refactor.extract` match); an empty selection, an Org note, an unknown file or another kind gets `[]`.
- **Completion** triggers: `[`, `(`, `#`, `:`. After `[[`: notes by fuzzy search, at most 50, title as detail, inserting what `wiki_target` returns (the stem, or a path when another note shares the stem, case-insensitively, or the stem would resolve to another note from this one). After `[[note#` (or `[[#` for this note): its headings, without the H1 title. After `](`: relative paths to notes, percent-encoded like `link_to`. After a blank and `#`: tags with note counts. A `#` as the first non-blank character of a line starts a heading: the server returns an empty list, so no popup.
- **Rename.** `textDocument/rename` on a link to a note, or on this note's H1, renames that note: the new name is a file stem (no directory or extension); the result is the link edits plus a `RenameFile` op (as marksman does). On the H1 it renames the file only, not the heading text.
- **Commands.** `workspace/executeCommand`; the first argument is the URI of a note, whose workspace answers. Note lists are `[{uri, range, message?}]` (`range` empty, at column 0 of the hit's line), in the order below; the client must keep it. A graph command (`orphans`, `missingBacklinks`, `related`) on a lazy root (including a note still served alone while its root opens) fails with `RequestFailed` and a `window/showMessage` (Error). A first argument that is not the URI of a note mdroots serves gets `null`; `search` without its query, a non-numeric `limit`, `linkTo` without a target URI and a bad `query` object get `InvalidParams`.
  - `mdroots.backlinks <uri>`: `Location[]` of links to the note from other notes (self-links excluded, one per line). `mdroots.anchorLinks <uri> <slug>`: `Location[]` of links from other notes naming the note's heading with that slug by anchor (the `N links` lens).
  - `mdroots.orphans <uri>`: the notes nothing links to, line 0, by path. `mdroots.missingBacklinks <uri>`: one entry per pair `from → to` where `to` does not link back: `from` at line 0, `message` `→ <to, root-relative>`, by `from`, then `to`. `mdroots.related <uri>`: notes two links away from this one, most intermediaries first, `message` `<k> via`.
  - `mdroots.search <uri> <query> [limit]`: full-text hits (`full_text`, `limit` default 50) at each hit's line, the snippet as `message`.
  - `mdroots.query <uri> [object]`: the `notes` flags as a JSON object in camelCase, run as a `NoteQuery` (`query`): `Location[]`, one per note at line 0, the title as `message`, in the query's sort (default title A–Z; the client must keep it). Every field is optional, and the argument may be missing or `null`. `paths` (the positional PATHs), `exclude`, `linkTo`, `linkedBy`, `related`: arrays of `file:` URIs (repeatable flags: and); `linkTo`, `linkedBy` and `related` name indexed notes, `paths` and `exclude` any file or directory under the root. `tag`: an array of tag expressions as `notes -t` parses them (and). `match`: one string, as `NoteQuery` (no repeat). `tagless`, `orphan`, `missingBacklink`: booleans. `created`, `modified`: a day, as `notes --created` reads it; `createdAfter`, `createdBefore`, `modifiedAfter`, `modifiedBefore`: dates, as `notes` parses them. The bounds of one timestamp intersect whatever the key order: the latest lower bound and the earliest upper bound hold, so `{"created": D, "createdBefore": D+17}` stays on day D. `sort`: a string as `notes -s`. `limit`: a non-negative integer. An unknown field, a value of the wrong type or that does not parse, a path outside the root or a `linkTo`/`linkedBy`/`related` URI that is not an indexed note is `InvalidParams` with a message naming the field. A graph filter (`orphan`, `missingBacklink`, `linkTo`, `linkedBy`, `related`) on a lazy root fails like the graph commands. Neovim: `:MdrootsQuery {json}` and the tag picker (`<leader>nt`) feed the quickfix list.
  - `mdroots.check <uri>`: `[{uri, diagnostics}]`, one entry per indexed note by path (`diagnostics` may be empty), the diagnostics as published, after the `mdroots.diagnostics` setting (`off`: every entry empty).
  - `mdroots.tags <uri>`: `[{name, count}]` in `tags` order. `mdroots.linkTo <uri> <target-uri> [label]`: the link text `link_to` writes (a JSON string; the client inserts it).
  - `mdroots.reindex <uri>`: a full `refresh` of the workspace, then the open documents are re-published (as after `didSave`); `null`.
  - `mdroots.info <uri>`: root, mode, reason and file count, then one `<name>: <value> (<source>)` line per effective setting (the source as `mdroots roots` prints it), also sent as `window/showMessage`. `mdroots.renameFile <from-uri> <to-uri>`: the rename edit, sent to the client as `workspace/applyEdit`.
- **`workspace/willRenameFiles`.** The link edits for every file and folder in the request, from `Workspace::rename_notes` (§3.2); the client moves the files (D10). Registered with two filters: `**/*.{md,markdown,org}` matching files, `**` matching folders. Each pair goes to every open workspace whose root contains the old path or, for a folder, lies under it; one `rename_notes` call per workspace plans its pairs together. A root is never opened here. A pair whose new path is outside a workspace's root is skipped by the library, so moving a whole notebook folder yields no edits for that notebook, which is right because links inside it are relative to it or by stem; a link into it from an outer workspace follows. The answer is one `WorkspaceEdit` of `documentChanges` with `TextDocumentEdit`s only (no `RenameFile`), or `null` when nothing needs an edit. Neovim 0.12 does not send the request (`<leader>nr` uses `mdroots.renameFile`); oil.nvim and file-tree plugins that send it get the link fixes.
- Not built ([ROADMAP](../ROADMAP.md)): semantic tokens.

## 4. Scheduling

gopls practices applied in-process ([gopls notes](../research/gopls.md)).

**Built** (§3.6): in-order requests on the message-loop thread, no worker pool; `$/cancelRequest` and a `didChange` on the document cancel queued requests, not a running one; one phase of diagnostics, 500 ms after the last `didChange`; requests never open a root, so a document is served single-file until its root is open; a root opens on one background opener thread, started by the first `didOpen` of a file in it; `workDoneProgress` (fallback one `showMessage`) for an open > 1 s.

**Target** (deferred, [ROADMAP §3](../ROADMAP.md#3-deferred-work)): views per overlay change or `change_log` advance, cancelling queries on older views; a small worker pool for workspace symbols, full text, large reference queries and pull diagnostics; two-phase diagnostics (the edited document at once, cross-file after the debounce) with a save-only trigger; a recent-mtime guard (a file modified < 2 s ago counts as maybe changed); a read semaphore (≤ 64 concurrent reads per process, fewer per lazy or virtual root) so a root on a virtual filesystem such as [EdenFS](https://github.com/facebook/sapling) can't starve a local vault; opening the root's DB while answering `initialize`.

The daemon question (D3) is reopened only with numbers: total `phys_footprint` with 10 editors plus ramble, CPU spent on duplicate overlay parsing, and p99 save-to-visible latency between peers.

## 5. Cache hygiene

Cache file names: [roots §5](roots.md#files). Failures, corruption, GC and last-seen stamping: [index §1.6–1.7](index.md#16-failures-and-races).

## 6. CLI

The `mdroots` binary (crate `mdroots-cli`) depends on the `mdroots` facade, and on `mdroots-lsp` for `mdroots lsp`. Each command only formats §3.2 results, so the CLI, as the first embedder, keeps the API honest. Arguments are parsed by hand.

| Command | Does | Exit |
|---|---|---|
| `notes [FLAG...] [PATH...]` | notes matching every flag (`zk list` filters over `Workspace::query`): `-t/--tag EXPR` (repeatable), `--tagless`, `-m/--match QUERY`, `-x/--exclude PATH` (repeatable), `--created DAY`, `--modified DAY`, `--created-after/--created-before/--modified-after/--modified-before DATE`, `--orphan`, `--missing-backlink`, `-l/--link-to NOTE`, `-L/--linked-by NOTE`, `--related NOTE` (all repeatable; each repeated `-m` must match too, and keeps its last word as a prefix), `-s/--sort KEY[+\|-]`, `-n/--limit N`. The notebook is found from the first PATH (default `.`) with `open_dir` (a file: `open_for`), so a subdirectory sees links from the whole notebook; PATHs then filter to notes under them (none: every note, as in zk). `-f/--format`: `path` (default; cwd-relative where possible; zk prints notebook-relative), `tsv` (path, title, tags joined with `,`, modified, created; times RFC 3339 UTC; `\`, tab, CR, LF escaped as `\\`, `\t`, `\r`, `\n`), `json` (one array) or `jsonl` (objects with absolute `path`, `title`, `tags`, `modified`, `created`; times RFC 3339 UTC or `null`). `-0/--delimiter0` ends records with NUL. A bad flag value prints `mdroots: FLAG: why`. NOTE (`-l`, `-L`, `--related`) must be an indexed note's path as given (no extension guessing; zk also matches partial paths such as `n`), else `mdroots: --link-to: not a note: NOTE`, exit 2 (zk exits 1). `-m` has no stemming, phrases, OR/NOT or field filters, unlike zk's FTS5 syntax | 0, with or without notes; 2 on a usage error or a NOTE that is not a note |
| `tags [--sort name\|count] [--format tsv\|json] [PATH]` | each tag with its note count (`Workspace::tags_under`, case-folded) in the notebook of PATH (default `.`, found as for `notes`), counting notes under PATH when given: `name<TAB>count` lines or a JSON array of `{name, count}`. Sorted by lowercase name, or by count (most first, then name) | 0 |
| `check [--quiet] [--fail-on error\|warning\|never] [PATH...]` | diagnostics of the notes under each PATH (default `.`): a directory is opened with `open_at` and all its notes checked; a file with `open_for`, only that file checked. A file under several PATHs is reported once, under the first. Stdout: `path:line:col: severity: message` (1-based; columns in characters), files by canonical path, then by position. Summary `N files, E errors, W warnings, I info, H hints` to stderr unless `--quiet`. `--fail-on` sets this run's exit threshold; severities come from the vote and tool configs (zk `dead-link`) | 1 on any error or warning (`--fail-on error`: any error; `never`: never), else 0 |
| `roots PATH` | the root chosen for PATH: `root:` (absolute), `mode:`, `why:` (discovery's one-line reason), `files:` (indexed count), `cache:` (DB path, or `memory`), `role:` (`reconciler`, `peer`, or `none` in memory), one `nested:` line per nested root, then one `NAME: VALUE (SOURCE)` line per setting (`Workspace::settings`); SOURCE is `TOOL FILE KEY` (config key), `TOOL default` (marker default), `vote` or `default` | 0 |
| `resolve FROM LINK` | each target, then `step:` and `status:` | 1 without a target |
| `backlinks NOTE` | `path:line: title` per link from another note (`Workspace::backlinks` minus NOTE's own links) | 0 |
| `search [--paths] [--] QUERY [PATH]` | notes containing every word of QUERY (`Workspace::full_text`; last word also as a prefix), one `path:line: snippet` per hit (1-based line), at most 1,000. A file PATH opens its discovered root with `open_for`; as reconciler it answers from the DB's FTS5 table. A directory PATH (default `.`) is opened with `open_at`, in memory, and scanned naively, even when it is a discovered root. `--paths` prints one path per hit (one hit per note; `notes --match` is the zk-parity form, without the 1,000 cap). `--` ends options, so a query may start with `-` | 1 without a hit, else 0 |
| `lsp [--stdio] [--log FILE]` | the language server (§3.6) on stdin/stdout until `exit` or EOF. `--stdio` is accepted and ignored (stdio is the only transport; many editor configs pass it). `--log` appends one line per message to FILE: time, direction (`<-` from client, `->` to it), method or `response`, id | 0 (2 on a protocol error) |

Paths under the current directory print relative to it, others absolute. `notes` and `tags` exit 0 on an empty result, as `zk list` does, so a clean `--orphan` sweep is not a failure; `check --fail-on` is the health gate. Usage errors, unknown arguments, `--help` and a bare `mdroots` print usage to stderr and exit 2, so a server never starts implicitly (it would hang under CI or cron). Other errors print `mdroots: <message>` and exit 2.

Every command uses the persistent cache (D5, D9): `check` on a file, `notes`, `tags`, `roots`, `resolve`, `backlinks`, `search` on a file and `lsp` open discovered roots through it; `check` and `search` on a directory use `open_at`, in memory. Only `lsp` turns on the native watcher; one-shot commands never start a thread. `MDROOTS_CACHE_DIR`, when set and non-empty, replaces the cache dir for every command, `lsp` included (`Options::cache_dir`); tests point it at a temp dir. Hidden test commands drive the many-process fixtures ([roots §7](roots.md#7-fixtures)).

On the corpus: `check tests/corpus/zkvault` reports 5 hints and exits 0 (78% of its explicit links resolve, under the 80% hint threshold); `check tests/corpus/zk-min` reports 1 warning and exits 1. Timings: [index §1.5](index.md#15-short-lived-instances) and [§3.3](index.md#33-diagnostics).

What is not built: [ROADMAP](../ROADMAP.md).

## Neovim 0.12+ example

Files in [`editors/nvim/`](../../editors/nvim/) (drop into your Neovim config dir):

| File | What |
|---|---|
| `lsp/mdroots.lua` | config auto-discovered by `vim.lsp.config`: `cmd = {'mdroots','lsp'}`, `filetypes = {markdown, org}`, a `reuse_client` predicate, `workspace_required = false`, a commented `settings = { mdroots = { diagnostics = … } }` block |
| `plugin/mdroots.lua` | `vim.lsp.enable('mdroots')` plus optional `LspAttach` extras: `gd`, `gO` (LSP symbols), guarded autotrigger completion, codelens, LSP folding, `<leader>ns` search, `<leader>nb` backlinks to loclist, `<leader>nr` rename note, `<leader>nn` extract the visual selection to a new note (the extract-note code action), `<leader>no` orphans, `<leader>nm` missing backlinks, `<leader>nR` related notes and `<leader>n/` full-text search (each into the quickfix list, in server order), `<leader>nt` tags (a picker), `:MdrootsInfo`, `:MdrootsCheck` (every diagnostic into the quickfix list), `:MdrootsReindex`. Codelens and folding turn on only when the server offers code lenses and folding ranges (it does, §3.6); `<leader>nn` is always mapped |

Choices (checked against the 0.12.5 runtime):

| Choice | Why |
|---|---|
| No `root_markers` | 0.12 starts a client for a matching filetype even with no root (root_dir nil; `workspace_required` defaults false). Root logic lives only in mdroots. The client then sends `workspaceFolders = null`, so the server never depends on workspace folders |
| `reuse_client` | with root_dir nil the default already reuses the client; the predicate matters only if something sets root_dir, keeping one process. A reused client sends no `didChangeWorkspaceFolders`; fine, as mdroots finds roots from file paths. One process per Neovim; instances share the root's SQLite cache (D3) |
| `cmd = {'mdroots','lsp'}` | the subcommand is required (§6) |
| `settings`, not `init_options` | Neovim sends `settings` via `workspace/didChangeConfiguration` (the server reads `mdroots.diagnostics` from it, §3.6); `init_options` is sent once, so can't carry changing settings |
| `gO` remap | the markdown ftplugin maps `gO` to a treesitter outline; mapping `vim.lsp.buf.document_symbol` in `LspAttach` runs after the ftplugin |
| Backlinks handler | `Client:exec_cmd` drops the result without a handler; ours feeds `Location[]` to the loclist via `vim.lsp.util.locations_to_items(result, client.offset_encoding)` |
| Notebook-list handler | `ordered_to_qflist(title)` builds one quickfix item per location in server order, text `message` else the line (the loaded buffer's, so unsaved edits show, else the file's). `locations_to_items` would re-sort by file and drop `message` |
| Rename | `grn` on a link or H1 uses `textDocument/rename`; `<leader>nr` calls `mdroots.renameFile` |
| Completion | enabled only if `supports_method('textDocument/completion')`; nvim-cmp/blink.cmp users set `vim.g.mdroots_autocomplete = false` |
| Position encoding | Neovim offers `utf-8` first; mdroots picks it |
| Filetypes | `markdown`, `org`. Not `mdx` (no default filetype), `quarto` (`.qmd`) or `rmd` (`.Rmd`) for now |

Other built-in 0.12 mappings cover the rest: `K`, `grr`, `grn`, `]d`/`[d`, `<C-]>` via `tagfunc`, `gra` (code actions: extract note on a visual selection).

### Smoke test

```
nvim --clean --headless -u NONE -c 'luafile bench/nvim_smoke.lua'   # from the repo root
```

[`bench/nvim_smoke.lua`](../../bench/nvim_smoke.lua) loads `editors/nvim` against the real `mdroots lsp` (`$MDROOTS_BIN`, else `$CARGO_TARGET_DIR/debug/mdroots`, else `target/debug/mdroots`; build with `cargo build -p mdroots-cli` first). It never writes the corpus vaults: edits go to scratch notes under `/tmp/mdroots-smoke/` (wiped at start), and the server's `XDG_CACHE_HOME` points there too. It waits on conditions, not sleeps; every line is an assertion, and a failure exits non-zero.

Checks: one client with root_dir nil and settings sent; utf-8 position encoding; maps and `:MdrootsInfo`, `:MdrootsCheck`, `:MdrootsReindex` (`:MdrootsInfo`'s `window/showMessage` names the root); `gO` overriding the ftplugin; definition; symbols; backlinks via `exec_cmd` into the loclist (self-links excluded); `<leader>nR` on a vault A note puts `mdroots.related` into the quickfix list in server order with the server's `N via` text; choosing `journal` in the `<leader>nt` tag picker fills the quickfix list from `mdroots.query`; `[[#` heading completion (H1 title excluded); no completion for `#` at line start; a broken-link diagnostic; cross-file goto from a vault README (backlinks and this goto are polled until the root's background open finishes); the LSP `foldexpr` on a note; the `6 backlinks` code lens on a vault A note; extract note via `<leader>nn` on a visual selection (next free file name, selection becomes the link, nothing written outside the scratch dir); `mdroots.renameFile` (the `workspace/applyEdit` fixes the linking note and moves the file, the buffer follows); unmodified vault buffers.

Passes on NVIM 0.12.5: 40 checks (35 `check` calls, two in loops over 4 files and 3 buffers); attach ≈ 25–500 ms. Not covered: the fold ranges themselves, anchor `N links` lenses, the progress notification.

## Planned design

Not built. [ROADMAP §3](../ROADMAP.md#3-deferred-work) orders the phases; [D10](../DECISIONS.md#d10-mdroots-proposes-edits-the-editor-applies-them) is the decision behind them. Each item moves into §3.2 or §3.6 when it ships.

### Fixes (phase 1: link repair)

Library first (D2): a fix is a `WorkspaceEdit` (`rename.rs`) the caller applies; the LSP turns each into a `CodeAction`.

```rust
let fixes = ws.fixes(&path, range, &cancel)?;   // Vec<Fix>, in source order of their link
#[non_exhaustive] pub struct Fix { pub title: String, pub kind: FixKind, pub preferred: bool, pub edit: WorkspaceEdit }
#[non_exhaustive] pub enum FixKind { QuickFix(Diagnostic), ConvertLink, LinkSelection }
```

`range` is in bytes. A non-empty range selects the links overlapping it; an empty range at `off` (a cursor) selects the link whose range contains `off` or ends at it, so a cursor just after `]]` or `)` counts. `QuickFix` carries the diagnostic it fixes, range and code included, so the server sets the action's `diagnostics` from the fix alone.

| Trigger | Fixes | Built on |
|---|---|---|
| `broken-link` | `Change to <title>`: up to 3 notes from `search_notes` on the target's stem, exact, prefix or substring tiers only (no gapped subsequence); only the link's path is replaced, in the form it was written in (wiki stem or path, relative or root-relative, with or without extension); syntax, anchor and label stay | `search_notes`; the path rewrite `rename_note` uses (`rename.rs`) |
| `broken-link` | `Create note <file>`: `CreateFile` (`overwrite: false`, `ignoreIfExists: false`) for the path the link names (stem links: `<stem>.<from's extension>` in `from`'s directory), content `# <target as written>\n`; never preferred; not offered when the path exists on disk, is outside the root, or is not a note extension | the extract-note create path |
| `broken-anchor` | `Change to #<slug>`: up to 3 headings of the target, same tiers over slug and text | `outline` |
| `ambiguous-link` | `Pick <rel path>`: one per candidate in `Diagnostic.related`; the path replaced as for `Change to`, with the shortest form that resolves to that candidate alone (`wiki_target` for stem links) | `wiki_target`; the `rename_note` path rewrite |
| a link under the cursor (no diagnostic) | `Convert to wiki link`: `[[<wiki_target>#anchor\|label]]`. `Convert to Markdown link`: the root's Markdown style when it votes one, else relative with the extension. Anchor and label kept (a wiki link without a label gets the target's title) | `link_text` |
| a non-empty selection that is not a link | `Link to <title>`: the selection becomes the label of a link to the best `search_notes` hit (exact or prefix tier only), up to 3 | `search_notes`, `link_to` |

Rules:
- A fix is `preferred` only when it is the single candidate of its trigger (one close note, one heading; never for ambiguity or `Create note`), so editors may apply it with one key and it never creates a file (D10).
- Links in code or comments get no fixes (D7). Org notes: broken-link and ambiguous fixes only; no conversion.
- Fixes are computed per request from `Workspace::diagnostics` over the current store and overlay (nothing is cached, nothing rides in the diagnostic's `data`), so a fix requested during the 500 ms publish debounce matches the current text.
- LSP kinds: diagnostic fixes `quickfix` (each with `diagnostics` set to the diagnostic it fixes), conversion `refactor.rewrite.link`, link selection `refactor.rewrite.linkSelection`; extract note keeps `refactor.extract.note`. `context.only` filters by prefix, as today. The empty-range early return applies to extract note only.
- Out of scope here: `source.fixAll`, adding a missing heading to the target, `mdroots check --fix` (D10).

### Smaller requests (phase 3)

