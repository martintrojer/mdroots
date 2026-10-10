# Changelog

All crates share one version. `mdroots` (the library) and `mdroots-cli` (the
`mdroots` binary) are the public crates; the `mdroots-*` crates they depend
on are internal and pinned to the exact version.

## 0.2.9

Fixes from a review of 0.2.8.

- Dates: frontmatter dates with out-of-range offsets (`+99:99`, `+-1:00`)
  are no longer read as created times. A non-ASCII `--created-*` or
  `--modified-*` value, or a huge relative count
  (`"9223372036854775807 months ago"`), is a usage error (exit 2)
  instead of a panic or an empty result. Frontmatter and query dates now
  share one parser.
- Written links resolve back to their note. Rename, `link_to` (and
  extract-note) and LSP completion percent-encode `% ( ) # < > &`, spaces
  and control characters in Markdown destinations. A root-relative or
  wiki link that a same-named file next to the linking note would shadow
  gets a leading `/` or a file-relative path instead. `[[` completion
  inserts what `link_to` writes, so notes sharing a name stay distinct.
  `](` completion handles `..` in the current file's path.
- Resolution: an Org heading's `:ID:` resolves `[[id:…]]` to its file. A
  site-rooted link to an existing unindexed file under the docs dir is
  unindexed, not broken. Title links match whatever the Unicode
  composition (NFC). On case-insensitive filesystems, `file:` links into
  the root match its path case-insensitively.
- JSON frontmatter decodes `\uXXXX` surrogate pairs (escaped emoji).
- In-memory search (no cache, peers, unsaved buffers) folds diacritics
  like the index: `cafes` finds a decomposed `cafés`, and kana, Greek and
  Cyrillic accents are no longer folded.
- LSP: a buffer opened before its file exists gets diagnostics, symbols
  and info once it is saved or edited, without reopening it.
- Root discovery and registry:
  - Stale rows are skipped: a deleted nested `.git` goes to the parent
    root with no re-walk on every open.
  - An `.mdroots` or `.git` added at a loose or lazy root's own
    directory is seen at once, not after the 7-day retry.
  - A file below a registered root on another filesystem (a new mount)
    is decided afresh.
  - Workspace-folder roots from earlier sessions, including lazy ones
    written by 0.2.8, no longer persist or block an enclosing loose or
    git root.
  - Loose roots no longer grow across a mount boundary.
  - Directories with non-UTF-8 names are descended instead of skipped.
    `Probe::read_dir` returns `OsString` names (an API change for `Probe`
    implementors).
- Cache dir: the shared `/var/tmp/mdroots-<uid>` fallback never follows
  a planted symlink, and a candidate it rejects keeps its mode.
- `zkdiff` opens notebook databases in directories with `?`, `#` or `%`.
- Docs, code comments and tests corrected; duplicated helpers merged.

## 0.2.8

- Tag expressions take `AND` (as the comma) and parentheses:
  `-t "career AND NOT projects"`, `-t "(a OR b), NOT c"`. Before, both
  were read as tag names and matched nothing; an unclosed `(` or a
  dangling `AND` is now a usage error naming its column. Keywords may be
  separated by tabs as well as spaces.

## 0.2.7

- `mdroots notes [FLAG...] [PATH...]` lists notes with
  [zk](https://github.com/zk-org/zk) `list`'s filters: `--tag` with zk's
  tag expressions (`a, NOT b`, `a OR b`, globs), `--tagless`, `--match`,
  `--exclude`, created and modified dates (`--created-after 2024-01-01`,
  `--modified-after "2 weeks ago"`), `--orphan`, `--missing-backlink`,
  `--link-to`, `--linked-by`, `--related`, `--sort KEY[+|-]`, `--limit`,
  `--format path|tsv|json|jsonl` and `-0`. PATHs are filters inside the
  notebook found from the first one, so a subdirectory still sees links
  from the rest of the notebook.
- `mdroots tags [--sort name|count] [--format tsv|json] [PATH]` lists tags
  with note counts. Tags differing only in case are one tag.
- `notes` and `tags` exit 0 with or without results.
- `mdroots search --paths` prints only the paths of matching notes.
- `mdroots check --fail-on error|warning|never` sets when it exits 1
  (default `warning`, as before).
- `mdroots roots` also prints the root's settings (link style, tag
  syntaxes, broken-link severity, docs dir) and where each came from.
- Library: `Workspace::query` with `mdroots::query::{NoteQuery, TagExpr}`,
  the graph queries `orphans`, `missing_backlinks`, `related`,
  `links_from` and `links_to`, `Workspace::open_dir`, `tags_under`,
  `settings`, and `NoteSummary::created` (frontmatter `date`, else the
  file's birth time).
- Repeated `-l`, `-L` and `--related` all apply (and), like every other
  filter; a `notes` flag where a value belongs is a usage error.
- The link style for inserted links: a wiki link without `/` no longer
  counts as root-relative in the vote, so an Obsidian-style vault gets
  `[[stem]]`; a root with no links yet gets its marker's default
  (Obsidian `[[stem]]`, zk Markdown without `.md`).
- Tool config fields that changed nothing (Obsidian
  `attachmentFolderPath`, marksman `title_from_heading`, Logseq
  `:file/name-format`) are no longer read.
- Docs: the README is a short pitch with a benchmark against zk and
  marksman ([method](docs/research/benchmark.md),
  `bench/compare/run.py` reproduces it); the specs and decisions match
  the code; the doc build is part of the gate.

## 0.2.6

- `mdroots --version` (or `-V`) prints `mdroots <version>` and exits 0;
  before it printed the usage. The release smoke test runs it.
- Fix: a small root (fewer than 50 directories) is never rate-aborted.
  Before, a few slow listings on a loaded machine (say, during a build)
  could cross the 100 ms window and turn a small notes repo into a lazy
  root; the wall budget still bounds such walks. Larger slow roots are
  still caught by the rate check at about 100 ms.

## 0.2.5

- Releases: a `v*` tag builds `mdroots` for macOS (arm64) and Linux
  (x86_64) and attaches the archives to the GitHub release.

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
  about 20 ms instead of 0.6 s). Ported from [ramble](https://github.com/martintrojer/ramble), a TUI markdown reader
  by the same author.
- `Options::code_dirs`: extra directories code mentions (`` `src/main.rs:12` ``)
  resolve against, after the note's directory and before the root.
- Linux: kernel pseudo-filesystems (`/proc`, `/sys`, ...) count as virtual,
  so opening a file there never walks them.

## 0.2.0

Language server:
- Picks up files changed on disk by another editor, [git](https://git-scm.com) or sync without a
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
[marksman](https://github.com/artempyanykh/marksman), [Foam](https://foambubble.github.io),
[Dendron](https://www.dendron.so), Logseq, [org-mode](https://orgmode.org)
and plain relative paths; a per-root
[SQLite](https://sqlite.org) cache with one writer per root; the `mdroots`
CLI (`check`, `roots`, `resolve`, `backlinks`) and `mdroots lsp`.
