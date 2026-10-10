# mdroots

[![CI](https://github.com/martintrojer/mdroots/actions/workflows/ci.yml/badge.svg)](https://github.com/martintrojer/mdroots/actions/workflows/ci.yml)

A zero-config markdown (and org) language server and Rust library. It finds
your notes, caches them in a per-root index, and understands the link styles
of [zk](https://github.com/zk-org/zk), [Obsidian](https://obsidian.md), [marksman](https://github.com/artempyanykh/marksman), [Foam](https://foambubble.github.io/foam/), [Dendron](https://www.dendron.so), [Logseq](https://logseq.com), [org-mode](https://orgmode.org) and plain
relative paths.

## Status

M1 is done: `mdroots-syntax` (parsing, link scan, frontmatter),
`mdroots-resolve` (resolution ladder), `mdroots-core` (`MemStore`) and
`tools/zkdiff`, which checks resolution against zk on real vaults
([results](docs/research/zk-differential.md)).

M2 is done: `mdroots-roots` finds the root of a file without listing
trees that are virtual, remote or too big: registry lookup, marker climb,
filesystem classification, [git](https://git-scm.com) index reading and budgeted walks, with an
in-memory registry ([spec](docs/specs/roots.md)).

M3 is done: the `mdroots` facade (`Workspace`: discover a root, index it
in memory, answer links, backlinks, tags and diagnostics) and the
`mdroots` CLI.

M4 is done: `mdroots-index` keeps a per-root [SQLite](https://sqlite.org)
DB in the user cache dir with each note's content and stat, so a later
process reads only changed files; one process per root (the flock holder)
writes it, the others read. Queries still run on an in-memory index
([D9](docs/DECISIONS.md)).

M5 is done: the language server, `mdroots lsp` (goto, references, hover,
symbols, completion, rename, diagnostics), with a [Neovim](https://neovim.io)
config smoke-tested against the real binary.

M6 is done: whole-root queries without quadratic cost (diagnostics for
3,000 notes in about 50 ms instead of 26 s), a native file watcher in
`mdroots lsp` (on local marker, VCS and loose roots only, never on virtual
or remote filesystems), full-text search over an
[FTS5](https://sqlite.org/fts5.html) table (`Workspace::full_text`,
`mdroots search`), cache GC, and a rebuild of a corrupt cache DB.

M7 is done: the editor features the Neovim plugin advertised (folding
by heading sections and frontmatter, "N backlinks" and "N links" code
lenses, and an extract-note code action that moves a selection into a new
note and links it in the root's style, read from an existing zk or
Obsidian config or voted from the notes), and a background open in
`mdroots lsp`: a file is served alone at once while its root is
discovered and indexed, with a progress notification for opens over a
second.

0.2.6 is on crates.io (`cargo install mdroots-cli`, `cargo add mdroots`).
Each release also has prebuilt `mdroots` binaries for macOS (Apple silicon)
and Linux (x86_64) on the
[releases page](https://github.com/martintrojer/mdroots/releases).
What is left (Linux CI, real-vault validation, known limitations, deferred
work and open questions): [docs/ROADMAP.md](docs/ROADMAP.md).

Editor features of `mdroots lsp`: goto definition (links, anchors, code
mentions), references and backlinks, hover previews, document and
workspace symbols, completion of notes, headings, paths and tags, rename
of a note with its links, diagnostics for broken links and anchors,
folding ranges, code lenses, document links, extract note, and live
updates from a file watcher.

```sh
cargo run -p mdroots-cli -- check tests/corpus/zkvault    # path:line:col: severity: message
cargo run -p mdroots-cli -- roots tests/corpus/zk-min/broken.md
cargo run -p mdroots-cli -- resolve tests/corpus/zk-min/a.md '[[b]]'
cargo run -p mdroots-cli -- backlinks tests/corpus/zk-min/a.md
cargo run -p mdroots-cli -- search note tests/corpus/zk-min/a.md
```

`check` exits 1 on any error or warning, so it works in CI. Commands and
output: [docs/specs/library.md §6](docs/specs/library.md#6-cli). Set
`MDROOTS_CACHE_DIR` to use another cache dir than the user's.

## Goals

- **No config:** roots are found automatically without walking trees that are too big or remote; existing tool configs are read, never written.
- **Fast first result:** the current buffer is ready in milliseconds; cross-file answers come from the persisted index, so unchanged notes are never re-read.
- **Nothing written into your trees:** the index is a rebuildable cache in the user cache dir.
- **Light:** < 35 MB private memory per process with 10 concurrent instances; background work at the lowest QoS class.
- **Liberal links:** wiki, markdown, org, reference links, bare paths; code is a mention, never a diagnostic.
- **Embeddable:** the LSP server is a thin adapter over the library.
- **A scanner, not a note manager:** mdroots reads and answers questions
  about notes; it doesn't create notes or fill templates (editor edits such
  as rename and extract-note are returned for the client to apply).

## Crates

```
mdroots-syntax ← mdroots-resolve ← mdroots-core ← mdroots-roots ← mdroots-index ← mdroots ← mdroots-lsp ← mdroots-cli
```

`mdroots-core` holds the `FileSystem` trait and `MemStore`, the in-memory
index; `mdroots-roots` finds roots; `mdroots-index` adds the cache dir,
the per-root writer lock, the per-root DB with its full-text table, the root registry and cache GC.
Embedders depend on `mdroots` ([ramble](https://github.com/martintrojer/ramble),
a TUI markdown reader, embeds the library); `mdroots-lsp` is the server as a library;
`mdroots-cli` is the `mdroots` binary, `mdroots lsp` included. See [docs/specs/library.md](docs/specs/library.md).

## Layout

```
crates/        library crates
tools/zkdiff/  differential test against zk's notebook.db
tests/corpus/  shared test vaults
editors/nvim/  Neovim 0.12+ config (lsp/ and plugin/)
bench/         lspbench.py, mdsurvey.py, mdresolve.py, nvim_smoke.lua
scripts/       check.sh
docs/          specs, decisions, research → start at docs/README.md
```

## Build and test

```sh
bash scripts/check.sh   # fmt, clippy -D warnings, tests
```

## Neovim

For Neovim 0.12+: put the `mdroots` binary on your `PATH`
(`cargo install --path crates/mdroots-cli`), copy
`editors/nvim/lsp/mdroots.lua` (and optionally
`editors/nvim/plugin/mdroots.lua`) into `~/.config/nvim/`, and open a
markdown file. No root markers or settings are needed. `:MdrootsInfo`
shows the root mdroots picked and why.

```sh
cargo build -p mdroots-cli
nvim --clean --headless -u NONE -c 'luafile bench/nvim_smoke.lua'   # smoke test, read-only on tests/corpus
```

Details: [docs/specs/library.md](docs/specs/library.md#neovim-012-example).

## License

MIT
