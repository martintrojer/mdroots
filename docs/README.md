# mdroots docs

## Doc map

Each fact has one owner. Other files link to the owner instead of
repeating it.

| File | Owns |
|---|---|
| [../README.md](../README.md) | pitch, install, quick start |
| [../CHANGELOG.md](../CHANGELOG.md) | release history: user-visible changes per version |
| [README.md](README.md) | this map and the doc rules |
| [specs/roots.md](specs/roots.md) | root discovery, filesystem classification, walk budgets, modes, nested roots, cache file and lock names, process roles, fixtures |
| [specs/index.md](specs/index.md) | DB schema, reconcile, freshness, watcher, failures, GC, link model, resolution ladder, dialects, frontmatter, diagnostics, tests; a "Planned design" appendix |
| [specs/library.md](specs/library.md) | crates, public API, design rules, server behaviour, CLI, [Neovim](https://neovim.io) integration |
| [DECISIONS.md](DECISIONS.md) | decisions in force: the rule, why, what was rejected, at most one number per decision |
| [ROADMAP.md](ROADMAP.md) | everything not built or not validated, open questions, thresholds to validate |
| [research/gopls.md](research/gopls.md) | what makes [gopls](https://go.dev/gopls) fast, what mdroots adopts, why no daemon |
| [research/zk-differential.md](research/zk-differential.md) | mdroots link resolution vs [zk](https://github.com/zk-org/zk)'s `notebook.db` on two real vaults |
| [research/benchmark.md](research/benchmark.md) | language server timing and memory: mdroots vs zk and marksman on generated notebooks |
| [research/ramble.md](research/ramble.md) | code ported from ramble, how ramble embeds mdroots, the gaps it works around |
| [../editors/nvim/](../editors/nvim/) | the Neovim plugin and LSP config (setup: [library spec](specs/library.md#neovim-012-example)) |
| `crates/*/README.md` | each crate's page on [crates.io](https://crates.io/crates/mdroots) |

## Rules

- **Specs** are the source of truth. They describe current behaviour and
  change in the same commit as the code.
- **Current design only.** Specs, decisions, research and the roadmap say
  what is, not how it got there. History lives only in the CHANGELOG.
- **DECISIONS.md** holds the decisions in force, each with its reason and
  the rejected alternatives. It is edited in place when a decision changes.
- **Research** holds measurements and comparisons that justify the specs
  and decisions, tagged with the tool versions measured.
- **ROADMAP.md** entries are deleted once they become code plus a spec change
  or a decision.
- **Links.** An open-source tool is linked on its first mention in each
  file. Cross-file section references are anchor links, not bare `§N`.
- mdroots is a scanner: it reads notes and never creates them.
