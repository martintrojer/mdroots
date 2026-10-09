# mdroots docs

## Layout

| File | What |
|---|---|
| [../README.md](../README.md) | what mdroots is, status, CLI example, crates, build |
| [DECISIONS.md](DECISIONS.md) | current decisions D1–D9: the rule, why, what was rejected |
| [ROADMAP.md](ROADMAP.md) | what is left: unvalidated parts, known limitations, deferred work, open questions |
| [specs/roots.md](specs/roots.md) | root discovery, filesystem classification, walk budgets, lazy mode, nested roots, many processes per root; what `mdroots-roots` implements |
| [specs/index.md](specs/index.md) | incremental index, link model, resolution ladder, dialects, frontmatter, differential testing |
| [specs/library.md](specs/library.md) | crates and features, public API, scheduling, CLI, [Neovim](https://neovim.io) integration |
| [research/gopls.md](research/gopls.md) | what makes gopls fast, what mdroots adopts, why no daemon |
| [research/m1-differential.md](research/m1-differential.md) | mdroots link resolution vs [zk](https://github.com/zk-org/zk)'s `notebook.db` on two real vaults |
| [research/ramble.md](research/ramble.md) | code ported from ramble, what is left to port, what ramble deletes |

## How the docs work

- **Specs** are the source of truth. They describe current behaviour and
  change in the same commit as the code.
- **DECISIONS.md** holds the decisions in force, each with its reason and
  the rejected alternatives. It is edited in place when a decision changes.
- **Research** holds measurements and comparisons that justify the specs
  and decisions.
- **ROADMAP.md** entries are deleted once they become code plus a spec change
  or a decision.
