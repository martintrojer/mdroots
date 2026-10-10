# Spec: index under churn, liberal links, dialects, frontmatter

Related: [roots](roots.md) (discovery, nesting, flock roles), [library](library.md) (crates, API), [DECISIONS](../DECISIONS.md) (D3, D4, D5, D7, D8, D9), [differential results](../research/zk-differential.md), measurement scripts in [`bench/`](../../bench/).

Goal: the user never thinks about the index. No init or reindex command; a
`kill -9` loses at most one uncommitted batch (≤ 200 files / ≤ 50 ms, §1.1);
ten editors starting at once answer within milliseconds from the cache. The
index is brought up to date synchronously on open, on `refresh`, and by the
opt-in watcher (§1.3); `mdroots lsp` runs that open on a background thread
and serves the opened file alone meanwhile ([library §3.6](library.md#36-embedding-the-server)).
Unbuilt mechanics are collected in the [Planned design](#planned-design) appendix.

## 0. Testbed

Two [zk](https://github.com/zk-org/zk) notebooks, both local APFS [git](https://git-scm.com) repos: **vault A** (~730 notes, also has
`.obsidian/`) and **vault B** (~210-note research vault). They are the
author's private vaults, not in the repo; `tests/corpus` holds scrubbed
shapes of them. mdroots only reads them; state lives in the cache dir (D5) and rename/edit tests run on a copy.
Counts skip hidden and editor temp files.

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

The data shows: resolution must try several strategies (two vaults by one
author differ); `#+LINK` abbreviations are external; not indexed ≠ missing, so
dead-link checks `stat`; bare paths count only if they exist; code is not prose.

**Targets** (both vaults): cold (no DB) all features < 250 ms with synchronous
indexing (§1.5); warm < 30 ms to first result; private memory
(`phys_footprint`) < 35 MB per process at N=10 concurrent instances; resolved
links ≥ zk; zero diagnostics on links in code or on bare-path candidates. RSS
is not the target because mmap'd [SQLite](https://sqlite.org) pages count in every process's RSS.

## 1. Index under churn

One writer per root (the flock reconciler), readers never write, no daemon (D3); the DB is a disposable cache (D4) of each note's bytes and stat, and every process answers queries from an in-memory index hydrated from it (D9). The crate is `mdroots-index`; the facade drives it (`crates/mdroots/src/indexing.rs`).

### 1.1 Rules
1. **The DB is a cache.** The open buffer and a `stat` are authoritative. The DB may be deleted at any time, including by the OS purging the cache dir.
2. **Small committed transactions**: ≤ 200 files or ≤ 50 ms per batch. No "index complete" state; a cancelled or killed reconcile leaves only whole batches behind.
3. **Single writer.** Only the reconciler writes. Peers serve the DB's content, files they re-read themselves, and their own unsaved buffers, all in memory.
4. **Every write transaction is `BEGIN IMMEDIATE`** with `busy_timeout` 2 s, because a deferred read-then-write transaction gets `SQLITE_BUSY` at once and ignores `busy_timeout`. The DB runs in WAL mode with `synchronous=NORMAL`.
5. **Unchanged files are never opened.** A process gets their bytes from the DB.

### 1.2 Schema

The schema (`SCHEMA = 2`, `crates/mdroots-index/src/db.rs`), created with `CREATE … IF NOT EXISTS` inside one `BEGIN IMMEDIATE` by whichever process opens the DB first:
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
A DB whose `meta.schema` differs is reported as corrupt. `change_log` is trimmed to its newest 10,000 entries in every write transaction. `SCHEMA` versions the per-root DB only (`roots/<id>.v2.db`); the registry has its own version and stays `roots.v1.db`.

**Full-text table.** The [FTS5](https://sqlite.org/fts5.html) table `fts` is
kept in sync by the same `BEGIN IMMEDIATE` transaction that writes `files`:
an upsert deletes and re-inserts the note's row, a remove deletes it, a stat
update leaves it. `IndexDb::search(query, limit)` takes plain text, never
FTS5 syntax: it splits on whitespace, drops terms without a letter or digit,
quotes each term (so `AND`, `NEAR(`, `-x` or `"` match as text), ANDs them
and lets the last match as a prefix. Results are ranked by `bm25`, then
path, each with FTS5's `snippet()` of about 12 tokens. Only the reconciler
writes the table; who reads it is in [library §3.2](library.md#32-a-workspace-mdroots).

**File version** = `fstat` of the fd the content was read from (`stat`, then
`read` returning the fd's stat); if `(ino, ctime_ns, size)` differs between
the two, re-stat and re-read, up to 3 times, then keep the last read. ctime,
not mtime, is the key because `utimes` can set mtime but not ctime (`cp -p`,
`rsync -a`, `tar x`); mtime is stored but never compared.

**Change detection** (`reconcile`, `crates/mdroots-index/src/reconcile.rs`).
For each path, in order:

| Case | Action |
|---|---|
| invalid root-relative path, not in the file set, missing, or not a regular file | drop the row (`Remove`, logs `del`) |
| row's `(ino, ctime_ns, size)` equals the stat, path not forced | reuse the row's bytes; no read, no write |
| dataless (cloud placeholder), not forced | leave the row as it is; reading would download the file |
| otherwise | read: new bytes → `Upsert` (logs `add` or `mod`); same bytes, new stat (`touch`) → `Stat` (stat columns only, no log entry) |
| stat or read error other than not-found | keep the row as it is; no content for that path |

Change detection compares content, not a hash: the bytes are stored anyway. There is
no `parser_ver`: every process parses the stored bytes, so a parser upgrade
needs no re-index.

There are no derived tables (keys, links, frontmatter); the planned ones are
in the [appendix](#derived-tables).

### 1.3 Finding what changed

Indexed set: `.md`, `.markdown` and `.org` files from the root's listing,
minus dot-prefixed names and editor temp files (`*~`, `#*#`, `*.swp`,
`4913`; [roots §1 stage 4](roots.md#stage-4-budgeted-walk)). Other files can still be link
targets via `stat` (§2.3).

A workspace syncs with the disk at three points, all
synchronous in the thread that runs them:

- **`Workspace::open_for`.** The reconciler uses the file list discovery just
  produced; on a registry hit (no fresh listing) it re-lists the root with
  `list_root` for the root's mode: a budgeted walk (marker, VCS and loose
  roots), a git index scan (index-driven, tracked-only),
  the enumerator (vcs-enumerated), or, for a lazy root, the working set of the
  opened file's directory ([roots §3](roots.md#3-lazy-and-vcs-enumerated-modes)). The opened file is always
  read when it has no row. Changes made while no mdroots process ran are
  caught here, by the re-list and re-stat.
- **`Workspace::refresh(&cancel)`.** A peer first tries to become the
  reconciler (the previous one may have exited). The reconciler re-lists as
  above and writes what changed; a peer re-stats the DB's file set and reads
  changed files into memory only. Overlays survive. `mdroots lsp` calls it on
  `didSave` (the saved document's workspace) and on `didChangeWatchedFiles`
  (every workspace containing a changed path that runs no native watcher).
- **`Workspace::refresh_paths(&paths, &cancel)`**, a point refresh: a note
  path is re-read, added or dropped; an existing directory adds the notes
  under it (hidden and pruned dirs skipped) and re-checks the indexed ones; a
  gone path drops every indexed note at or under it. The reconciler writes
  the changes; the store is patched in place (`MemStore::apply_contents`),
  not rebuilt. The native watcher calls it.

| Who | File set reconciled | Writes |
|---|---|---|
| reconciler, non-lazy root | the listing plus the opened file; rows not listed are removed. An unlistable root (over budget, aborted) keeps the DB's file set | yes |
| reconciler, lazy root | the DB's file set plus working-set files not in it; rows outside the working set are kept | yes |
| peer | the DB's file set (a peer never lists); for a lazy root plus the working set | no |
| peer of a still-empty DB (another process is indexing) | what discovery listed, or the working set | no |
| anyone, `refresh_paths` | only the given paths (and the notes under a given directory) | reconciler only |

After `open_for` and `refresh` the process rebuilds its `MemStore` from the
returned bytes (`MemStore::from_contents`) and swaps it in; the index lock is
held until the swap, so a point refresh and a full one never interleave.

**Native watcher** (`crates/mdroots/src/watch.rs`). Opt-in with
`Options::watch(true)`; libraries default to off, `mdroots lsp` turns it on,
the one-shot CLI commands never do. A workspace watches only if all hold:

- it is the reconciler (a peer promoted on `refresh` starts watching then);
- it has a DB (not memory mode);
- the root mode is marker, VCS or loose. Tracked-only roots can be a home
  directory, vcs-enumerated roots never walk, and lazy and single-file roots
  are never watched;
- the root's filesystem classifies as local: never virtual, remote or cloud
  (so never on a virtual filesystem such as
  [EdenFS](https://github.com/facebook/sapling), whose roots open lazy and
  are not watched).

[notify](https://crates.io/crates/notify) 8 (FSEvents on macOS, inotify on
Linux) watches the root recursively. A thread named `mdroots-watch`
debounces events (200 ms quiet, at most 1 s after the first), keeps paths
whose root-relative components are not hidden and not in the walk's prune
list, and that are notes, directories, or gone; then it calls
`refresh_paths`. Access events (its own reads) are ignored. An event for the
root directory itself, a lost-events rescan flag, a notify error or a failed
point refresh triggers a full `refresh`. The thread holds only a weak
reference: it ends when the last clone of the workspace drops, and never
keeps the process alive. A root notify cannot watch is simply not watched.

`Workspace::subscribe()` returns a channel of the absolute paths each
watcher-driven refresh changed (every indexed note after a full refresh).
Explicit `refresh` and `refresh_paths` calls send nothing. `mdroots lsp`
re-publishes the diagnostics of open documents from it, and ignores
`didChangeWatchedFiles` for a watching workspace, so the disk has one owner.
Measured: a note changed on disk by another program republishes in about
226 ms, with no save. Offline changes are caught by the re-list on open;
FSEvents replay and other change sources are [planned](#change-sources).


### 1.4 Reads and peer freshness
- Every query reads the process's `MemStore` (one `RwLock`),
  with overlays laid over the disk content. The one query that reads the DB
  is `full_text` in the reconciler (the FTS table, [library](library.md)
  §3.2). A peer learns of the reconciler's writes on its next `refresh`, which
  re-reads the DB rows. `change_log` is written (add/mod/del per path,
  trimmed to ~10k) but no reader follows it.
- Indexed lookups, a hot cache and incremental diagnostics are
  [planned](#reads-with-derived-tables).

### 1.5 Short-lived instances
Common: `nvim file.md` then `:q` after 2 s, CI, commit-message editors.
- There is no background thread unless `Options::watch` is set (only
  `mdroots lsp` sets it), so a process does its reconcile inside `open_for`
  and leaves nothing running. `nvim +wq` costs one open.
- **No DB:** the first process to take the root's flock creates the DB and
  writes every note in committed batches while it opens. A peer that finds
  the DB still empty indexes in memory without writing.
- **Existing DB:** the process reads the rows, re-stats every file and reads
  only the changed ones. Measured on a synthetic 3,000-note notebook
  (`mdroots check` on one note, schema 2 with the FTS table): 0.6–0.7 s with
  a fresh cache, 0.11 s with the DB present (release build, files in the OS
  cache). An 11-note vault: 0.32 s cold, under 0.01 s warm.
- A dead reconciler's flock is dropped by the kernel; its committed batches persist (WAL).
- `mdroots lsp` replies to `initialize` before opening any workspace; a
  root starts opening on a background thread on the first `didOpen` of a file
  in it, and the file is served alone until then (timings in
  [library §3.6](library.md#36-embedding-the-server)).
- Nothing runs VACUUM or optimize, and no process blocks on another.
  Sweeps and an exit checkpoint are [planned](#background-work).

### 1.6 Failures and races
| Case | Outcome |
|---|---|
| Two processes find no DB | the flock holder creates it (`BEGIN IMMEDIATE` + `CREATE … IF NOT EXISTS`) and writes it; peers index in memory meanwhile |
| Ten start at once | discovery is serialised under `discover.lock`; one registry row; one reconciler, the others peers ([roots §7](roots.md#7-fixtures), fixtures 12, 13) |
| Peer wants freshness, no flock holder | `refresh` tries `LOCK_EX\|LOCK_NB` and becomes the reconciler |
| File restored with older mtime | new inode or newer ctime → re-read; new bytes → upsert (fixture 17) |
| Reconciler killed or cancelled mid-batch | the open transaction rolls back; committed batches stay; the next reconciler re-stats every file anyway |
| Reconciler stopped (SIGSTOP) | peers still open and answer from the DB without waiting (fixture 18). A stale-reconciler warning is [planned](#background-work) |
| `SQLITE_BUSY` | `busy_timeout` 2 s on every connection |
| Cache dir deleted under a running process | the next process recreates it and becomes the reconciler; the old process keeps serving from memory (fixture 16) |
| Different schema | separate DB files by name (`<id>.v1.db` and `<id>.v2.db` never meet); a file with a different `meta.schema` is `Corrupt`. GC deletes old-schema files (§1.7) |
| DB corruption | the reconciler runs `PRAGMA quick_check` when it opens the DB or takes it over on promotion. On failure, or `Corrupt` from opening or mid-sync, it builds a new generation `<id>.v2-<gen8>.db` (`gen8` = the first 8 hex digits of its new `meta.generation`), fills it, and records it in the registry's `db_file` only after that first sync succeeds. The corrupt file is never renamed or unlinked while open; GC deletes it later. A peer that opens a corrupt file serves from an empty in-memory stand-in (it indexes like a peer of an empty DB) and never fails `open_for`. On `refresh` and `refresh_paths` every process re-reads the registry row and reopens when it names another file ([roots §7](roots.md#7-fixtures), fixture 15) |
| GC while peers run | GC deletes a root's DB files only while it holds `LOCK_EX\|LOCK_NB` on the root's `.open`; any process with the root open holds `LOCK_SH`, so the root is skipped until the next run (fixture 14) |

### 1.7 Files and connections
Location: `mdroots_index::cache_dir` picks the first local, writable
candidate of the D5 chain, else the index stays in memory. Each candidate's
filesystem is classified on its nearest existing ancestor before anything is
created; empty environment variables count as unset. The chosen dir is
created or reset to mode `0700`, because it holds copies of the user's
notes. `Options::cache_dir(path)` replaces the chain (the CLI sets it from
`MDROOTS_CACHE_DIR`, for every command including `lsp`);
`Options::index(IndexMode::Memory)` never touches the cache dir; an
`Options` with an explicit `fs`/`probe` (in-memory test trees) and no cache
dir stays in memory. File and lock names, and one lock scope per root and
schema: [roots §5](roots.md#files).

**GC** (`mdroots_index::gc`, `crates/mdroots-index/src/gc.rs`) runs at most
daily per cache dir: the process that sets `meta.gc_at` in `roots.v1.db`
inside a `BEGIN IMMEDIATE` transaction runs it. `open_for` checks whether it
is due (one registry read) after a successful open over the real
filesystem, and runs it on the same thread. Candidates, each deleted only
under `LOCK_EX|LOCK_NB` on its root's `.open` (else skipped until the next
run):

1. DB files of this schema that the root's `db_file` does not name (old
   generations), or of a root with no registry row;
2. DB files of another schema whose newest file is 7 days old or more (by
   mtime);
3. roots not seen for 30 days, or whose path is gone: their DB files and
   their registry row;
4. while the DB files left exceed 1 GiB: whole roots, least recently seen
   first, registry row included.

GC deletes only `.db`, `-wal` and `-shm` files directly in `roots/`, never
follows a symlink, and never deletes lock files: they are tiny, and
unlinking one a process waits on would let a second process take it.
`last_seen_ms` is stamped by every process on open and on refresh, at most
hourly per root.

Each workspace with a DB holds one connection, behind a mutex, used by open,
`refresh`, `refresh_paths` and the reconciler's `full_text`; other queries
never touch it. Only the reconciler writes through it.

## 2. Liberal link model (D7)

### 2.1 Structure first, then a liberal scan
1. **pulldown-cmark 0.13** (wikilinks, footnotes, tables, tasklists, YAML/TOML metadata, math), offset iterator, classifies every byte range: prose, heading, link, image, code (fenced/indented/inline), HTML, comment, math, frontmatter.
2. A hand-written single-pass **scanner** (no regex backtracking) over text ranges finds what CommonMark doesn't model. It also re-reads pulldown's wikilinks: `[[t][d]]` becomes an org link to `t` (pulldown reads it as a wikilink to `t][d`), and a wikilink spanning lines is demoted to text.

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

**Org `#+LINK`**: `#+LINK: T https://…/?t=%s` lines are collected per file; `[[T:123]]` expands (`%s` replaced, else appended) and is external. `org-link-abbrev-alist` is not read (it lives in Emacs config). An undefined `[[X:…]]` prefix that is not a known scheme is external, never diagnosed.

### 2.2 Context decides meaning
| Context | Indexed | Completion | Goto | References | Diagnostics | Rename rewrites |
|---|---|---|---|---|---|---|
| prose, heading | yes | yes | yes | yes | explicit forms | yes |
| frontmatter value | yes | in fm | yes | yes | explicit forms (`[[…]]`); plain values are implicit, never | `[[…]]` yes; plain values only if resolved |
| html `href`/`src` | yes | no | yes | yes | yes | yes |
| code block / inline code | tagged `code` | no | if it resolves | no (backlinks leave code out) | never | no |
| comment (`<!-- -->`, [Obsidian](https://obsidian.md) `%%…%%`) | tagged | no | yes | no | never | no |

Every diagnosed broken link is reported at the root's broken-link severity
(§3.3), whatever its context.

Goto works in code (a `[[note]]` in a README fence is worth jumping to);
diagnostics never do (`[[{{filename-stem}}]]`, Lean `[[]]`). Fences follow CommonMark (``` or `~~~`, ≥ 3, close ≥ open,
unterminated runs to EOF, indented code). Org: `#+begin_src`/`#+begin_example`
blocks and `: ` lines. Math counts as code.

### 2.3 Confidence levels
A target is **indexed** (a `files` row) and/or **on disk** (`stat` succeeds); gitignored, hidden, non-md and pruned files can be on disk but not indexed.

| Level | Forms | Becomes a link when | Broken |
|---|---|---|---|
| explicit | md, wiki, org, ref link, html href | always | diagnostic only if neither indexed nor on disk (severity §3.3) |
| implicit | bare path, `<path>`, plain frontmatter value | target exists (index lookup, then one `stat`) | silently dropped |
| external | URLs, `file:` outside root, `#+LINK` abbreviations | always | never |

An explicit link whose resolved path exists on disk (with or without `.md`) is "unindexed": goto and hover work, no diagnostic.

Bare-path guards: contains `/`, or starts `./`/`../`/`~/`, or has a known
extension; no whitespace, not in a URL, not preceded by `:` or `.`. Check the
path set first; `stat` only `./`/`../`/`~/` forms, at most once per token,
never in lazy mode on a virtual FS. Found by a pre-pass at index time but
resolved lazily, only for the open buffer and reference queries.

### 2.4 Resolution ladder
One function used by every feature. Normalise first: expand `#+LINK` (result
is external), URL-decode, strip `.md`/`.markdown`/`.org`, strip
`#anchor`/`::search`, NFC, case-fold on case-insensitive FS. The same
normalisation produces the in-memory lookup keys, so steps 1–8 are key lookups.

**The first step with at least one hit stops the ladder**; later steps are not consulted.

1. relative to the linking file (CommonMark, [Gollum](https://github.com/gollum/gollum))
2. relative to the root (zk `wiki` format, Obsidian "absolute")
3. site-rooted `/x/y` → root-relative, then under the docs dir when a docs-tool marker exists: mkdocs `docs_dir` (default `docs/`), Docusaurus `docs/`, Hugo `content/`
4. stem (Obsidian shortest path, [Foam](https://foambubble.github.io/foam/), marksman)
5. frontmatter `id`, org `:ID:`, id prefix of filename (`202101011200 title.md`)
6. frontmatter `title`, then H1, by slug
7. frontmatter `aliases`
8. dialect transforms: [Logseq](https://logseq.com) `a/b` → `a___b.md` / `a%2Fb.md` ([Dendron](https://www.dendron.so) `a.b.c` is covered by stem)
9. zk partial match (filename/path contains): not indexable, so goto, hover and completion only, never diagnostics; a hint, not rewritten on rename

**Ties** at the stopping step: pick the closest by path distance (Obsidian), flag `ambiguous`, emit an info diagnostic with related locations, list all in goto. Vault A's 39 duplicate stems never tie because its links stop at step 2.

**Piped wikis** `[[x|y]]`: Obsidian and Foam put the target left, Dendron and Gollum right. Resolve left; if it fails and right resolves, use right. The outcome feeds the vote, so completion inserts in the root's order.

**Org**: `file:` paths enter at step 1 (with `~` expansion); `::*Heading` →
heading slug, `::#id` → `:CUSTOM_ID:`, `::text` → text search in hover only;
`id:` → `:ID:` index. Targets outside the root are external (vault A's 96
`file:~/…` links must not produce errors).

Against zk, every zk-resolved link in both vaults agrees ([zk-differential](../research/zk-differential.md)).

## 3. Dialects without configuration (D8)

### 3.1 Markers
A marker is a file or directory whose existence names the dialect. A tool's
config is read only when its marker is present and the setting changes an
answer; it is never required or written, and a missing or malformed config
falls back to the vote and the defaults.

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

`MemStore::vote` runs the `mdroots_resolve::dialect::Vote` over
the current notes (overlays win; a lazy working set votes over what it holds):
every note's headings and tags, and its links in referencing contexts (not code
or comments) that are not External, each with the ladder step that resolved it.
Only explicit links count. The result is computed on first use and cached with
the other whole-root caches (diagnostics policy, backlink index), so any content
change (overlay, refresh, watcher update) drops it. It is per process, not
stored. It yields:
- the insert style: the most common step among file-relative, root-relative,
  stem and title over resolved explicit links, ties to root-relative
  (vault A: root-relative; vault B: stem). A wiki link without a `/` counts
  as stem even when it resolved at the root-relative step: root-relative
  means a path with a slash;
- wiki vs Markdown share; `.md` suffix share among Markdown links;
- `#tag` seen: ≥ 3 distinct tags in ≥ 2 files (to beat `#include`);
- H1-as-title if ≥ 70% of docs have exactly one H1;
- the resolved share.

**Link style for inserted links** (`link_style`, used by `Workspace::link_to`
and extract-note, [library §3.2](library.md#32-a-workspace-mdroots)). Precedence:
1. A zk config (`.zk` marker) wins, also over an
   Obsidian one in the same root. `[format.markdown]
   link-format = "wiki"` gives `[[dir/stem]]` (zk's wiki links are
   root-relative); `"markdown"` or absent (zk's default) gives Markdown links
   relative to the file; a custom template with `[[` gives `[[stem]]` when it
   uses `{{filename}}`, else `[[dir/stem]]`, and one without `[[` gives
   relative Markdown links. The `.md` suffix is kept only with
   `link-drop-extension = false`.
2. Else an Obsidian config (`.obsidian/app.json`): wiki unless
   `useMarkdownLinks`; for wiki, `newLinkFormat` `"shortest"` (the default)
   gives `[[stem]]`, `"relative"` or `"absolute"` give `[[dir/stem]]`; for
   Markdown, `"absolute"` gives root-relative links, anything else
   file-relative; the suffix is kept.
3. Else the vote: wiki when at least half the explicit links are wiki,
   root-relative when the insert style is root-relative (else `[[stem]]` or
   file-relative), and the `.md` suffix when at least half the Markdown links
   carry it.
4. A root without explicit links gets its marker's default: `[[stem]]` for
   Obsidian, Markdown links relative to the file without `.md` for zk (zk
   before Obsidian), otherwise Markdown links relative to the file with
   `.md`.

Piped-wiki order and tag syntaxes beyond `#tag` are not voted
([ROADMAP](../ROADMAP.md)). Extract-note
names files by the GitHub slug of the title. Creating notes from templates
is out of scope: mdroots is a scanner.

### 3.3 Diagnostics
| Broken explicit links | Severity |
|---|---|
| default | warning |
| existing config says error (zk `dead-link = "error"`), or > 98% of the root's explicit links resolve | error |
| < 80% resolve (imported or messy vault) | hint, because a wall of red errors makes people uninstall |

Never diagnosed: code, comments, implicit links, external links (incl.
`#+LINK`), targets outside the root, targets on disk but not indexed.

**Policy as implemented** (`DiagnosticPolicy` in `mdroots-core`, used by
every front end). Computing the share resolves every link of the root, so
`MemStore::policy(lazy)` computes it once and keeps it until the next
content change (an overlay, a refresh, a watcher update).
The share is cached, so a new non-note target file (say, a PDF) counts as
resolving only after the next content change, refresh or watcher update.
Measured on a synthetic 3,000-note notebook (release build): diagnostics for
every file in 46–65 ms.

- **Share.** Counted over the root's explicit links in referencing contexts
  (not code or comments), external links excluded. Resolved, ambiguous and
  on-disk-but-unindexed targets count as resolving. The thresholds are
  strict: > 98% is error, < 80% hint. A root with no such links gets warning.
  zk `dead-link` wins over the share; `dead-link = "none"` turns broken links
  and anchors off.
- **Codes.** `BrokenLink` and `BrokenAnchor` (a link to one note in which the
  heading, custom id or block anchor is missing) at the broken severity;
  `AmbiguousLink` at info, with every candidate in `related`;
  `InvalidFrontmatter` at info, the rest of the note still indexed. Partial
  matches (hints) are never diagnosed.
- **Lazy.** The share counts only `stat`-checkable links: path link forms
  (markdown, reference, image, HTML, org) or any target containing `/`. An
  unresolved link that is not `stat`-checkable (a bare `[[stem]]`) is a
  `NotInWorkingSet` hint, never broken.
- Diagnostics come sorted by start offset, then `InvalidFrontmatter`,
  `BrokenLink`, `BrokenAnchor`, `AmbiguousLink`, `NotInWorkingSet`.

**Point-fresh publishing.** Waiting for whole-index freshness never ends for a
peer or a lazy root. A process publishes a document's diagnostics once the
document is parsed (buffer or disk) and each link target has been looked up in
its index and, where missing, `stat`ed. `mdroots lsp` re-publishes the open
documents of a workspace after each `refresh`, and, for a watching workspace,
after each watcher-driven change (§1.3). Re-publishing only the affected
documents (by following `change_log`) is [planned](#reads-with-derived-tables). In lazy roots ([roots §3](roots.md#3-lazy-and-vcs-enumerated-modes)), only
`stat`-checkable links (relative, root-relative) are diagnosed.

## 4. Frontmatter

### 4.1 Formats and parsing
- YAML `---…---` (or `...` close), TOML `+++…+++`, JSON `{…}`; org `#+KEY:` and `:PROPERTIES:`; Logseq `key:: value` at the top; [MultiMarkdown](https://fletcherpenney.net/multimarkdown/) `Key: value` only with no other frontmatter and a matching first line.
- Tolerant: unparsable YAML → `InvalidFrontmatter` info diagnostic, the rest still indexed. Scalars and string lists kept; nested maps flattened (`a.b`).
- **Key case**: stored as written; mapping to a standard meaning is case-insensitive. On collision (`Title`/`title`, `title`/`linkTitle`) the exact standard name wins (in the order listed in §4.2), else the first case-insensitive match. An info diagnostic on the loser is [planned](#frontmatter-features).
- **Placeholders** `—`, `–`, `-`, `n/a`, `N/A`, `TBD`, `none`, `""`, `~`, `null` mean no value: stored, but not links, ids or titles, and not offered in completion (vault B: 51 values).
- **Comma-joined lists**: a value containing `, ` is split and each part tried as an implicit link; links only if every non-placeholder part resolves, else a plain string (vault B: 39 files like `"gen/a.html, gen/a.txt"`). Tags split without the resolve check.
- **Library API** (`mdroots_syntax::Frontmatter`): `entries()` gives the flattened `a.b` keys with placeholders mapped to `Value::Null`. `fields()` gives the top-level entries as written, in source order, before flattening: one `Field { key, value, range }` per entry, duplicate keys included. `FieldValue` is `Scalar` (source text on one line, `null` included), `List`, or `Map` with the child fields; `display()` shows a list joined with `, ` and a map as `{…}`. `range` covers the entry's lines in the document. Keys found only by the parser (TOML tables and dotted keys, YAML flow maps) take the inner block, and their children take the parent's range. JSON fields are the flattened entries. Org fields take the block range. `parsed()` is false when a non-blank block yields no key. `inner()` is the text between the fences (for unfenced formats, the whole range). TOML values on the parser path come out as TOML writes them (`1.0`, a nested array as `[1, 2]`).
- **Unfenced headers**: `ParseOptions::unfenced_frontmatter` (default true) turns off detection of Logseq, MultiMarkdown and JSON headers. When off, those lines stay prose, so their links are prose links. Fenced blocks and org keywords are not affected.

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
Not built: key and value completion, hover on a key, references on a value
and the missing-key hint ([Planned design](#frontmatter-features)).

## 5. Testing

Harnesses in [`bench/`](../../bench/): `mdsurvey.py` (corpus stats),
`mdresolve.py` (prototype ladder), `lspbench.py` (stdio client timing each
method to first non-empty result, plus diagnostics and RSS, against `marksman`
and `zk lsp`). `lspbench.py` still needs `--skip`, per-request `--timeout`, a
capability check (it waits 30 s on unimplemented methods), and `phys_footprint`.

### 5.1 Differential and churn tests
1. **Resolution parity**: per link, compare with zk's `links.target_id` → `notes.path` (`sqlite3 -readonly`) and marksman's `textDocument/definition`. Buckets: agree, mdroots-only, other-only (fix or justify). Results: [zk-differential](../research/zk-differential.md).
2. **Diagnostics parity**: vs marksman `publishDiagnostics` (58 files in vault A, triage each) and zk `target_id IS NULL AND external = 0` (56 / 138). Expected differences: `#+LINK` abbreviations, gitignored targets on disk.
3. **Feature smoke**: symbols, hover, reference counts on the 10 most-linked notes vs marksman; differences explained by context rules.
4. **Timing and memory**: cold (cache wiped), warm, warm after checkout of 50 files, 10 parallel instances; `phys_footprint` at N=10.
5. **Kill loop**: `kill -9` the reconciler at random batch boundaries; DB converges to a clean index. Rename/edit storms with 3 peers.
6. **Freshness rule**, each checked against a clean index: `cp -p`/`rsync -a`/`tar x` of an older version replaces indexed content; `touch` updates stat columns only; a file changing between stat and read is re-read. Built: `crates/mdroots-index/tests/reconcile.rs` and [roots](roots.md) fixture 17. With [derived tables](#derived-tables): a `parser_ver` bump re-parses each row once and an older reconciler does not undo it.
7. **GC while peers run**: with 3 peers open, schema cleanup, GC and forced rebuild unlink nothing while `<id>.open` is shared; peers detect the new generation and reopen. Built: [roots](roots.md) fixtures 14 and 15, plus `crates/mdroots-index/tests/gc.rs` and `crates/mdroots/tests/gc.rs`. Not built: the repeat with the cache dir deleted.
8. **Single writer**: 3 editors on one root. No peer writes (built: a peer's `refresh` leaves `PRAGMA data_version` unchanged, `crates/mdroots/tests/workspace.rs`). One peer save = one overlay parse + one reconciler parse/write. Cross-editor visibility p99 within the ~100–500 ms budget. [FSEvents replay](#change-sources) after offline edits, including a forced `MustScanSubDirs`. The native watcher is covered by `crates/mdroots/tests/watch.rs` and the server's republish test.
9. **Org**: vault A's 11 `#+LINK` files give zero diagnostics and hover shows expanded URLs; fixtures for `[[t][d]]` in `.md` and multi-line `[[…]]`.
10. **Code context**: fenced `[[…]]` in a README, a zk template, Lean `[[]]` → zero diagnostics, goto still works; 44 gitignored targets quiet; dot-prefixed editor temp files never indexed.
11. **Frontmatter**: `"—"` makes no links; 39 comma-joined values give two links each; a `Title`/`title` collision resolves to `title` (the info diagnostic is planned).

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
So that a process no longer needs every note in memory, the schema gains
tables built from the parse, with indexed lookups. Deferred because the
measured gap is small and the target vaults (~730 and ~210 notes) are far
below it (D9).
```sql
keys(file_id, kind, key)       -- kind: stem | path | slug | id | alias; INDEX(kind, key)
links(file_id, range, context, kind, target_raw, target_kind, target_key)
                               -- target_key normalised as in §2.4; INDEX(target_kind, target_key)
frontmatter(file_id, key, value)
dir_state(path PRIMARY KEY, mtime_ns, nentries)
```
plus `meta` keys `reconciled_at`, `fsevents_last_id`, `fsevents_volume_uuid`
and the voted conventions. Derived rows depend on the parser, so they bring a
`parser_ver` column: rows parsed by an older parser are re-parsed in the
background, and an older binary leaves newer parses of unchanged content
alone. Only a schema change gets a new filename (`<id>.v<schema>.db`).

### Reads with derived tables
Queries become indexed lookups (`links WHERE target_kind=? AND
target_key=?`, `keys WHERE kind=? AND key=?`, FTS); zk-style `LIKE '%x%'`
stays off the diagnostics path. Each process keeps a capped **hot cache**
(`stem/title/id/alias → file_id`) for completion, updated from `change_log
WHERE seq > :last_seen_seq` when `PRAGMA data_version` moved (~1.2 µs when
unchanged). A peer below the oldest `seq` rebuilds its hot cache; a new
`generation` restarts `seq` and drops cursor and hot cache. Diagnostics
become incremental: a changed file affects its own links and the links whose
`target_key` matches its keys, and only the affected open documents are
re-published. A capped read pool replaces the single connection once more
queries read SQLite.

### Change sources
All reconciler-only:

| Source | Cost | Acted on by |
|---|---|---|
| FSEvents replay from `meta.fsevents_last_id` (macOS) | ~ms | reconciler at start: changes made while no mdroots ran, no walk |
| [Watchman](https://facebook.github.io/watchman/) `since` clock, if it already watches the root | ~ms | reconciler; never start a watch ourselves on a virtual FS |
| `dir_state` diff + file `stat` | ~1 µs/file, ~10 ms at 10k | reconciler fallback: readdir only dirs whose mtime changed |

`notify` hard-codes `kFSEventStreamEventIdSinceNow`, so replay needs
`sinceWhen = fsevents_last_id` through `fsevent-sys` or direct FFI, and the
workspace forbids unsafe code. Replay may report only directories. On
`MustScanSubDirs`, `UserDropped`, `KernelDropped`, `EventIdsWrapped`, a
different volume id, or purged history, fall back to the `dir_state` diff
for that subtree (or the root). The new event ID is committed only after
`HistoryDone`, in the same transaction as the batch it covers. On Linux, a
takeover runs one `dir_state` diff before starting inotify.

### Background work
With a background thread, reconcile becomes a priority queue: the open file
and its directory; link targets of open buffers (`QOS_CLASS_UTILITY`); files
newer than `reconciled_at`; rows with an old `parser_ver`; a throttled
verification sweep at most once a day. Background work runs at
`QOS_CLASS_BACKGROUND`; Linux uses `nice` + idle `ioprio`. Sweeps of an
existing DB start only after ~300 ms alive. On exit,
`wal_checkpoint(PASSIVE)` runs only if the WAL exceeds 4 MB; the reconciler
also checkpoints periodically and sets `journal_size_limit`, with a warning
above a cap, because a leaked read transaction blocks checkpoints. A peer
seeing a stale `reconciled_at` logs one warning and point-checks in memory.
Any cache read error after open becomes a miss (parse the file, never fail
the request).

### Frontmatter features
From the `frontmatter` table:
- key completion ranked by frequency in the root; value completion per key from used values (placeholders excluded);
- hover on a key: "used in 138/210 notes; top values …";
- references on a value: every note with the same value;
- **missing-key hint** only when the key is in ≥ 95% of sibling notes in the same dir; no schema file. Excludes dirs named `cache`, `generated`, `_site`, `public`, `out` or listed in `.mdrootsignore`. Would fire 7× in vault B and 3× in vault A;
- an info diagnostic on the losing key of a collision (§4.1).
