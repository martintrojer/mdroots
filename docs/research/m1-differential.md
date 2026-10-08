# M1 differential: mdroots link resolution vs zk's notebook.db

Plan: [specs/index.md](../specs/index.md) (differential testing). Two real
vaults: **vault A**, a ~730-note zk vault, and **vault B**, a ~210-note
research vault.

## Method

`tools/zkdiff` loads a vault twice:

- **mdroots:** `MemStore::open(StdFs, vault)`, every link of every note through the ladder without the Partial step (diagnostics mode).
- **zk:** every `links` row joined to its source and target paths, read from `.zk/notebook.db` opened read-only and immutable (`file:…?mode=ro&immutable=1`). zk itself is never run; the DB mtime and `git status` of both vaults were unchanged after the runs.

Only links zk could store are compared: markdown, reference, image,
autolink, bare URL, wiki and org-in-md, outside code, comments, frontmatter
and HTML. hrefs are normalised the way zk stores them:

| Kind | Normalisation |
|---|---|
| markdown destination | joined onto the source dir and lexically cleaned (Go `path.Join`) |
| URL with a host | as written |
| wiki | left side of `\|`; backslashes stripped on the mdroots side, because zk's wikilink parser drops them (`\[` → `[`, `\n` → `n`) |
| org-in-md | `target][desc` (goldmark's wikilink quirk); backslashes stripped as for wiki |
| all | percent-decoded, NFC |

Rows are matched per source file by href in occurrence order (`links.id`),
`snippet_start` breaking ties. Positions are not compared: zk stores no link
offsets, and its LSP references line is the first substring match of the
note stem, not the link position.

```sh
cargo run --release -p zkdiff -- <vault> --json out.json
```

The JSON holds run metadata (`meta`) and the orphan rows (`orphans`, `orphan_count`).

## Results

| | vault A | vault B |
|---|---|---|
| zk notes / link rows | 714 / 1,836 | 210 / 2,344 (+5 orphan rows) |
| mdroots files / compared links | 730 / 2,481 | 210 / 2,337 |
| **agree** | 1,790 | 2,190 |
| **mdroots-only** | 0 | 138 |
| **zk-only** | **0** | **0** |
| **disagree** (unindexed / ambiguous) | **0** | **0** |
| external-mismatch | 45 | 0 |
| unmatched-zk | 1 | 16 |
| unmatched-mdroots | 9 | 0 |
| not-in-zk (`.org` files) | 634 | 0 |
| image-not-in-zk | 3 | 9 |

- Vault A: all 1,030 zk-resolved links agree (all wikis: 780 RootRelative, 250 FileRelative).
- Vault B: all 1,991 comparable zk-resolved links agree (wikis 1,111 Stem, 606 FileRelative, 229 RootRelative; markdown 45 FileRelative). No answer was Ambiguous.
- Against the prototype measurement in the index spec: vault A's 1,030/1,033 wikis match exactly (the 3 misses are unresolved on both sides). Vault B's 1,956 = 1,946 compared + 10 wikis inside `<!-- -->` comments, which mdroots resolves (context Comment) but zk does not index. Step 1 runs first, so a link from a top-level file counts as FileRelative, not RootRelative.
- zk's unresolved internal links (56 / 138): in vault A mdroots agrees on 13 (Broken) and calls 43 External; in vault B all 138 are Unindexed.

## Categories

**mdroots-only (138, vault B).** Markdown links from `references/*.md` to existing non-note files (`cache/*.{html,txt,json}` 86, `papers/*.pdf` 52). zk resolves only to notes; mdroots reports Unindexed, so goto works and no diagnostic fires. Verdict: mdroots ("not indexed is not missing").

**external-mismatch (45, vault A).**
- 42 org-in-md `[[file:~/…]]` links: zk stores them as unresolved internal links; mdroots maps `~/`, finds a target outside the root and reports External. Verdict: mdroots; zk's are dead-link false positives.
- 1 `[t](file:///home/…)`: zk joins it onto the source dir as a path; mdroots reads the scheme, absolute path outside the root: External. Verdict: mdroots.
- 2 `file://images/2025/x.png`: zk treats `images` as a URL host; mdroots strips `file://`, finds the vault file, Unindexed. Strictly wrong, but it is what the author meant. Kept.

**unmatched-zk (1 + 16).**
- Vault A: zk indexes a symlinked note twice; mdroots's walk does not follow symlinks. Expected.
- Vault B, 13 rows: frontmatter `source-url:` values linkified by newer zk versions only (older-indexed notes with the same layout have no row). mdroots emits frontmatter URLs with context Frontmatter, which the comparison skips. Not an mdroots bug.
- Vault B, 3 rows: scheme-less `www.` hosts linkified by goldmark. mdroots's URL scan requires `http(s)://`; such a host never resolves to a note. Kept as a difference.

**unmatched-mdroots (9, vault A).**
- 7 empty destinations `[t]()`: zk stores nothing; mdroots reports Broken. Follow-up for diagnostics parity: empty links should probably be silent.
- 2 URLs inside org syntax that markdown does not parse (`[[https://…][` split over lines; a URL in `#+begin_src` in a `.md` file). mdroots's bare-URL scan finds them; zk's linkify does not. Harmless.

**not-in-zk (634, vault A).** zk indexes no `.org` files. 630 External (399 org links including `#+LINK` abbreviations, 231 bare URLs), 3 Unindexed images, 1 Broken `id:` link (no note has that `:ID:`).

**image-not-in-zk (3 / 9).** zk stores no markdown images (goldmark image nodes). All well-formed: vault A relative images (Unindexed, files exist), vault B remote images (External).

**orphan rows (5, vault B).** Rows whose source note was deleted; zk does not enable SQLite foreign keys, so `ON DELETE CASCADE` never ran. Counted and excluded.

## Fixes found by the run

Both have regressions in `crates/mdroots-syntax/tests/scan.rs`:

- Bare URLs in headings were not scanned (`# loop: https://…`); the URL tail became a bare-path candidate.
- A trailing `][` is trimmed from bare URLs.

## Known differences

| Difference | Who is right | Count |
|---|---|---|
| Non-note files on disk: Unindexed vs unresolved | mdroots | 138 (B) |
| `file:~/…`, `file:///…` outside the root: External vs dead | mdroots | 43 (A) |
| `file://images/…`: Unindexed vs external URL | mdroots (useful reading) | 2 (A) |
| Symlinked note indexed twice | mdroots (no symlink follow) | 1 (A) |
| Frontmatter `source-url` linkified by newer zk | zk version quirk | 13 (B) |
| Scheme-less `www.` hosts | zk links them, mdroots does not | 3 (B) |
| Empty destination `[t]()`: Broken vs nothing | diagnostics-parity follow-up | 7 (A) |
| URLs inside unparsed org syntax in md | harmless | 2 (A) |
| Markdown images: zk stores none | expected | 3 / 9 |
