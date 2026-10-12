# mdroots-cli

The `mdroots` command: a zero-config markdown and org notes tool and
language server, built on the [`mdroots`](https://crates.io/crates/mdroots)
library.

```sh
cargo install mdroots-cli        # installs the `mdroots` binary
mise use -g github:martintrojer/mdroots   # or a prebuilt binary (macOS arm64, Linux x86_64)

mdroots check notes/             # broken links etc.; exit 1 on errors or warnings (CI-friendly)
mdroots roots notes/garden.md    # which root was picked, and why
mdroots resolve notes/a.md '[[garden]]'
mdroots backlinks notes/garden.md
mdroots lsp                      # the language server, over stdio
```

`mdroots notes` takes `zk list`'s filters (`-t`, `-m`, `-l`, `-L`,
`--orphan`, `--related`, dates, sort, formats). Where it differs from zk:

- `-m QUERY` matches every word of QUERY, the last also as a prefix;
  there is no stemming, no phrases, no OR/NOT and no field filters. A
  repeated `-m` must match too.
- The NOTE of `-l`, `-L` and `--related` is the path of an indexed note,
  as given: no extension guessing or partial paths. Anything else exits 2.
- Links include a bare path in prose to an existing note (`see b.md`),
  which zk does not count. A bare path to a missing file is plain text,
  not a broken link.
- `mdroots backlinks NOTE` lists the other notes linking to NOTE, not
  NOTE's links to itself.

`mdroots check` prints diagnostics on stdout and a summary line on stderr
(`--quiet` drops it).

No configuration or root markers are needed: mdroots finds the root of
each file itself and reads existing [zk](https://github.com/zk-org/zk),
[Obsidian](https://obsidian.md) and
[marksman](https://github.com/artempyanykh/marksman) configs. A
[Neovim](https://neovim.io) 0.12+ config is in
[`editors/nvim`](https://github.com/martintrojer/mdroots/tree/main/editors/nvim).

More: <https://github.com/martintrojer/mdroots>.
