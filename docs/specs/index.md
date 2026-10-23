# Spec: index under churn, liberal links, dialects, frontmatter

Related: [roots](roots.md) (discovery, nesting, flock roles), [library](library.md) (crates, API), [DECISIONS](../DECISIONS.md) (D3, D4, D5, D7, D8, D9), [differential results](../research/zk-differential.md), measurement scripts in [`bench/`](../../bench/).

Goal: the user never thinks about the index. No init or reindex command; a
`kill -9` loses at most one uncommitted batch (§1.1); ten editors starting
at once answer within milliseconds from the cache. The index syncs on open,
on `refresh` and via the opt-in watcher (§1.3). Unbuilt mechanics:
[Planned design](#planned-design).

## 0. Testbed

Two private [zk](https://github.com/zk-org/zk) notebooks in local APFS [git](https://git-scm.com) repos: **vault A** (~730 notes, also
`.obsidian/`) and **vault B** (~210-note research vault). Not in the repo;
`tests/corpus` holds scrubbed shapes of them. mdroots only reads them
(state in the cache dir, D5; rename/edit tests use a copy). Counts skip
hidden and editor temp files.

| | vault A | vault B |
|---|---|---|
| md / org files | 713 / 17 | 210 / 0 |
| full walk + parse (no DB) | 104–129 ms | 12–36 ms |
| duplicate stems | 39 (weekly notes) | 0 |
| wiki links | 1,041 in `.md`, root-relative; 197 in `.org`, 194 of them `#+LINK` abbreviations defined in 11 files | 1,973: 1,121 stem, 570 file-relative, 265 root-relative |
| piped `[[a\|b]]` | 1 | 151 |
| md links to local files | 8 outside code, all missing | 183: 139 tracked, 44 to gitignored files that exist |
| org `[[t][d]]` / `[[file:…]]` | 376 / 96 (96 point outside the root) | 0 |
| link-like text in code (fenced / inline) | 0 / 18 | 20 lines / 91 |
| bare path-like tokens / exist on disk | 2,423 / 2 | 471 / 192 (mostly frontmatter values) |
| zk unresolved internal / share resolved | 56 / 94.7% | 138 / 93.5% |

| Baseline (`initialize` → first result) | [marksman](https://github.com/artempyanykh/marksman) | zk lsp (prebuilt `notebook.db`) |
|---|---|---|
| vault A / vault B | 750–821 / 636–694 ms | 34–37 / 26–27 ms |
| RSS | 139–143 MB | 33–34 MB |
| doc/workspace symbols | yes | not implemented |

[pulldown-cmark](https://github.com/pulldown-cmark/pulldown-cmark) parses at ~1.1 GB/s, but opening a file costs ~100 µs on macOS (20k files ≈ 2 s), so warm start needs the DB.

So: resolution must try several strategies (two vaults by one author
differ); `#+LINK` abbreviations are external; not indexed ≠ missing, so
dead-link checks `stat`; bare paths count only if they exist; code is not
prose.

**Targets** (both vaults): cold (no DB) all features < 250 ms with
synchronous indexing (§1.5); warm < 30 ms to first result; private memory
(`phys_footprint`) < 35 MB per process at N=10 concurrent instances;
resolved links ≥ zk; zero diagnostics on links in code or on bare-path
candidates. Not RSS: mmap'd [SQLite](https://sqlite.org) pages count in
every process's RSS.

## 1. Index under churn

One writer per root (the flock reconciler), readers never write, no daemon (D3). The DB is a disposable cache (D4) of each note's bytes and stat; every process answers queries from an in-memory index hydrated from it (D9). Crate: `mdroots-index`, driven by the facade (`crates/mdroots/src/indexing.rs`).

### 1.1 Rules
1. **The DB is a cache.** The open buffer and a `stat` are authoritative. The DB may vanish at any time, including when the OS purges the cache dir.
2. **Small committed transactions**: ≤ 200 files or ≤ 50 ms per batch. No "index complete" state; a cancelled or killed reconcile leaves only whole batches.
3. **Single writer.** Only the reconciler writes. Peers serve, in memory, the DB's content, files they re-read and their own unsaved buffers.
4. **Every write transaction is `BEGIN IMMEDIATE`** with `busy_timeout` 2 s: a deferred read-then-write transaction gets `SQLITE_BUSY` at once, ignoring `busy_timeout`. WAL mode, `synchronous=NORMAL`.
5. **Unchanged files are never opened.** Their bytes come from the DB.

### 1.2 Schema

`SCHEMA = 2` (`crates/mdroots-index/src/db.rs`), created with `CREATE … IF NOT EXISTS` in one `BEGIN IMMEDIATE` by the first process to open the DB:
```sql
files(id INTEGER PRIMARY KEY AUTOINCREMENT,   -- ids never reused
      path TEXT UNIQUE NOT NULL,              -- root-relative, '/'-separated
      ino, ctime_ns, mtime_ns, size,          -- the stat the content was read with
      content BLOB NOT NULL,                  -- the file's bytes
      indexed_at)                             -- ms since the epoch
change_log(seq INTEGER PRIMARY KEY AUTOINCREMENT, path, kind)   -- add | mod | del
meta(key PRIMARY KEY, value)                  -- schema, generation (16 random hex digits, set at creation)
fts USING fts5(path UNINDEXED, body,          -- rowid = files.id; body = the content as lossy UTF-8
    tokenize = 'unicode61 remove_diacritics 2')
```
A different `meta.schema` is reported corrupt. Every write transaction trims `change_log` to its newest 10,000 entries. `SCHEMA` versions only the per-root DB (`roots/<id>.v2.db`); the registry has its own version (`roots.v1.db`).

**Full-text table.** The [FTS5](https://sqlite.org/fts5.html) table `fts`
is written in the same transaction as `files`: an upsert deletes and
re-inserts the row, a remove deletes it, a stat update leaves it.
`IndexDb::search(query, limit)` takes plain text, never FTS5 syntax: split
on whitespace, drop terms without a letter or digit, quote each term (so
`AND`, `NEAR(`, `-x` or `"` match as text), AND them, last term also a
prefix. Ranked by `bm25`, then path, each with an FTS5 `snippet()` of ~12
tokens. Only the reconciler writes it; readers:
[library §3.2](library.md#32-a-workspace-mdroots).

**File version** = `fstat` of the fd the content was read from (`stat`,
then `read` returning the fd's stat). If `(ino, ctime_ns, size)` differs
between the two, retry up to 3 times, then keep the last read. Keyed on
ctime, not mtime: `utimes` can set mtime but not ctime (`cp -p`,
`rsync -a`, `tar x`). mtime is stored, never compared.

**Change detection** (`reconcile`, `crates/mdroots-index/src/reconcile.rs`), per path, first match:

| Case | Action |
|---|---|
| invalid root-relative path, not in the file set, missing, or not a regular file | drop the row (`Remove`, logs `del`) |
| row's `(ino, ctime_ns, size)` equals the stat, path not forced | reuse the row's bytes; no read, no write |
| dataless (cloud placeholder), not forced | keep the row; reading would download the file |
| otherwise | read: new bytes → `Upsert` (logs `add` or `mod`); same bytes, new stat (`touch`) → `Stat` (stat columns only, no log entry) |
| stat or read error other than not-found | keep the row; no content for that path |

Content is compared directly, not hashed (the bytes are stored anyway). No
`parser_ver`: every process parses the stored bytes, so a parser upgrade
needs no re-index. No derived tables yet ([appendix](#derived-tables)).

### 1.3 Finding what changed

Indexed set: `.md`, `.markdown` and `.org` files from the root's listing,
minus dot-prefixed names and editor temp files
([roots §1 stage 4](roots.md#stage-4-budgeted-walk)). Other files can still
be link targets via `stat` (§2.3).

Three sync points, each synchronous in the calling thread:

- **`Workspace::open_for`.** The reconciler uses discovery's file list; on
  a registry hit it re-lists with `list_root` for the root's mode: budgeted
  walk (marker, VCS, loose), git index scan (index-driven, tracked-only),
  the enumerator (vcs-enumerated), or for a lazy root the working set of
  the opened file's directory
  ([roots §3](roots.md#3-lazy-and-vcs-enumerated-modes)). The opened file
  is always read when it has no row. This catches changes made while no
  mdroots process ran.
- **`Workspace::refresh(&cancel)`.** A peer first tries to become the
  reconciler. The reconciler re-lists as above and writes changes; a peer
  re-stats the DB's file set and reads changed files into memory only.
  Overlays survive. When `mdroots lsp` calls it:
  [library §3.6](library.md#36-embedding-the-server).
- **`Workspace::refresh_paths(&paths, &cancel)`**, a point refresh (the
  native watcher's): a note path is re-read, added or dropped; an existing
  directory adds its notes (hidden and pruned dirs skipped) and re-checks
  indexed ones; a gone path drops every indexed note at or under it. The
  reconciler writes; the store is patched in place
  (`MemStore::apply_contents`), not rebuilt.

| Who | File set reconciled | Writes |
|---|---|---|
| reconciler, non-lazy root | listing plus opened file; unlisted rows removed. Unlistable root (over budget, aborted): the DB's file set | yes |
| reconciler, lazy root | DB's file set plus new working-set files; rows outside the working set kept | yes |
| peer | the DB's file set (never lists); lazy root: plus the working set | no |
| peer of a still-empty DB (another process is indexing) | what discovery listed, or the working set | no |
| anyone, `refresh_paths` | only the given paths (and notes under a given directory) | reconciler only |

After `open_for` and `refresh` the process builds a new `MemStore` from the
returned bytes (`MemStore::from_contents`) and swaps it in, holding the
index lock until the swap, so point and full refreshes never interleave.

**Native watcher** (`crates/mdroots/src/watch.rs`). Opt-in via
`Options::watch(true)`: off for libraries, on in `mdroots lsp`, never in
one-shot CLI commands. A workspace watches only if:

- it is the reconciler (a peer promoted on `refresh` starts watching then);
- it has a DB (not memory mode);
- the mode is marker, VCS or loose (tracked-only roots can be a home
  directory, vcs-enumerated roots never walk; lazy and single-file roots
  are never watched);
- its filesystem classifies as local, never virtual, remote or cloud (so
  never [EdenFS](https://github.com/facebook/sapling), whose roots open
  lazy).

[notify](https://crates.io/crates/notify) 8 (FSEvents on macOS, inotify on
Linux) watches the root recursively. Thread `mdroots-watch` debounces
(200 ms quiet, at most 1 s after the first event), keeps notes, directories
and gone paths whose root-relative components are neither hidden nor in
the walk's prune list, and calls `refresh_paths`. Access events (its own
reads) are ignored. An event on the root itself, a lost-events rescan
flag, a notify error or a failed point refresh triggers a full `refresh`.
The thread holds a weak reference, ending when the last workspace clone
drops. A root notify cannot watch is not watched.

`Workspace::subscribe()` reports the paths each watcher-driven refresh
changed (every indexed note after a full refresh;
[library §3.2](library.md#32-a-workspace-mdroots)). `mdroots lsp`
re-publishes open documents from it and ignores `didChangeWatchedFiles`
for a watching workspace, so the disk has one owner. Measured: a note
changed by another program republishes in ~226 ms, no save. FSEvents
replay and other change sources: [planned](#change-sources).


### 1.4 Reads and peer freshness
- Queries read the process's `MemStore` (one `RwLock`), overlays over disk
  content; only the reconciler's `full_text` reads the DB (FTS,
  [library](library.md) §3.2). A peer sees the reconciler's writes on its
  next `refresh`. No reader follows `change_log`.
- Indexed lookups, a hot cache and incremental diagnostics:
  [planned](#reads-with-derived-tables).

### 1.5 Short-lived instances
Common: `nvim file.md` then `:q` after 2 s, CI, commit-message editors.
- No background thread unless `Options::watch` (only `mdroots lsp` sets
  it): reconcile happens inside `open_for`, nothing keeps running.
  `nvim +wq` costs one open.
- **No DB:** the first flock holder creates it and writes every note in
  committed batches while opening; a peer that finds it empty indexes in
  memory without writing.
- **Existing DB:** read the rows, re-stat every file, read only changed
  ones. Synthetic 3,000-note notebook (`mdroots check` on one note, schema
  2 with FTS, release build, files in OS cache): 0.6–0.7 s with a fresh
  cache, 0.11 s with the DB. An 11-note vault: 0.32 s cold, < 0.01 s warm.
- The kernel drops a dead reconciler's flock; committed batches persist (WAL).
- `mdroots lsp` opens roots on a background thread and serves the file
  alone meanwhile ([library §3.6](library.md#36-embedding-the-server)).
- Nothing runs VACUUM or optimize; no process blocks on another. Sweeps
  and an exit checkpoint: [planned](#background-work).

### 1.6 Failures and races
| Case | Outcome |
|---|---|
| Two processes find no DB | the flock holder creates it (`BEGIN IMMEDIATE` + `CREATE … IF NOT EXISTS`) and writes it; peers index in memory meanwhile |
| Ten start at once | discovery serialised under `discover.lock`; one registry row; one reconciler, the rest peers ([roots §7](roots.md#7-fixtures), fixtures 12, 13) |
| Peer wants freshness, no flock holder | `refresh` tries `LOCK_EX\|LOCK_NB` and becomes the reconciler |
| File restored with older mtime | new inode or newer ctime → re-read; new bytes → upsert (fixture 17) |
| Reconciler killed or cancelled mid-batch | the open transaction rolls back; committed batches stay; the next reconciler re-stats every file |
| Reconciler stopped (SIGSTOP) | peers still open and answer from the DB without waiting (fixture 18). Stale-reconciler warning: [planned](#background-work) |
| `SQLITE_BUSY` | `busy_timeout` 2 s on every connection |
| Cache dir deleted under a running process | the next process recreates it as reconciler; the old one serves from memory (fixture 16) |
| Different schema | separate files by name (`<id>.v1.db`, `<id>.v2.db`); a file with a different `meta.schema` is `Corrupt`. GC deletes old-schema files (§1.7) |
| DB corruption | the reconciler runs `PRAGMA quick_check` on opening the DB or taking it over on promotion. On failure, or `Corrupt` from opening or mid-sync, it builds a new generation `<id>.v2-<gen8>.db` (`gen8` = first 8 hex digits of its new `meta.generation`), fills it, and writes it to the registry's `db_file` only after that first sync succeeds. The corrupt file is never renamed or unlinked while open; GC deletes it later. A peer opening a corrupt file serves from an empty in-memory stand-in (like a peer of an empty DB); `open_for` never fails. On `refresh` and `refresh_paths` every process re-reads the registry row and reopens if it names another file ([roots §7](roots.md#7-fixtures), fixture 15) |
| GC while peers run | GC deletes a root's DB files only while holding `LOCK_EX\|LOCK_NB` on its `.open`; a process with the root open holds `LOCK_SH`, so the root waits for the next run (fixture 14) |

### 1.7 Files and connections
Location: `mdroots_index::cache_dir` picks the first local, writable
candidate of the D5 chain (each classified on its nearest existing
ancestor before anything is created; empty env vars count as unset), else
memory. The dir is created or reset to mode `0700`: it holds copies of the
user's notes.
`Options::cache_dir(path)` replaces the chain (the CLI sets it from
`MDROOTS_CACHE_DIR` for every command, `lsp` included);
`Options::index(IndexMode::Memory)` never touches the cache dir; an
`Options` with explicit `fs`/`probe` (in-memory test trees) and no cache
dir stays in memory. File and lock names, one lock scope per root and
schema: [roots §5](roots.md#files).

**GC** (`mdroots_index::gc`, `crates/mdroots-index/src/gc.rs`): at most
daily per cache dir, run by the process that sets `meta.gc_at` in
`roots.v1.db` in a `BEGIN IMMEDIATE` transaction. After a successful open
over the real filesystem, `open_for` checks if it is due (one registry
read) and runs it on the same thread. Candidates, each deleted only under
`LOCK_EX|LOCK_NB` on its root's `.open` (else skipped until the next run):

1. DB files of this schema not named by the root's `db_file` (old
   generations), or of a root with no registry row;
2. DB files of another schema whose newest file is ≥ 7 days old (mtime);
3. roots not seen for 30 days, or whose path is gone: DB files and
   registry row;
4. while the remaining DB files exceed 1 GiB: whole roots, least recently
   seen first, registry row included.

GC deletes only `.db`, `-wal` and `-shm` files directly in `roots/`, never
follows symlinks, and never deletes lock files (tiny, and unlinking one a
process waits on would let a second process take it).
Every process stamps `last_seen_ms` on open and refresh, at most hourly per
root.

Each workspace with a DB holds one mutex-guarded connection, used by open,
`refresh`, `refresh_paths` and the reconciler's `full_text`; other queries
never touch it. Only the reconciler writes through it.

## 2. Liberal link model (D7)

### 2.1 Structure first, then a liberal scan
1. **pulldown-cmark 0.13** (wikilinks, footnotes, tables, tasklists, YAML/TOML metadata, math), offset iterator, classifies every byte range: prose, heading, link, image, code (fenced/indented/inline), HTML, comment, math, frontmatter.
2. A hand-written single-pass **scanner** (no regex backtracking) over text ranges finds what CommonMark doesn't model, and re-reads pulldown's wikilinks: `[[t][d]]` becomes an org link to `t` (pulldown sees a wikilink to `t][d`); a wikilink spanning lines becomes text.

| Candidate | Examples | Notes |
|---|---|---|
| wiki | `[[t]]`, `[[t#h]]`, `[[t#^block]]`, `[[a\|b]]`, `![[embed]]` | scanner adds `#^block` |
| org | `[[t][d]]`, `[[file:x.org::*H]]`, `[[id:UUID]]`, `[[https://…][d]]`, `[[T:123][d]]` | in `.org` and `.md` |
| md | `[t](rel/path.md#h)`, `[t](rel/path)`, `[t](/site/rooted)`, `[t][ref]`, `<rel/path.md>` | `.md` suffix optional |
| bare path | `notes/foo`, `./foo.md`, `../img/x.png`, `~/vault/x.md` | only if it resolves (§2.3) |
| URL | `https://…`, `<https://…>`, `file:///…` | external |
| tag | `#tag`, `#a/b`, `#multi word#`, `:colon:tags:`, org heading `:tag:` | syntaxes enabled by vote |
| footnote | `[^1]` | in-doc goto/refs |
| templating | `{{< ref "x.md" >}}`, `{% link x.md %}` | resolve the inner path |

**Org `#+LINK`**: `#+LINK: T https://…/?t=%s` lines are collected per file; `[[T:123]]` expands (`%s` replaced, else appended) and is external. `org-link-abbrev-alist` (Emacs config) is not read. An undefined `[[X:…]]` prefix that is not a known scheme is external, never diagnosed.

### 2.2 Context decides meaning
| Context | Indexed | Completion | Goto | References | Diagnostics | Rename rewrites |
|---|---|---|---|---|---|---|
| prose, heading | yes | yes | yes | yes | explicit forms | yes |
| frontmatter value | yes | in fm | yes | yes | explicit forms (`[[…]]`); plain values are implicit, never | `[[…]]` yes; plain values only if resolved |
| html `href`/`src` | yes | no | yes | yes | yes | yes |
| code block / inline code | tagged `code` | no | if it resolves | no (backlinks leave code out) | never | no |
| comment (`<!-- -->`, [Obsidian](https://obsidian.md) `%%…%%`) | tagged | no | yes | no | never | no |

Diagnosed broken links get the root's broken-link severity (§3.3) in any
context. Goto works in code (a `[[note]]` in a README fence); diagnostics
never do (`[[{{filename-stem}}]]`, Lean `[[]]`). Fences follow CommonMark
(``` or `~~~`, ≥ 3, close ≥ open, unterminated runs to EOF, indented code).
Org: `#+begin_src`/`#+begin_example` blocks and `: ` lines. Math counts as
code.

### 2.3 Confidence levels
A target is **indexed** (a `files` row) and/or **on disk** (`stat` succeeds); gitignored, hidden, non-md and pruned files can be on disk but not indexed.

| Level | Forms | Becomes a link when | Broken |
|---|---|---|---|
| explicit | md, wiki, org, ref link, html href | always | diagnostic only if neither indexed nor on disk (severity §3.3) |
| implicit | bare path, `<path>`, plain frontmatter value | target exists (index lookup, then one `stat`) | silently dropped |
| external | URLs, `file:` outside root, `#+LINK` abbreviations | always | never |

An explicit link whose resolved path exists on disk (with or without `.md`) is "unindexed": goto and hover work, no diagnostic.

Bare-path guards: contains `/`, starts `./`/`../`/`~/`, or has a known
extension; no whitespace, not in a URL, not preceded by `:` or `.`. Path
set first; `stat` only `./`/`../`/`~/` forms, at most once per token, never
in lazy mode on a virtual FS. Found by an index-time pre-pass, resolved
lazily for the open buffer and reference queries only.

### 2.4 Resolution ladder
One function for every feature. Normalise: expand `#+LINK` (result is
external), URL-decode, strip `.md`/`.markdown`/`.org` and
`#anchor`/`::search`, NFC, case-fold on case-insensitive FS. The same
normalisation builds the in-memory lookup keys, so steps 1–8 are key
lookups.

**The first step with at least one hit stops the ladder.**

1. relative to the linking file (CommonMark, [Gollum](https://github.com/gollum/gollum))
2. relative to the root (zk `wiki` format, Obsidian "absolute")
3. site-rooted `/x/y` → root-relative, then under the docs dir when a docs-tool marker exists: mkdocs `docs_dir` (default `docs/`), Docusaurus `docs/`, Hugo `content/`
4. stem (Obsidian shortest path, [Foam](https://foambubble.github.io/foam/), marksman)
5. frontmatter `id`, org `:ID:`, id prefix of filename (`202101011200 title.md`)
6. frontmatter `title`, then H1, by slug
7. frontmatter `aliases`
8. dialect transforms: [Logseq](https://logseq.com) `a/b` → `a___b.md` / `a%2Fb.md` ([Dendron](https://www.dendron.so) `a.b.c` is covered by stem)
9. zk partial match (filename/path contains): not indexable, so goto, hover and completion only, never diagnostics; a hint, not rewritten on rename

**Ties** at the stopping step: closest by path distance wins (Obsidian), flagged `ambiguous` with an info diagnostic and related locations; goto lists all. Vault A's 39 duplicate stems never tie: its links stop at step 2.

**Piped wikis** `[[x|y]]`: target left in Obsidian and Foam, right in Dendron and Gollum. Resolve left, else right if it resolves. The outcome feeds the vote, so completion inserts in the root's order.

**Org**: `file:` paths enter at step 1 (with `~` expansion); `::*Heading` →
heading slug, `::#id` → `:CUSTOM_ID:`, `::text` → text search in hover
only; `id:` → `:ID:` index. Targets outside the root are external (vault
A's 96 `file:~/…` links must not produce errors).

Every zk-resolved link in both vaults agrees with mdroots ([zk-differential](../research/zk-differential.md)).

## 3. Dialects without configuration (D8)

### 3.1 Markers
A marker is a file or directory whose existence names the dialect. A
tool's config is read only when its marker exists and the setting changes
an answer; never required or written. A missing or malformed config falls
back to the vote and defaults.

| Dialect | Marker | Reads | Specifics |
|---|---|---|---|
| zk | `.zk/` | `config.toml`: `[format.markdown]` `hashtags`, `colon-tags`, `multiword-tags`, `link-format`, `link-drop-extension`; `[lsp.diagnostics] dead-link` | id-prefixed filenames, partial match, `#multi word#`, `:colon:` tags; never touches `notebook.db` |
| Obsidian | `.obsidian/` | `app.json`: `useMarkdownLinks`, `newLinkFormat` | shortest-path stem, `#^block`, `![[embed]]`, `aliases`, nested tags, `%%comments%%` |
| marksman | `.marksman.toml` | — | title-slug |
| Foam | `.foam/`, `.vscode/foam.json` | — | stem; generated link reference definitions at file end count as one link |
| Dendron | `dendron.yml` | — | dot hierarchy, `[[alias\|note]]`, fm `id`, epoch-ms dates |
| [Logseq](https://logseq.com) | `logseq/config.edn` | — | `___`/`%2F` namespaces, `pages/` + `journals/`, `key:: value`, bullets |
| [org-mode](https://orgmode.org) / [org-roam](https://www.orgroam.com) | `.org` files, `.orgids`, `org-roam.db` | per-file `#+LINK` | `#+TITLE`, `#+FILETAGS`, `:ID:`, `[[id:]]`, `[[file:]]` |
| Gollum / GitHub wiki | `Home.md` + `_Sidebar.md` | — | same dir first, `[[text\|Page]]` |
| [mkdocs](https://www.mkdocs.org) / [Docusaurus](https://docusaurus.io) / [Hugo](https://gohugo.io) / [Jekyll](https://jekyllrb.com) / [mdBook](https://rust-lang.github.io/mdBook/) | `mkdocs.yml`, `docusaurus.config.*`, `hugo.toml`/`config.toml`+`content/`, `_config.yml`, `book.toml` | `mkdocs.yml`: `docs_dir` | site-rooted links, `slug`/`permalink`, shortcodes |
| [Zettlr](https://www.zettlr.com) | `.ztr-directory` | — | `[[id]]` against 14-digit ids |

Several markers (vault A: `.zk`, `.obsidian`) are merged, not ranked; the ladder accepts every style.

### 3.2 Vote and link style

`MemStore::vote` runs `mdroots_resolve::dialect::Vote` over the current
notes (overlays win; a lazy working set votes over what it holds): their
headings, tags, and non-External explicit links in referencing contexts
(not code or comments), each with its resolving ladder step. Computed on
first use per process, not stored, cached with the other whole-root caches
(diagnostics policy, backlink index); any content change (overlay,
refresh, watcher update) drops it. It yields:
- the insert style: the most common step among file-relative,
  root-relative, stem and title over resolved explicit links, ties to
  root-relative (vault A: root-relative; vault B: stem). A wiki link
  without `/` counts as stem even if it resolved at the root-relative step;
- wiki vs Markdown share; `.md` suffix share among Markdown links;
- `#tag` seen: ≥ 3 distinct tags in ≥ 2 files (to beat `#include`);
- H1-as-title if ≥ 70% of docs have exactly one H1;
- the resolved share.

**Link style for inserted links** (`link_style`, used by `Workspace::link_to`
and extract-note, [library §3.2](library.md#32-a-workspace-mdroots)). Precedence:
1. A zk config (`.zk` marker) wins, even over an Obsidian one.
   `[format.markdown] link-format = "wiki"` → `[[dir/stem]]` (zk wiki links
   are root-relative); `"markdown"` or absent (zk's default) → Markdown
   links relative to the file; a custom template with `[[` → `[[stem]]` if
   it uses `{{filename}}`, else `[[dir/stem]]`; without `[[` → relative
   Markdown links. `.md` kept only with `link-drop-extension = false`.
2. Else an Obsidian config (`.obsidian/app.json`): wiki unless
   `useMarkdownLinks`. Wiki: `newLinkFormat` `"shortest"` (default) →
   `[[stem]]`, `"relative"`/`"absolute"` → `[[dir/stem]]`. Markdown:
   `"absolute"` → root-relative, else file-relative. Suffix kept.
3. Else the vote: wiki if ≥ half the explicit links are wiki; root-relative
   if the insert style is (else `[[stem]]` or file-relative); `.md` if ≥
   half the Markdown links carry it.
4. A root without explicit links gets its marker's default: Obsidian
   `[[stem]]`; zk (before Obsidian) file-relative Markdown without `.md`;
   otherwise file-relative Markdown with `.md`.

Piped-wiki order and tag syntaxes beyond `#tag` are not voted
([ROADMAP](../ROADMAP.md)). Extract-note names files by the title's GitHub
slug. No notes from templates: mdroots is a scanner.

### 3.3 Diagnostics
| Broken explicit links | Severity |
|---|---|
| default | warning |
| existing config says error (zk `dead-link = "error"`), or > 98% of the root's explicit links resolve | error |
| < 80% resolve (imported or messy vault) | hint, because a wall of red errors makes people uninstall |

Never diagnosed: code, comments, implicit links, external links (incl.
`#+LINK`), targets outside the root, targets on disk but not indexed.

**Policy as implemented** (`DiagnosticPolicy` in `mdroots-core`, every
front end). The share resolves every link of the root, so
`MemStore::policy(lazy)` caches it until the next content change (overlay,
refresh, watcher update); a new non-note target (say, a PDF) counts as
resolving only after that. Synthetic 3,000-note notebook (release build):
diagnostics for every file in 46–65 ms.

- **Share.** Over the root's non-external explicit links in referencing
  contexts (not code or comments). Resolved, ambiguous and
  on-disk-but-unindexed targets count as resolving. Thresholds are strict
  inequalities. No such links: warning. zk `dead-link` wins over the
  share; `dead-link = "none"` turns broken links and anchors off.
- **Codes.** `BrokenLink` and `BrokenAnchor` (the one target note lacks the
  heading, custom id or block anchor) at the broken severity;
  `AmbiguousLink` at info, every candidate in `related`;
  `InvalidFrontmatter` at warning, the rest of the note still indexed
  (§4.1); `FrontmatterKeyCollision` at info on the losing key's line
  (`` `Title` is ignored; `title` sets the title ``, §4.1), whatever the
  broken-link policy. Partial matches (hints) are never diagnosed.
- **Lazy** ([roots §3](roots.md#3-lazy-and-vcs-enumerated-modes)). Only
  `stat`-checkable links are diagnosed and counted in the share: path link
  forms (markdown, reference, image, HTML, org) or any target containing
  `/`. Any other unresolved link (a bare `[[stem]]`) is a
  `NotInWorkingSet` hint, never broken.
- Order: by start offset, then `InvalidFrontmatter`, `FrontmatterKeyCollision`, `BrokenLink`,
  `BrokenAnchor`, `AmbiguousLink`, `NotInWorkingSet`.

**Point-fresh publishing.** Whole-index freshness never arrives for a peer
or lazy root, so a document's diagnostics are published once it is parsed
(buffer or disk) and each link target is looked up in the index and, if
missing, `stat`ed. `mdroots lsp` re-publishes a workspace's open documents
after each `refresh` and each watcher-driven change (§1.3); re-publishing
only affected documents (via `change_log`) is
[planned](#reads-with-derived-tables).

## 4. Frontmatter

### 4.1 Formats and parsing
- YAML `---…---` (or `...` close), TOML `+++…+++`, JSON `{…}`; org `#+KEY:` and `:PROPERTIES:`; Logseq `key:: value` at the top; [MultiMarkdown](https://fletcherpenney.net/multimarkdown/) `Key: value` only with no other frontmatter and a matching first line.
- Tolerant: unparsable YAML or TOML → `InvalidFrontmatter` warning, the rest still indexed. A line scan finds the entries; each counts only if the parser accepts its lines alone as a one-entry document (range extended over following lines starting with `]` or `}`, key line moved to column 0, continuation lines re-indented to 2 spaces, leading tabs read as spaces; YAML retries once with each unquoted flow-item-leading `#` hidden, `[#a, #b]`). A counting entry takes that parse's shape; block-map children are judged the same way, one level at a time. A non-counting entry has no value (`Value::Null` in `entries()`): no tag, title, alias, id, date or link, though `Document::frontmatter_fields()` shows it as written. Strict: values a human could read but the parser rejects alone give nothing (`title: Foo: bar`, `title: [Draft] My note`, `up: [[A]], [[B]]`, `tags: *x`), and the block's warning says so. A valid block is unaffected. Scalars and string lists kept; nested maps flattened (`a.b`).
- **Key case**: stored as written; mapping to a standard meaning is case-insensitive. On collision (`Title`/`title`, `title`/`linkTitle`) the exact standard name wins (§4.2 order), else the first case-insensitive match. Info diagnostic on the loser, `FrontmatterKeyCollision` (§3.3): the loser is a present entry spelled differently from the winner; exact duplicate keys, placeholder values and dates are not collisions.
- **Placeholders** `—`, `–`, `-`, `n/a`, `N/A`, `TBD`, `none`, `""`, `~`, `null` mean no value: stored, but not links, ids or titles, and not offered in completion (vault B: 51 values).
- **Comma-joined lists**: a value containing `, ` is split, each part tried as an implicit link; links only if every non-placeholder part resolves, else one string (vault B: 39 files like `"gen/a.html, gen/a.txt"`). Tags split without the resolve check.
- **Library API** (`mdroots_syntax::Frontmatter`): `entries()` gives the flattened `a.b` keys, placeholders mapped to `Value::Null`. `Document::frontmatter_fields()` gives the top-level entries as written, in source order, unflattened (re-read from source on first use; a parsed document holds only the entries): one `Field { key, value, range }` per entry, duplicates included. `FieldValue` is `Scalar` (source text on one line, `null` included), `List`, or `Map` with the child fields; `display()` joins a list with `, ` and shows a map as `{…}`. `range` covers the entry's lines in the document. Keys found only by the parser (TOML tables and dotted keys, YAML flow maps) take the inner block, and their children the parent's range. JSON fields are the flattened entries. Org fields take the block range. `parsed()` is false when a non-blank block yields no key. `inner()` is the text between the fences (for unfenced formats, the whole range). TOML values on the parser path come out as TOML writes them (`1.0`, a nested array as `[1, 2]`).
- **Unfenced headers**: `ParseOptions::unfenced_frontmatter` (default true); off, Logseq, MultiMarkdown and JSON headers are not detected and stay prose (their links are prose links). Fenced blocks and org keywords are unaffected.

### 4.2 Standard keys
| Meaning | Keys | Used for |
|---|---|---|
| title | `title`, `linkTitle`, `#+TITLE` | ladder step 6, completion label, workspace symbol, hover |
| aliases | `aliases`, `alias` (Hugo `aliases` = alternative site paths) | ladder step 7, completion |
| id | `id`, `uid`, `zettel-id`, `:ID:` | ladder step 5 |
| tags | `tags`, `tag`, `keywords`, `categories`, `#+FILETAGS`; list, comma or space string; leading `#` stripped | tag index, completion, references, `#tag` workspace symbols |
| dates | `date`, `created`, `updated`, `modified`, `lastmod`, `week`/`year`; ISO, `YYYY-MM-DD HH:MM`, epoch ms | completion ranking, hover |
| summary | `description`, `summary`, `abstract`, `excerpt` | completion detail, hover |
| site path | `slug`, `permalink`, `url` | site-rooted links |
| state | `draft`, `publish`, `status`, `stage`, `type` | hover badge, value filters |
| relations | any value with `[[…]]`, or a string/comma list that resolves as a path | real links: goto, backlinks, rename |

### 4.3 Schema-free features
Not built: [Planned design](#frontmatter-features).

## 5. Testing

Harnesses in [`bench/`](../../bench/): `mdsurvey.py` (corpus stats),
`mdresolve.py` (prototype ladder), `lspbench.py` (stdio client timing each
method to first non-empty result, plus diagnostics and RSS, vs `marksman`
and `zk lsp`; `--skip` drops methods, `--timeout` bounds each method's wait
including retries, methods missing from the server's capabilities are
`nocap` and never sent; records `phys_footprint` on macOS).

### 5.1 Differential and churn tests
1. **Resolution parity**: per link, compare with zk's `links.target_id` → `notes.path` (`sqlite3 -readonly`) and marksman's `textDocument/definition`. Buckets: agree, mdroots-only, other-only (fix or justify). Results: [zk-differential](../research/zk-differential.md).
2. **Diagnostics parity**: vs marksman `publishDiagnostics` (58 files in vault A, triage each) and zk `target_id IS NULL AND external = 0` (56 / 138). Expected differences: `#+LINK` abbreviations, gitignored targets on disk.
3. **Feature smoke**: symbols, hover, reference counts on the 10 most-linked notes vs marksman; context rules explain differences.
4. **Timing and memory**: cold (cache wiped), warm, warm after checkout of 50 files, 10 parallel instances; `phys_footprint` at N=10.
5. **Kill loop**: `kill -9` the reconciler at random batch boundaries; the DB converges to a clean index. Rename/edit storms with 3 peers.
6. **Freshness rule**, each vs a clean index: `cp -p`/`rsync -a`/`tar x` of an older version replaces indexed content; `touch` updates stat columns only; a file changing between stat and read is re-read. Built: `crates/mdroots-index/tests/reconcile.rs` and [roots](roots.md) fixture 17. With [derived tables](#derived-tables): a `parser_ver` bump re-parses each row once and an older reconciler does not undo it.
7. **GC while peers run**: with 3 peers open, schema cleanup, GC and forced rebuild unlink nothing while `<id>.open` is shared; peers detect the new generation and reopen. Built: [roots](roots.md) fixtures 14 and 15, `crates/mdroots-index/tests/gc.rs`, `crates/mdroots/tests/gc.rs`. Not built: the repeat with the cache dir deleted.
8. **Single writer**: 3 editors on one root. No peer writes (built: a peer's `refresh` leaves `PRAGMA data_version` unchanged, `crates/mdroots/tests/workspace.rs`). One peer save = one overlay parse + one reconciler parse/write. Cross-editor visibility p99 within the ~100–500 ms budget. [FSEvents replay](#change-sources) after offline edits, including a forced `MustScanSubDirs`. Native watcher: `crates/mdroots/tests/watch.rs` and the server's republish test.
9. **Org**: vault A's 11 `#+LINK` files give zero diagnostics and hover shows expanded URLs; fixtures for `[[t][d]]` in `.md` and multi-line `[[…]]`.
10. **Code context**: fenced `[[…]]` in a README, a zk template, Lean `[[]]` → zero diagnostics, goto still works; 44 gitignored targets quiet; dot-prefixed editor temp files never indexed.
11. **Frontmatter**: `"—"` makes no links; 39 comma-joined values give two links each; a `Title`/`title` collision resolves to `title`, with an info diagnostic on `Title`.

### 5.2 Thresholds still to validate
| Threshold | Where | Known so far |
|---|---|---|
| > 98% → error, < 80% → hint | §3.3 | vaults 94.7% / 93.5%: both get warnings without their zk config |
| missing-key hint at ≥ 95% | [Planned design](#frontmatter-features) | 7 / 3 hints, mostly generated dirs |
| batch ≤ 200 files / ≤ 50 ms | §1.1 | — |
| ~300 ms before sweeps of an existing DB | [Planned design](#background-work) | — |
| cold < 250 ms with background QoS | §0, [Planned design](#background-work) | walk + parse 104–129 ms on vault A without QoS limits |
| `change_log` 10k entries (also the planned hot-cache cap) | §1.2, §1.4 | — |
| WAL > 4 MB exit checkpoint; periodic interval and cap | [Planned design](#background-work) | — |
| `busy_timeout=2s` | §1.6 | 10 processes at once on one root: no `SQLITE_BUSY` reached a caller ([roots](roots.md) fixture 12) |
| tag vote ≥ 3 in ≥ 2 files; H1-as-title 70% | §3.2 | — |
| 7-day hysteresis for root and lazy decisions | [roots](roots.md) | — |

What is not built or not validated: [ROADMAP](../ROADMAP.md).

## Planned design

Not built. [ROADMAP](../ROADMAP.md) orders the work and says why each item waits.

### Derived tables
Parse-derived tables with indexed lookups, so a process need not hold
every note in memory. Deferred: the measured gap is small and the target
vaults (~730 and ~210 notes) are far below it (D9).
```sql
keys(file_id, kind, key)       -- kind: stem | path | slug | id | alias; INDEX(kind, key)
links(file_id, range, context, kind, target_raw, target_kind, target_key)
                               -- target_key normalised as in §2.4; INDEX(target_kind, target_key)
frontmatter(file_id, key, value)
dir_state(path PRIMARY KEY, mtime_ns, nentries)
```
plus `meta` keys `reconciled_at`, `fsevents_last_id`, `fsevents_volume_uuid`
and the voted conventions. Derived rows carry a `parser_ver`: rows from an
older parser are re-parsed in the background, and an older binary leaves
newer parses of unchanged content alone. Only a schema change gets a new
filename (`<id>.v<schema>.db`).

### Reads with derived tables
Indexed lookups (`links WHERE target_kind=? AND target_key=?`,
`keys WHERE kind=? AND key=?`, FTS); zk-style `LIKE '%x%'` stays off the
diagnostics path. Each process keeps a capped **hot cache**
(`stem/title/id/alias → file_id`) for completion, updated from `change_log
WHERE seq > :last_seen_seq` when `PRAGMA data_version` moved (~1.2 µs when
unchanged). A peer below the oldest `seq` rebuilds it; a new `generation`
restarts `seq` and drops cursor and hot cache. Incremental diagnostics: a
changed file affects its own links and those whose `target_key` matches its
keys; only affected open documents are re-published. A capped read pool
replaces the single connection once more queries read SQLite.

### Change sources
All reconciler-only:

| Source | Cost | Acted on by |
|---|---|---|
| FSEvents replay from `meta.fsevents_last_id` (macOS) | ~ms | reconciler at start: changes made while no mdroots ran, no walk |
| [Watchman](https://facebook.github.io/watchman/) `since` clock, if it already watches the root | ~ms | reconciler; never start a watch ourselves on a virtual FS |
| `dir_state` diff + file `stat` | ~1 µs/file, ~10 ms at 10k | reconciler fallback: readdir only dirs whose mtime changed |

`notify` hard-codes `kFSEventStreamEventIdSinceNow`; replay needs
`sinceWhen = fsevents_last_id` via `fsevent-sys` or direct FFI, and the
workspace forbids unsafe code. Replay may report only directories. On
`MustScanSubDirs`, `UserDropped`, `KernelDropped`, `EventIdsWrapped`, a
different volume id or purged history, fall back to the `dir_state` diff
for that subtree (or the root). The new event ID is committed only after
`HistoryDone`, in the batch's transaction. On Linux, a takeover runs one
`dir_state` diff before starting inotify.

### Background work
A background thread runs reconcile as a priority queue: the open file
and its directory; link targets of open buffers (`QOS_CLASS_UTILITY`);
files newer than `reconciled_at`; rows with an old `parser_ver`; a
throttled verification sweep at most daily. Background work runs at
`QOS_CLASS_BACKGROUND` (Linux: `nice` + idle `ioprio`). Sweeps of an
existing DB start after ~300 ms alive. On exit,
`wal_checkpoint(PASSIVE)` only if the WAL exceeds 4 MB; the reconciler also
checkpoints periodically and sets `journal_size_limit`, warning above a cap,
because a leaked read transaction blocks checkpoints. A peer seeing a stale
`reconciled_at` logs one warning and point-checks in memory. A cache read
error after open becomes a miss (parse the file, never fail the request).

### Frontmatter features
From the `frontmatter` table:
- key completion ranked by frequency in the root; value completion per key from used values (placeholders excluded);
- hover on a key: "used in 138/210 notes; top values …";
- references on a value: every note with the same value;
- **missing-key hint** only when ≥ 95% of sibling notes in the dir have the key; no schema file. Excludes dirs named `cache`, `generated`, `_site`, `public`, `out` or listed in `.mdrootsignore`. Would fire 7× in vault B, 3× in vault A.
