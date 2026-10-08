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

The earlier in-repo snapshots are gone; fetch the code from the ramble repo
at the named commit when the milestone starts.

| Milestone | ramble code (`75b8285`) | Use |
|---|---|---|
| M3 | `src/app/codepath.rs` `code_path_dirs` | extra code-mention search dirs in `Options` (page dir, VCS root, tree root, deduped); dedup in `Options` or the caller |
| M5 | `src/lsp/uri.rs` `uri_to_path`, `canonical_uri` | URI ↔ path in `mdroots-lsp`; zk sends UTF-32 columns without advertising it |
| M5 | `tests/support/fake_lsp.rs`, `src/lsp/framing.rs` | script format (expect/reply/send/sleep/exit, JSONL log) and framing for a scripted test *client*; the server half does not carry over |
| M4 | `src/app/watch.rs` + `tests/watch.rs` | lessons, not code: FSEvents replays pre-watch writes, editors replace by rename, compare content hashes not events, kick once after `watch()` so a write between load and watch is seen |
| M2 | `src/notebook.rs` `walk_notes`, `src/app/launch.rs` `vcs_root` | reference only: the `ignore::WalkBuilder` flags and the VCS marker list |

## What ramble deletes once it embeds mdroots

Counts at ramble `75b8285`. Phases from ramble's migration plan: 1 = mdroots
behind a flag, 2 = mdroots default, 3 = link and frontmatter data from
mdroots, 4 = LSP client deleted.

| ramble code | Lines | Phase | Replaced by |
|---|---|---|---|
| `src/lsp/mod.rs`, `framing.rs`, `position.rs`, `uri.rs` | 604 + 60 + 93 + 53 | 4 | in-process `mdroots::Workspaces`; byte offsets and paths end to end |
| `src/app/lsp_glue.rs` | 702 | 4 | ~150-line `mdroots_glue.rs` (`document_links`, `preview`, `subscribe`, backend label) |
| `src/notebook.rs` | 276 | 4 | `notes`, `full_text`, `tags`, `notes_with_tag`, `backlinks`; picker types (`Item`, `Op`, `link_items`, ~60) stay |
| `src/nav.rs` resolve body, `scheme`, `percent_decode`, `hex` | ~70 | 3 | `DocLink.target`/`anchor`/`status`; anchor/URL dispatch stays |
| `src/app/codepath.rs` pure half + `code_path_dirs`, `add_code_path_links` | 101 + ~48 | 3 | `DocLink{kind: CodeMention, target, line}`, dirs from `Options`; `follow_code_path` stays |
| `src/doc.rs` link and code-span collection | ~100 | 3 | `document_links`; `code_content` stays for rendering |
| `src/config.rs` LSP config + default `[[lsp.server]]` | ~76 | 4 | one optional `[mdroots]` table |
| `src/app/picker.rs` LSP/zk paths, backlink landing | ~175 | 4 | synchronous mdroots calls; `Backlink.range` makes the "first link back" workaround obsolete |
| `follow.rs`, `mod.rs`, `run.rs` LSP glue | ~20 | 4 | `AppEvent::Mdroots(Event)` |
| tests: `lsp.rs`, `lsp_zk_e2e.rs`, `marksman_e2e.rs`, `fake_lsp.rs`, `notebook.rs` | 806 + 322 + 126 + 147 + 239 | 4 | position/URI cases already in mdroots; the rest go |
| tests: LSP and fake-LSP parts of `app.rs`, `codepath.rs`, `nav.rs`, `config.rs` | ~582 + 318 + 47 + 108 + 77 + 35 | 3–4 | cases move to mdroots `scan`/`ladder`/`normalize` tests; app tests rewired to mdroots |
| frontmatter branch: `src/frontmatter.rs`, `doc.rs` detection, parser half of `tests/frontmatter.rs` (421) | 807 + ~60 | 3 | `Document::frontmatter()`; the fold UI stays |
| deps | — | 3–4 | `lsp-types`, `url` drop; `saphyr` drops in 3; `serde_json`, `ignore`, `toml` stay |

Totals on main: about **2,380 lines of `src/`** (1,788 whole files + ~590
partial) and **2,810 lines of tests** go, against ~150 lines of glue and the
`[mdroots]` config. The frontmatter branch adds ~870 `src/` lines more.

## Corrections for ramble's migration plan

- "About 1.8–2k lines removed" counts whole `src/` files only; it misses `picker.rs`, `doc.rs` link collection, the partial removals and ~2,810 test lines.
- Heading slugs are optional to drop: ramble needs them to jump to `#anchor` on the page it renders (keep its own or call `mdroots::syntax::slug::github`).
- `mask` (keeping empty-frontmatter fences out of the render pass) stays until mdroots exposes the frontmatter region to renderers.
- The hover popup (`src/ui/hover.rs`) stays, fed by `preview()`; only the LSP markdown flattening goes.
- `serde_json` stays (`review.rs`), `ignore` stays (sidebar), `toml` stays (config).
- ramble's `cli.rs` `tree_root` lacks `.hg` while `launch.rs` `vcs_root` has it; fix before both become one call.
- The embedder API ramble needs is in [specs/library.md](../specs/library.md): `document_links` with `text_range` and `status`, `preview`, unranked `notes`, `notes_with_tag`, `full_text` with snippet and line, backlinks with `from_title`, `line`, `in_code`, and `touched`.
