# mdroots

A zero-config markdown (and org) language server and Rust library. It finds
your notes, indexes them in the background, and understands the link styles
of [zk](https://github.com/zk-org/zk), [Obsidian](https://obsidian.md), [marksman](https://github.com/artempyanykh/marksman), [Foam](https://foambubble.github.io/foam/), [Dendron](https://www.dendron.so), [Logseq](https://logseq.com), [org-mode](https://orgmode.org) and plain
relative paths.

## Status

M1 is done: `mdroots-syntax` (parsing, link scan, frontmatter),
`mdroots-resolve` (resolution ladder), `mdroots-core` (`MemStore`) and
`tools/zkdiff`, which checks resolution against zk on real vaults
([results](docs/research/m1-differential.md)).

M2 is done: `mdroots-roots` finds the root of a file without listing
trees that are virtual, remote or too big: registry lookup, marker climb,
filesystem classification, [git](https://git-scm.com) index reading and budgeted walks, with an
in-memory registry ([spec](docs/specs/roots.md)).

M3 is done: the `mdroots` facade (`Workspace`: discover a root, index it
in memory, answer links, backlinks, tags and diagnostics) and the
`mdroots` CLI. Next is M4: the SQLite index in `mdroots-index`.

```sh
cargo run -p mdroots-cli -- check tests/corpus/zkvault    # path:line:col: severity: message
cargo run -p mdroots-cli -- roots tests/corpus/zk-min/broken.md
cargo run -p mdroots-cli -- resolve tests/corpus/zk-min/a.md '[[b]]'
cargo run -p mdroots-cli -- backlinks tests/corpus/zk-min/a.md
```

`check` exits 1 on any error or warning, so it works in CI. Commands and
output: [docs/specs/library.md §6](docs/specs/library.md#6-cli).

## Goals

- **No config:** roots are found automatically without walking trees that are too big or remote; existing tool configs are read, never written.
- **Fast first result:** the current buffer is ready in milliseconds; cross-file answers come from the persisted index and improve as background reconcile runs.
- **Nothing written into your trees:** the index is a rebuildable cache in the user cache dir.
- **Light:** < 35 MB private memory per process with 10 concurrent instances; background work at the lowest QoS class.
- **Liberal links:** wiki, markdown, org, reference links, bare paths; code is a mention, never a diagnostic.
- **Embeddable:** the LSP server is a thin adapter over the library.

## Crates

```
mdroots-syntax ← mdroots-resolve ← mdroots-core ← mdroots-index ← mdroots ← mdroots-lsp, mdroots-cli
                                         ↑                           ↑
                                   mdroots-roots ────────────────────┘
```

`mdroots-core` holds the `Store` and `FileSystem` traits, `MemStore` and
reconcile; `mdroots-index` adds the SQLite store and the per-root writer
lock. Embedders depend on `mdroots`; `mdroots-cli` is the `mdroots`
binary. `mdroots-index` and `mdroots-lsp` are not written yet, so the
facade runs over `MemStore` and always includes `mdroots-roots`. See [docs/specs/library.md](docs/specs/library.md).

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

For [Neovim](https://neovim.io) 0.12+, copy `editors/nvim/lsp/mdroots.lua` (and optionally `editors/nvim/plugin/mdroots.lua`)
into `~/.config/nvim/`. Details: [docs/specs/library.md](docs/specs/library.md).

## License

MIT
