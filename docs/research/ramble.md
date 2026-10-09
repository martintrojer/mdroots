# ramble: code ported into mdroots, and what ramble drops

ramble, a separate TUI markdown reader by the same author (~14k lines of
Rust, pulldown-cmark 0.13, no tokio), will embed mdroots and delete its own
LSP client. Both projects are MIT licensed with the same author, so code is
copied with the ramble commit recorded in a source comment or the commit
message. Main-branch files are identical at ramble `d394783` and `75b8285`;
frontmatter comes from an unmerged ramble branch at `5f6e7ed2b798`.

## Ported

| ramble code (commit) | What it does | mdroots home |
|---|---|---|
| `src/doc.rs` `slugify`, `unique_slug`, `is_mark` (`75b8285`) | GitHub slugs keeping combining marks, `{#id}` override, `-1`/`-2` dedup | `mdroots-syntax` slug (`tests/slug.rs`) |
| `src/doc.rs` `collect_links` + `link_text_range_is_drawn_text` proptest (`75b8285`) | link range plus visible `text_range`, empty-text `[](x)`, nesting | `mdroots-syntax` structure (`tests/structure.rs`) |
| `src/doc.rs` `collect_code_spans`, `code_content` (`75b8285`) | inline code outside links/images; text = CommonMark code text | `mdroots-syntax` scan (CodeMention candidates) |
| `src/doc.rs` `from_bytes`, `options()` (`75b8285`) | NUL in first 8 KiB = binary, lossy UTF-8; pulldown option set | `mdroots-syntax` `parse_bytes`, `markdown_options()` |
| `src/frontmatter.rs` + unit tests (`5f6e7ed2b798`) | YAML/TOML: line scan keeps key order, source text, duplicates; parser decides value shape only; never fails | `mdroots-syntax` frontmatter |
| `src/doc.rs` `empty_front_matter`, `mask` (`5f6e7ed2b798`) | empty `---`/`---` or `+++`/`+++` at offset 0 is frontmatter (pulldown-cmark 0.13 emits none); fences blanked, offsets kept | `mdroots-syntax` structure |
| `src/lsp/position.rs` + tables, proptest (`75b8285`) | byte offset ↔ LSP position for UTF-8/16/32, `\n`-only line split | `mdroots-syntax` `LineIndex` (`tests/line_index.rs`) |
| `src/app/codepath.rs` `strip_position`, `resolve`, `locate` + pure tests (`75b8285`) | inline code naming an existing file is a link; full text first, then `:LINE[:COL]` stripped; `~/` via injected home; ≤ 512 bytes; rejects whitespace and `://` | `mdroots-syntax` scan, `mdroots-resolve` ladder code-mention branch (`ResolveCtx.code_dirs`) |
| `src/nav.rs` `scheme`, `percent_decode`, `file:` forms + `resolve_table` (`d394783`) | schemes (≥ 2 chars, so `C:` is a path), lossy percent-decoding, `file:///`, `file://localhost/`, `file:/` | `mdroots-resolve` normalize (`tests/normalize.rs`) |
| `src/nav.rs` extensionless rule (`c2a138b7`) | `[x](a)` tries `a`, then `a.md` (zk default link style) | `mdroots-resolve` ladder |
| `src/lsp/uri.rs` `canonical_path` + symlink test (`75b8285`) | canonicalise the longest existing ancestor, append the missing tail | `mdroots-core` `FileSystem::canonicalize` |
| `tests/fixtures/zk/` (`75b8285`) | 5-note zk notebook checked live against zk 0.15.6 | `tests/corpus/zk-min/` |

## Still to port

Fetch from the ramble repo at the named commit.

| ramble code (commit) | Use | Status |
|---|---|---|
| `src/frontmatter.rs` nested list items from the parser (`065bdf7`) + `nested_list_items_take_the_parser_items` | `l:\n  - a\n  - - b\n    - c` is `["a", "b, c"]`; mdroots 0.2.0 gives `["a", "- b", "c"]` | ported in 0.2.1 (with `04f915b` and `d5ff6fb`) |
| `src/frontmatter.rs` block-scalar list items (`01e1ef6`) + `block_scalar_list_items_are_their_text` | `- \|` with indented text is that text; mdroots 0.2.0 gives the literal `\|` | ported in 0.2.1 |
| `src/app/codepath.rs` `code_path_dirs` (`75b8285`) | extra code-mention search dirs (page dir, VCS root, tree root, deduped) as an `Options` field | built in 0.2.1 as `Options::code_dirs` (the caller computes the dirs) |
| `src/app/watch.rs` + `tests/watch.rs` (`75b8285`) | lessons for the watcher: editors replace by rename, compare content not events, kick once after `watch()` so a write between load and watch is seen | built in M6 without porting (`crates/mdroots/src/watch.rs` reconciles by stat and content); the FSEvents replay lesson waits for replay ([ROADMAP](../ROADMAP.md) §3) |
| `src/lsp/uri.rs` `uri_to_path`, `canonical_uri` (`75b8285`) | URI ↔ path in `mdroots-lsp` | not ported: `mdroots-lsp` decodes `file:` URIs with lsp-types' `fluent-uri`; zk's undeclared UTF-32 columns matter only to ramble's optional zk backend |
| `tests/support/fake_lsp.rs`, `src/lsp/framing.rs` (`75b8285`) | a scripted test client | not needed: `mdroots-lsp` tests use `lsp_server::Connection::memory()` and the CLI tests drive the real binary |
| `src/notebook.rs` `walk_notes`, `src/app/launch.rs` `vcs_root` (`75b8285`) | the `ignore::WalkBuilder` flags and the VCS marker list | used as reference for the M2 walk and marker table |

All of ramble's front matter fixes up to `01e1ef6` are in mdroots 0.2.1:
block scalars are text on one line (`04f915b`), linear-time parsing
(`d5ff6fb`; 20k keys in about 20 ms), nested list items from the parser
(`065bdf7`) and block-scalar list items as text (`01e1ef6`).

## What ramble deletes once it embeds mdroots

Counts at ramble `75b8285` (unchanged at `cf0dcb8`). Steps are those of
ramble's plan (`docs/specs/2026-10-08-mdroots-migration.md` in the ramble
repo): S1 = syntax from mdroots, S2 = mdroots as the backend, S3 = mdroots
by default, S4 = the LSP client kept as an optional backend for third-party
servers (moved, not deleted).

| ramble code | Lines | Step | Replaced by |
|---|---|---|---|
| `src/lsp/mod.rs`, `framing.rs`, `position.rs`, `uri.rs` | 604 + 60 + 93 + 53 | S4 | moved behind the optional backend; the main path is in-process `mdroots::Workspaces` with byte offsets and paths end to end |
| `src/app/lsp_glue.rs` | 702 | S2, S4 | ~150-line `mdroots_glue.rs` (`document_links`, `goto`, `preview`, `refresh_paths`, backend label); the rest moves into the optional backend |
| `src/notebook.rs` | 276 | S2 | `notes`, `full_text`, `tags`, `backlinks` (notes by tag filtered from `notes()`); picker types (`Item`, `Op`, `link_items`, ~60) stay |
| `src/nav.rs` resolve body, `scheme`, `percent_decode`, `hex` | ~70 | S2 | `DocLink.target`/`anchor`/`status`, `goto`; anchor/URL dispatch stays |
| `src/app/codepath.rs` pure half + `code_path_dirs`, `add_code_path_links` | 101 + ~48 | S2 | `DocLink{kind: CodeMention, target, line}`, dirs from `Options` once ported; `follow_code_path` stays |
| `src/doc.rs` link and code-span collection | ~100 | S1 | `mdroots::syntax` links and code spans; `code_content` stays for rendering |
| `src/config.rs` LSP config + default `[[lsp.server]]` | ~76 | S3 | no config by default; `[[lsp.server]]` only for the optional backend |
| `src/app/picker.rs` LSP/zk paths, backlink landing | ~175 | S2 | mdroots calls on the worker thread; `Backlink.range` makes the "first link back" workaround obsolete |
| `follow.rs`, `mod.rs`, `run.rs` LSP glue | ~20 | S2 | an mdroots event on the existing channel |
| tests: `lsp.rs`, `lsp_zk_e2e.rs`, `marksman_e2e.rs`, `fake_lsp.rs`, `notebook.rs` | 806 + 322 + 126 + 147 + 239 | S4 | kept as tests of the optional backend; `notebook.rs` tests rewired to mdroots |
| tests: LSP and fake-LSP parts of `app.rs`, `codepath.rs`, `nav.rs`, `config.rs` | ~582 + 318 + 47 + 108 + 77 + 35 | S1–S2 | cases move to mdroots `scan`/`ladder`/`normalize` tests; app tests rewired to mdroots |
| frontmatter branch: `src/frontmatter.rs`, `doc.rs` detection, parser half of `tests/frontmatter.rs` (421) | 807 + ~60 | S1 | `Document::frontmatter()` once the two front matter fixes above are ported; the fold UI stays |
| deps | — | S1–S4 | `saphyr` drops in S1; `lsp-types`, `url` stay with the optional backend; `serde_json`, `ignore`, `toml` stay |

ramble's plan estimates about −3,000 lines on its main path against ~200
lines of mdroots glue, with about 1,000 lines of LSP client kept behind the
optional backend.

## Notes for ramble's migration

- Heading slugs are optional to drop: ramble needs them to jump to `#anchor` on the page it renders (keep its own or call `mdroots::syntax::slug::github`).
- `mask` (keeping empty-frontmatter fences out of the render pass) can use `Workspace::frontmatter_range` or `Document::frontmatter().range` instead of its own detection.
- The hover popup (`src/ui/hover.rs`) stays, fed by `preview()`; only the LSP markdown flattening goes.
- `serde_json` stays (`review.rs`), `ignore` stays (sidebar), `toml` stays (config).
- The embedder API ramble needs is in [specs/library.md](../specs/library.md) §3.2–§3.3. Built by 0.2.0: `document_links` (with `text_range`, `anchor`, `line`, `status`), `goto`, `preview`, unranked `notes`, `full_text` with snippet and line, backlinks with `from_title`, `line`, `in_code`, `refresh_paths` (instead of `touched`), `subscribe` (changed paths), `open_single` and `Workspaces::get` for a non-blocking first open. Still gaps ([ROADMAP](../ROADMAP.md) §3): `notes_with_tag`, `NoteSummary.modified`, `Preview.summary`, typed `subscribe` events, ramble's plan: `docs/specs/2026-10-08-mdroots-migration.md` in the ramble repo.
