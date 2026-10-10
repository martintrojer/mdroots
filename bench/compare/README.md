# Language server comparison: mdroots, zk, marksman

Scripts that time three markdown language servers on the same generated
notebooks: mdroots, [zk](https://github.com/zk-org/zk) and
[marksman](https://github.com/artempyanykh/marksman). The recorded results
and their caveats are in
[docs/research/benchmark.md](../../docs/research/benchmark.md).

Requirements: Python 3 (standard library only), `zk` and `marksman` on
`PATH`, and an mdroots release build. macOS adds `phys_footprint` figures
(via `footprint`); elsewhere those cells are empty.

## Run

From the repo root:

```
cargo build --release -p mdroots-cli && python3 bench/compare/run.py --mdroots target/release/mdroots
```

That generates 1,000- and 10,000-note notebooks in a fresh temp dir, runs
5 interleaved rounds per size and prints the tables. The 10k size takes
about an hour, mostly zk indexing. A quick smoke run:

```
python3 bench/compare/run.py --mdroots target/release/mdroots --runs 1 200
```

Options: `--work DIR` (an empty dir, or one run.py made before, reused with its notebooks), `--runs N`,
`--timeout S`, `--init-timeout S`; sizes are note counts (`200`, `1k`,
`10k`). Re-print the tables with
`python3 bench/compare/report.py WORK/results.jsonl`.

## Files

| File | Role |
|---|---|
| `gen.py N DIR` | deterministic notebook: the generator of `crates/mdroots/examples/bench_root.rs` (same LCG, seed 42), plus headings, inline tags, `.zk/config.toml` and `.marksman.toml`; `note-00000.md` is the probe page |
| `run.py` | the driver: isolation, notebooks, rounds, `results.jsonl` |
| `report.py` | median [min–max] markdown tables from `results.jsonl` |
| `../lspbench.py` | the stdio LSP client that times one server process |

## Isolation

`run.py` sets `HOME`, `XDG_{CONFIG,DATA,CACHE,STATE}_HOME`,
`DOTNET_CLI_HOME`, `TMPDIR` and each `MDROOTS_CACHE_DIR` to dirs inside the
work dir, and unsets `ZK_NOTEBOOK_DIR`. It only points servers at notebooks
it generated inside the work dir and refuses any other path. zk writes its
`.zk/notebook.db` into the generated notebook. marksman (a .NET program)
may look up its user config under the platform's application-support dir,
which .NET resolves without `$HOME`; in the recorded run it read nothing
and wrote nothing there.
