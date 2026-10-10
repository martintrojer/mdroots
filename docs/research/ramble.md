# ramble: code ported into mdroots, and how ramble embeds it

[ramble](https://github.com/martintrojer/ramble), a separate TUI markdown
reader by the same author (Rust,
[pulldown-cmark](https://github.com/pulldown-cmark/pulldown-cmark) 0.13, no
[tokio](https://tokio.rs)), embeds the
`mdroots` library (0.2.3 or later) in process. Both projects are MIT
licensed with the same author, so code was copied with the ramble commit
recorded in a source comment or the commit message (tables below).

## How ramble uses mdroots

- **Syntax.** ramble's `src/doc.rs` takes links, headings (with slugs
  from `mdroots::syntax::slug`) and front matter from
  `mdroots::syntax::parse_with`, with `markdown_options()` for both parses
  so ranges line up. Its front matter fold reads
  `mdroots::syntax::Frontmatter`.
- **Backend.** `src/app/mdroots_glue.rs` runs one worker thread that owns
  `mdroots::Workspaces`. A page is answered first from
  `Workspace::open_single` (status `mdroots ○`), then from its root through
  `Workspaces::for_path` (`mdroots ●`); `Workspaces::get` reuses a ready
  root. The page's text goes in with `set_overlay`.
- **Links and goto.** `document_links` gives link targets, anchors and
  broken-link dimming; `gd` and `Enter` follow those targets. `K` shows
  `preview` of the target, from the target's own root.
- **Pickers.** `notes`, `notes_with_tag`, `full_text`, `tags` and
  `backlinks` feed the notes, search, tags and backlinks pickers; the notes
  picker sorts by `NoteSummary.modified`.
- **Live updates.** Workspaces open with `Options::watch(true)`; the worker
  subscribes to each root it answers from and re-answers the page when a
  change lands under that root. A page reload calls `refresh_paths`.
- **Config.** The default config has no language servers: every page uses
  mdroots unless the user configures an `[[lsp.server]]`.

## What ramble deleted, and what it keeps

Deleted: `src/frontmatter.rs` (its YAML/TOML line scanner) and its direct
[saphyr](https://github.com/saphyr-rs/saphyr) dependency, the link and
heading collectors and slug code in `src/doc.rs`, and the [zk](https://github.com/zk-org/zk) and
[marksman](https://github.com/artempyanykh/marksman) language servers from
its default config.

Kept, by design:

- the layout parse in `src/doc.rs` (blocks, inlines, code spans for
  rendering), which mdroots does not model;
- code-path links and their follow (`src/app/codepath.rs`), with its own
  search dirs (page dir, VCS root, tree root); `Options::code_dirs` covers
  the same ground but ramble does not use it yet;
- its own file watcher (`src/app/watch.rs`) to re-render the page, and
  local dispatch for URLs, same-page anchors and links mdroots does not
  report;
- the LSP client (`src/lsp/`, `src/app/lsp_glue.rs`, the zk adapters in
  `src/notebook.rs`) as an optional backend, used only for pages a
  configured `[[lsp.server]]` serves.

## Ported

| ramble code (commit) | What it does | mdroots home |
|---|---|---|
| `src/doc.rs` `slugify`, `unique_slug`, `is_mark` (`75b8285`) | GitHub slugs keeping combining marks, `{#id}` override, `-1`/`-2` dedup | `mdroots-syntax` slug (`tests/slug.rs`) |
| `src/doc.rs` `collect_links` + `link_text_range_is_drawn_text` proptest (`75b8285`) | link range plus visible `text_range`, empty-text `[](x)`, nesting | `mdroots-syntax` structure (`tests/structure.rs`) |
| `src/doc.rs` `collect_code_spans`, `code_content` (`75b8285`) | inline code outside links/images; text = CommonMark code text | `mdroots-syntax` scan (CodeMention candidates) |
| `src/doc.rs` `from_bytes`, `options()` (`75b8285`) | NUL in first 8 KiB = binary, lossy UTF-8; pulldown option set | `mdroots-syntax` `parse_bytes`, `markdown_options()` |
| `src/frontmatter.rs` + unit tests (`5f6e7ed2b798`) | YAML/TOML: line scan keeps key order, source text, duplicates; parser decides value shape only; never fails | `mdroots-syntax` frontmatter |
| `src/frontmatter.rs` nested list items from the parser (`065bdf7`) + `nested_list_items_take_the_parser_items` | `l:\n  - a\n  - - b\n    - c` is `["a", "b, c"]` | `mdroots-syntax` frontmatter (0.2.1) |
| `src/frontmatter.rs` block-scalar list items (`01e1ef6`) + `block_scalar_list_items_are_their_text` | `- \|` with indented text is that text | `mdroots-syntax` frontmatter (0.2.1) |
| `src/doc.rs` `empty_front_matter`, `mask` (`5f6e7ed2b798`) | empty `---`/`---` or `+++`/`+++` at offset 0 is frontmatter (pulldown-cmark 0.13 emits none); fences blanked, offsets kept | `mdroots-syntax` structure |
| `src/lsp/position.rs` + tables, proptest (`75b8285`) | byte offset ↔ LSP position for UTF-8/16/32, `\n`-only line split | `mdroots-syntax` `LineIndex` (`tests/line_index.rs`) |
| `src/app/codepath.rs` `strip_position`, `resolve`, `locate` + pure tests (`75b8285`) | inline code naming an existing file is a link; full text first, then `:LINE[:COL]` stripped; `~/` via injected home; ≤ 512 bytes; rejects whitespace and `://` | `mdroots-syntax` scan, `mdroots-resolve` ladder code-mention branch (`ResolveCtx.code_dirs`) |
| `src/app/codepath.rs` `code_path_dirs` (`75b8285`) | extra code-mention search dirs (page dir, VCS root, tree root, deduped) | `Options::code_dirs` (0.2.1; the caller computes the dirs) |
| `src/nav.rs` `scheme`, `percent_decode`, `file:` forms + `resolve_table` (`d394783`) | schemes (≥ 2 chars, so `C:` is a path), lossy percent-decoding, `file:///`, `file://localhost/`, `file:/` | `mdroots-resolve` normalize (`tests/normalize.rs`) |
| `src/nav.rs` extensionless rule (`c2a138b7`) | `[x](a)` tries `a`, then `a.md` (zk default link style) | `mdroots-resolve` ladder |
| `src/lsp/uri.rs` `canonical_path` + symlink test (`75b8285`) | canonicalise the longest existing ancestor, append the missing tail | `mdroots-core` `FileSystem::canonicalize` |
| `tests/fixtures/zk/` (`75b8285`) | 5-note zk notebook checked live against zk 0.15.6 | `tests/corpus/zk-min/` |

## Not ported

| ramble code (commit) | Use | Why not |
|---|---|---|
| `src/app/watch.rs` + `tests/watch.rs` (`75b8285`) | lessons for the watcher: editors replace by rename, compare content not events, kick once after `watch()` so a write between load and watch is seen | built without porting (`crates/mdroots/src/watch.rs` reconciles by stat and content); the FSEvents replay lesson waits for replay ([ROADMAP §3](../ROADMAP.md#3-deferred-work)) |
| `src/lsp/uri.rs` `uri_to_path`, `canonical_uri` (`75b8285`) | URI ↔ path in `mdroots-lsp` | `mdroots-lsp` decodes `file:` URIs with [lsp-types](https://github.com/gluon-lang/lsp-types)' [`fluent-uri`](https://github.com/yescallop/fluent-uri-rs); zk's undeclared UTF-32 columns matter only to ramble's optional LSP backend |
| `tests/support/fake_lsp.rs`, `src/lsp/framing.rs` (`75b8285`) | a scripted test client | `mdroots-lsp` tests use `lsp_server::Connection::memory()` and the CLI tests drive the real binary |
| `src/notebook.rs` `walk_notes`, `src/app/launch.rs` `vcs_root` (`75b8285`) | the `ignore::WalkBuilder` flags and the VCS marker list | used as reference for the walk and marker table |

## Gaps ramble works around

Each is in [ROADMAP §3](../ROADMAP.md#3-deferred-work); the API is in
[library spec §3](../specs/library.md#3-public-api-sketch).

- **`subscribe` sends changed paths only.** ramble needs only "something
  under this root changed" and re-answers the page; it cannot tell a
  created, removed or renamed note apart.
- **`Preview` has no `summary`.** ramble's `K` popup shows the title, the
  front matter lines and the excerpt.
- **Unfenced front matter is a `ParseOptions` switch, not a workspace
  option.** ramble parses with `unfenced_frontmatter` off (a
  [Logseq](https://logseq.com), [MultiMarkdown](https://fletcherpenney.net/multimarkdown/) or JSON header is prose to
  it); a workspace always parses with the default (on), so ramble gets no
  matching `DocLink` for a link inside such a header and resolves it
  locally.
