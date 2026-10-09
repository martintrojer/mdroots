# mdroots-cli

The `mdroots` command: a zero-config markdown and org notes tool and
language server, built on the [`mdroots`](https://crates.io/crates/mdroots)
library.

```sh
cargo install mdroots-cli        # installs the `mdroots` binary

mdroots check notes/             # broken links etc.; exit 1 on errors or warnings (CI-friendly)
mdroots roots notes/garden.md    # which root was picked, and why
mdroots resolve notes/a.md '[[garden]]'
mdroots backlinks notes/garden.md
mdroots lsp                      # the language server, over stdio
```

No configuration or root markers are needed: mdroots finds the root of
each file itself and reads existing [zk](https://github.com/zk-org/zk),
[Obsidian](https://obsidian.md) and
[marksman](https://github.com/artempyanykh/marksman) configs. A
[Neovim](https://neovim.io) 0.12+ config is in
[`editors/nvim`](https://github.com/martintrojer/mdroots/tree/main/editors/nvim).

More: <https://github.com/martintrojer/mdroots>.
