# AGENTS.md

## Gates

- **Fast** (while iterating): `cargo nextest run -p <crate>`, or `cargo nextest run -E 'binary(<test file>)'` for one integration test file. Whole workspace: `cargo nextest run --workspace` (~10 s warm).
- **Slow** (before closing work, and once per merged batch): `bash scripts/check.sh` — fmt, clippy `-D warnings`, `cargo test --workspace`, `cargo doc` `-D warnings`. ~25–40 s warm on macOS. CI runs it on Linux and macOS, plus check and test on MSRV 1.89.
- **Targeted slow**: `cargo nextest run -p mdroots-roots --test fixtures` (real-filesystem discovery fixtures, spec `docs/specs/roots.md` §7) and `cargo nextest run -p mdroots-cli --test many_process`.
- **Budget**: slow gate 5 min. Over budget is a trigger for test upkeep, not a stop.

Record each gate run in the task note (`GATE: commit tree command scope`, `RESULT: exit duration counts skips`, `CONDITIONS: host, git, load, cache`).

Test notes:
- `FS_LOCK` in `crates/mdroots-roots/tests/fixtures.rs` stays: it guards the real-time 1 s check on each discover call. Under `cargo test` it serialises the fixtures binary; under nextest it is a no-op.
- The slowest tests write thousands of files (`fixture_03`, `fixture_02`; their sizes come from the spec). File creation, not discovery, sets their time.
- Proptest's RNG crates build at opt-level 1 (`Cargo.toml`); workspace crates keep the plain dev profile.

Fast git: some tests spawn git. If the `git` on PATH is a slow wrapper, put a faster git binary first on PATH for local runs: `d=$(mktemp -d); ln -s <fast git> "$d/git"; PATH="$d:$PATH" <gate>`.
