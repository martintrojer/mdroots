# mdroots

[![CI](https://github.com/martintrojer/mdroots/actions/workflows/ci.yml/badge.svg)](https://github.com/martintrojer/mdroots/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/mdroots.svg)](https://crates.io/crates/mdroots)

**A markdown and org language server that works the moment you open a note.**
No init step, no config, no root markers. mdroots finds the notebook a file
belongs to, indexes it in the background, and keeps the index in your user
cache dir, so the next editor that opens the notebook starts warm.

It understands the link styles your notes already use:
[zk](https://github.com/zk-org/zk), [Obsidian](https://obsidian.md),
[marksman](https://github.com/artempyanykh/marksman),
[Foam](https://foambubble.github.io/foam/), [Dendron](https://www.dendron.so),
[Logseq](https://logseq.com), [org-mode](https://orgmode.org) and plain
relative paths, in one notebook if you mix them.

## Why

- **Instant.** The note you opened answers in about 10 ms, whatever the
  notebook size; whole-notebook answers follow once the index is up.
- **Warm across editors.** Every editor on a notebook shares one cache; only
  changed notes are re-read.
- **Zero config, honest about it.** Roots and conventions are detected, and
  the settings you already made in zk or Obsidian are honoured.
  `mdroots roots NOTE` shows the root it picked and why.
- **Safe on big trees.** It never walks a remote, virtual or oversized tree;
  it falls back to the folders you actually open.
- **A library first.** The server, the CLI and embedders such as
  [ramble](https://github.com/martintrojer/ramble) (a TUI reader) share the
  same Rust API.

## Compared with zk and marksman

Time from server start, synthetic notebooks, median of 5 runs
([method, caveats and how to reproduce](docs/research/benchmark.md)):

| 10,000 notes | mdroots (warm cache) | mdroots (cold) | zk (indexed) | marksman |
|---|---|---|---|---|
| first answer (go to definition) | 9 ms | 28 ms | 210 ms | 4.9 s |
| full note completion | 404 ms | 2.3 s | 294 ms | 4.9 s |
| broken-link diagnostic | 380 ms | 2.3 s | 227 ms | 5.5 s |
| memory (phys_footprint) | 92 MB | 92 MB | 54 MB | 320 MB |
| setup | none | none | `zk index`: 112 s | none |

zk answers fastest once `zk index` has run, and uses less memory at this
size; mdroots needs no index step and answers the open note first. At 1,000
notes mdroots uses the least memory (17 MB vs 24 MB for zk and 105 MB for
marksman).

## What it is not

- **Not a note manager.** It reads notes and answers questions about them;
  it never creates notes or fills templates. Renames and extract-note come
  back as edits for your editor to apply.
- **Not a daemon.** Each editor runs it in-process; the first process on a
  notebook writes the cache, the others read it.
- **Not a tenant in your notes.** Nothing is written into your trees; tool
  configs are read, never required or written.

## Install

```sh
cargo install mdroots-cli     # the `mdroots` binary (server + CLI)
cargo add mdroots             # the library
```

Prebuilt binaries for macOS (Apple silicon) and Linux (x86_64) are on the
[releases page](https://github.com/martintrojer/mdroots/releases).

**Neovim 0.12+:** copy [`editors/nvim/lsp/mdroots.lua`](editors/nvim/lsp/mdroots.lua)
and [`plugin/mdroots.lua`](editors/nvim/plugin/mdroots.lua) into
`~/.config/nvim/` and open a markdown file. The plugin file calls
`vim.lsp.enable('mdroots')` and adds optional mappings; to skip it, call
`vim.lsp.enable('mdroots')` in your own config. `:MdrootsInfo` shows the root and
why ([details](docs/specs/library.md#neovim-012-example)).

## Use

```sh
mdroots check notes/                # broken links and anchors; exit 1 on problems (CI)
mdroots search 'rust async' notes/  # notes containing every word
mdroots backlinks notes/a.md        # who links here
mdroots roots notes/a.md            # the notebook root mdroots picked, and why
mdroots lsp                         # the language server on stdio
```

Editor features: go to definition (links, headings, `path:line` code
mentions), references and backlinks, hover previews, symbols, completion of
notes, headings, paths and tags, rename with link updates, broken-link
diagnostics, folding, code lenses, document links and extract-note.

## Learn more

- [How roots are found](docs/specs/roots.md) and what happens on huge or
  remote trees
- [The index, links and resolution](docs/specs/index.md)
- [The library API, the CLI and the server](docs/specs/library.md)
- [Design decisions](docs/DECISIONS.md) and [what's not built yet](docs/ROADMAP.md)
- [Changelog](CHANGELOG.md) and [the docs map](docs/README.md)

## Develop

```sh
bash scripts/check.sh   # fmt, clippy -D warnings, all tests
```

MIT licensed.
