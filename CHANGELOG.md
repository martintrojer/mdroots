# Changelog

All crates share one version. `mdroots` (the library) and `mdroots-cli` (the
`mdroots` binary) are the public crates; the `mdroots-*` crates they depend
on are internal and pinned to the exact version.

## 0.2.4

- `mdroots lsp` answers `textDocument/documentLink` (and advertises
  `documentLinkProvider`, no resolve): one link per link with a target
  path, covering the whole link, targeting the file's URI with `#anchor`
  as written, or `#L<line>` for a code mention with a line. Broken links
  and external URLs get none. For clients that take link targets from
  document links rather than goto.

## 0.2.3

- `Workspace::notes_with_tag(tag)`: the notes carrying a tag, compared
  case-insensitively (`#Rust` and `tags: [rust]` both match `rust`), sorted
  by path like `notes()`.
- `NoteSummary::modified`: the file's modification time, `None` for a note
  that exists only as an editor overlay. `NoteSummary` is `#[non_exhaustive]`,
  so code outside the crate cannot build it with a struct literal and is not
  broken by the new field.
- CI: the gate runs on GitHub Actions for Linux and macOS, plus the MSRV.

## 0.2.2

- `Frontmatter::fields()`: the top-level front matter entries as written,
  each with its byte range in the document and nested maps as a tree
  (`Field`, `FieldValue`, `FieldValue::display`). Also new:
  `Frontmatter::parsed()` (false when a non-blank block has no key) and
  `Frontmatter::inner()` (the text between the fences).
- `ParseOptions::unfenced_frontmatter` (default true): set it to false to
  leave [Logseq](https://logseq.com), [MultiMarkdown](https://fletcherpenney.net/multimarkdown/)
  and JSON headers as prose.
- TOML front matter values that only the parser reads (in tables, dotted
  keys) are written as TOML writes them, in `entries()` too: `1.0` stays
  `1.0` (was `1`), and a nested array is one item `[1, 2]` (was `1, 2`).

## 0.2.1

- Front matter: block scalars (`|`, `>`) whose lines look like keys or list
  items are text on one line; nested list items come from the YAML parser
  (`[a, [b, c]]` gives `a` and `b, c`); a block-scalar list item (`- |`) is
  its text; map list items (`{…}`) are placeholders, so they never become
  tags or aliases; scan and parser are combined in linear time (20k keys in
  about 20 ms instead of 0.6 s). Ported from ramble, a TUI markdown reader
  by the same author.
- `Options::code_dirs`: extra directories code mentions (`` `src/main.rs:12` ``)
  resolve against, after the note's directory and before the root.
- Linux: kernel pseudo-filesystems (`/proc`, `/sys`, ...) count as virtual,
  so opening a file there never walks them.

## 0.2.0

Language server:
- Picks up files changed on disk by another editor, git or sync without a
  save: a native watcher in `mdroots lsp`, only for the process that writes a
  root's cache, only on local marker, VCS and loose roots (never on virtual
  or remote filesystems).
- Folding by heading sections and frontmatter.
- Code lenses: "N backlinks" above the title, "N links" above headings that
  other notes link to by anchor (command `mdroots.anchorLinks`).
- Extract note: the `refactor.extract.note` code action moves a selection
  into a new note and replaces it with a link in the root's style.
- Background open: a newly opened file is served alone at once while its root
  is discovered and indexed; a progress notification for opens over a second.
- `mdroots lsp --stdio` is accepted (and ignored; stdio is the only transport).

Library (`mdroots`):
- `Workspace::full_text` and `Hit`: full-text search over an
  [FTS5](https://sqlite.org/fts5.html) table in the per-root cache, with a
  plain scan for unsaved text and memory mode.
- `Options::watch`, `Workspace::watching`, `refresh_paths`, `subscribe`.
- `Workspace::link_style` and `link_to`: links written the way the root
  already writes them (an existing [zk](https://github.com/zk-org/zk) or
  [Obsidian](https://obsidian.md) config first, else a vote over the notes).
- `Workspace::extract_note`, `WorkspaceEdit::create`, `frontmatter_range`,
  `anchor_backlinks`, `Workspace::open_single`, `Workspaces::get`.
- Whole-root queries no longer re-resolve every link per file: diagnostics
  for every note of a 3,000-note root take about 50 ms instead of 26 s.

Cache:
- Per-root DB schema v2 (adds the full-text table); v1 files are deleted by
  GC after 7 days.
- A corrupt DB is rebuilt into a new generation file; a daily GC removes old
  generations, old schema files, roots unseen for 30 days or whose path is
  gone, and the least recently seen roots over 1 GB. Roots another process
  has open are never touched.

CLI:
- `mdroots search QUERY [PATH]`.

What is still missing or unvalidated: [docs/ROADMAP.md](docs/ROADMAP.md).

## 0.1.0

First release: root discovery that never walks a virtual, remote or huge
tree; parsing and link resolution for zk, Obsidian,
[marksman](https://github.com/artempyanykh/marksman), Foam, Dendron, Logseq,
org-mode and plain relative paths; a per-root
[SQLite](https://sqlite.org) cache with one writer per root; the `mdroots`
CLI (`check`, `roots`, `resolve`, `backlinks`) and `mdroots lsp`.
