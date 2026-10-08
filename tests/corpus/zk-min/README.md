# zk-min: a minimal zk notebook fixture

Ported from ramble (MIT, same author), `tests/fixtures/zk/` at commit
`d394783`. zk 0.15.6 was run against these files in ramble's tests, so zkdiff's
unit tests use them as known-good behaviour.

| File | Content | What it covers |
|---|---|---|
| `a.md`, `b.md` | `[Note B](b)`, `[Note A](a)` | zk's extensionless markdown links: `a` resolves to `a.md` |
| `broken.md` | `[nowhere](missing-note)` | the one broken link |
| `emoji.md` | `😀 日本 [Note B](b)` | multi-byte text before a link (UTF-8/16/32 columns; zk 0.15.6 counts UTF-32) |
| `tagged.md` | front matter `tags: [project]`, `[Note A](a)` | a frontmatter tag and a link below frontmatter |
| `.zk/config.toml` | `[note] filename = "{{id}}"` | the minimum zk needs to treat the dir as a notebook |
| `.marksman.toml` | comment only | marksman root marker |
