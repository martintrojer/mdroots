# Changelog

All crates share one version. `mdroots` (the library) and `mdroots-cli` (the
`mdroots` binary) are the public crates; the `mdroots-*` crates they depend
on are internal and pinned to the exact version.

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
