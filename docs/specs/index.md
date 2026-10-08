# Spec: index under churn, liberal links, dialects, frontmatter

Related: [roots](roots.md) (discovery, nesting, flock roles), [library](library.md) (crates, `Store` trait), [DECISIONS](../DECISIONS.md) (D3, D4, D5, D7, D8), [differential results](../research/m1-differential.md), measurement scripts in [`bench/`](../../bench/).

Goal: the user never thinks about the index. No init or reindex command; a
`kill -9` loses at most ~50 ms of work; ten editors starting at once answer
within milliseconds and improve as the index catches up.

## 0. Testbed

Two zk notebooks, both local APFS git repos: **vault A** (~730 notes, also has
`.obsidian/`) and **vault B** (~210-note research vault). mdroots only reads
them; state lives in the cache dir (D5) and rename/edit tests run on a copy.
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
| zk unresolved internal / share resolved | 57 / 94.7% | 138 / 93.5% |

| Baseline (`initialize` → first result) | marksman | zk lsp (prebuilt `notebook.db`) |
|---|---|---|
| vault A / vault B | 750–821 / 636–694 ms | 34–37 / 26–27 ms |
| RSS | 139–143 MB | 33–34 MB |
| doc/workspace symbols | yes | not implemented |

pulldown-cmark parses at ~1.1 GB/s, but opening a file costs ~100 µs on macOS (20k files ≈ 2 s), so warm start needs the DB.

The data shows: resolution must try several strategies (two vaults by one
author differ); `#+LINK` abbreviations are external; not indexed ≠ missing, so
dead-link checks `stat`; bare paths count only if they exist; code is not prose.

**Targets** (both vaults): cold (no DB) all features < 250 ms with synchronous
indexing (§1.5); warm < 30 ms to first result; private memory
(`phys_footprint`) < 35 MB per process at N=10 concurrent instances; resolved
links ≥ zk; zero diagnostics on links in code or on bare-path candidates. RSS
is not the target because mmap'd SQLite pages count in every process's RSS.

## 1. Index under churn

One writer per root (the flock reconciler), readers never write, no daemon (D3); the DB is a disposable cache (D4).

### 1.1 Rules
1. **The DB is a cache.** The open buffer and a `stat` are authoritative. The DB may be deleted at any time, including by the OS purging the cache dir.
2. **Small committed transactions**: ≤ 200 files or ≤ 50 ms. No "index complete" state; a 300 ms process still leaves its batches behind.
3. **Single writer.** Only the reconciler writes. Peers serve DB + in-memory overlays of their own unsaved and just-saved files.
4. **Every reconciler transaction is `BEGIN IMMEDIATE`** with `busy_timeout`, because a deferred read-then-write transaction gets `SQLITE_BUSY` at once and ignores `busy_timeout`.
5. **Reads never load the whole index.** Queries hit SQLite indexes; peers learn of writes from `change_log`.

### 1.2 Schema
```sql
files(id INTEGER PRIMARY KEY AUTOINCREMENT,   -- ids never reused
      path UNIQUE, ino, ctime_ns, mtime_ns, size, hash, parser_ver, indexed_at, state)
      -- state: 'ok' | 'dirty' | 'gone'; NULL stat columns = never read
keys(file_id, kind, key)       -- kind: stem | path | slug | id | alias; INDEX(kind, key)
links(file_id, range, context, kind, target_raw, target_kind, target_key)
                               -- target_key normalised as in §2.4; INDEX(target_kind, target_key)
frontmatter(file_id, key, value)
change_log(seq INTEGER PRIMARY KEY AUTOINCREMENT, file_id, kind)   -- add | mod | del
dir_state(path PRIMARY KEY, mtime_ns, nentries)
meta(key, value)   -- schema, generation, reconciled_at, fsevents_last_id,
                   -- fsevents_volume_uuid, voted conventions, root mode
```
`reconciled_at` = end of last full sweep; `generation` = UUID set at DB creation; peers keep `last_seen_seq` in memory only.

**File version** = `fstat` of the fd the content was read from (`stat`,
`open`, `read`, `fstat`); if `(ino, ctime_ns, size)` differs between the two,
retry (3×, then mark `dirty`). ctime, not mtime, is the key because `utimes`
can set mtime but not ctime (`cp -p`, `rsync -a`, `tar x`); mtime is a hint.

**Change detection.** Read a file when a §1.3 source reports it, its row is
missing or not `ok`, or `(ino, ctime_ns, size)` differs from the row.

**Freshness rule.** With one writer, the rule only decides whether to
re-parse. The reconciler's read is always newest, so stat columns are always stored:
```sql
BEGIN IMMEDIATE;
SELECT id, ctime_ns, hash, parser_ver FROM files WHERE path = :path;
-- :parser_ver = this binary's if re-parsed, else the stored one
-- :state      = 'ok', or 'dirty' for the coarse-ctime case
INSERT INTO files(path, ino, ctime_ns, mtime_ns, size, hash, parser_ver, state, indexed_at)
VALUES (:path, :ino, :ctime_ns, :mtime_ns, :size, :hash, :parser_ver, :state, :now)
ON CONFLICT(path) DO UPDATE SET
  ino = excluded.ino, ctime_ns = excluded.ctime_ns, mtime_ns = excluded.mtime_ns,
  size = excluded.size, hash = excluded.hash, parser_ver = excluded.parser_ver,
  state = excluded.state, indexed_at = excluded.indexed_at;
-- if re-parsed: rewrite headings, links, tags, keys, frontmatter; append change_log
COMMIT;
```

| Old row vs new read | Action |
|---|---|
| no row, `hash` differs, or stored `parser_ver` < ours | re-parse, rewrite derived rows, append `add`/`mod` |
| same `hash`, stored `parser_ver` ≥ ours | stat columns only (`touch`, checkout back and forth); no `change_log` entry |
| same `ctime_ns`, different `hash` | coarse-ctime FS (HFS+, ext3, some FUSE): store with `state='dirty'` so the next sweep re-reads |

**Parser upgrades**: bump `parser_ver`; old rows stay usable and are re-parsed in the background (open and linked files first); an older binary leaves newer parses of unchanged content alone. Only a schema change gets a new filename (`<id>.v<schema>.db`).

### 1.3 Finding what changed, cheapest first

Indexed set: `.md`, `.markdown` and `.org` files from the ignore-aware walk,
minus dot-prefixed files/dirs and editor temp files (`.m-reflow-*`,
`.m-preview-*`, `.#*`, `*~`, `*.swp`, `4913`). Other files can still be link
targets via `stat` (§2.3).

| Source | Cost | Acted on by |
|---|---|---|
| open buffers (`didOpen`/`didChange`/`didSave`) | free | the editor's own process, in its overlay; the reconciler writes its own editor's saves directly |
| native watcher (FSEvents, inotify) | free | reconciler only |
| LSP `didChangeWatchedFiles` | free | reconciler, only when it runs no native watcher; peers ignore it |
| FSEvents replay from `meta.fsevents_last_id` (macOS) | ~ms | reconciler at start: changes made while no mdroots ran, no walk |
| [Watchman](https://facebook.github.io/watchman/) `since` clock, if it already watches the root | ~ms | reconciler; never start a watch ourselves on a virtual FS such as [EdenFS](https://github.com/facebook/sapling) |
| `dir_state` diff + file `stat` | ~1 µs/file, ~10 ms at 10k | reconciler fallback: readdir only dirs whose mtime changed, stat every tracked file |
| git index | in-process | index-driven mode ([roots](roots.md)): file list only; each file is still `stat`ed |

**One owner for file events**, so one save is not parsed N times: a peer parses only what its own editor sends, into its overlay, dropped once the DB row's hash matches.

**FSEvents replay.** The `notify` crate hard-codes `kFSEventStreamEventIdSinceNow`,
so replay uses `fsevent-sys` (or direct FFI) with `sinceWhen = fsevents_last_id`.
Replay may report only directories. On `MustScanSubDirs`, `UserDropped`,
`KernelDropped`, `EventIdsWrapped`, a different volume UUID, or purged
history, fall back to the `dir_state` diff for that subtree (or the root). The
new event ID is committed only after `HistoryDone`, in the same transaction as
the batch it covers. Replay cost after a day offline is still to be measured.

**Reconcile is a priority queue:**
1. the open file, then its directory
2. link targets of open buffers (`QOS_CLASS_UTILITY`)
3. files with ctime/mtime newer than `reconciled_at`, newest first
4. rows with old `parser_ver`
5. everything else: verification sweep, throttled, ≤ once a day per root

A kill leaves `dirty` rows for the next process. Background work runs at
`QOS_CLASS_BACKGROUND` (E-cores); Linux uses `nice` + idle `ioprio`.

### 1.4 Incremental reads and peer freshness
- Queries are indexed lookups (`links WHERE target_kind=? AND target_key=?`, `keys WHERE kind=? AND key=?`, FTS). Every key the ladder matches is stored normalised. zk-style `LIKE '%x%'` cannot use an index and stays off the diagnostics path.
- Each process keeps a lazy **hot cache** (`stem/title/id/alias → file_id`) for completion, updated by `SELECT file_id, kind FROM change_log WHERE seq > :last_seen_seq`, checked only when `PRAGMA data_version` moved (~1.2 µs when unchanged). It has an entry cap; above it, completion uses indexed prefix queries. It counts in the 35 MB budget.
- **change_log semantics:**
  - `AUTOINCREMENT` on `files.id` and `seq`: never reused within a generation.
  - Delete → `gone` row, its links/keys deleted, `del` appended; the row is purged only after its `del` is trimmed.
  - The reconciler trims to the last ~10k entries; a peer whose cursor is below the oldest `seq` rebuilds its hot cache.
  - `del` evicts from the hot cache; `add`/`mod` re-reads that file's keys.
  - A new `generation` restarts `seq` at 1; peers drop cursor and hot cache.
- **Diagnostics are incremental**: a changed `file_id` affects its own links and the links whose `target_key` matches its keys. One indexed query (the SQL version of marksman's `Conn` diff). Publishing rule in §3.3.

### 1.5 Short-lived instances
Common: `nvim file.md` then `:q` after 2 s, CI, commit-message editors.
- Reply to `initialize` first, then open the DB. Never wait for the flock.
- **No DB:** index synchronously at once, open buffer → its link targets → the rest, in committed batches, no delay gate. Only the flock holder does the full pass; peers parse their buffer and its targets in memory.
- **Existing DB:** background sweeps start only after ~300 ms alive, so `nvim +wq` does no reconcile. It still answers from DB + buffer and point-checks its document (§3.3).
- A dead reconciler's flock is dropped by the kernel; its committed batches persist (WAL).
- Exit (`shutdown`, SIGTERM, stdin EOF): finish or roll back the batch (≤ 50 ms), `wal_checkpoint(PASSIVE)` only if WAL > 4 MB. Never VACUUM, optimize, or block on other processes.

### 1.6 Failures and races
| Case | Outcome |
|---|---|
| Two processes find no DB | the flock holder creates it (`BEGIN IMMEDIATE` + `CREATE … IF NOT EXISTS`); peers serve single-file results until it appears |
| Peer wants freshness, no flock holder | tries `LOCK_NB` and becomes reconciler |
| File restored with older mtime | new inode or newer ctime → re-read; hash change → re-parse |
| One save seen by N processes | saver parses into its overlay; reconciler parses once and writes; others see `change_log` |
| Reconciler dies mid-batch | txn rolls back; next holder resumes from `dirty` rows |
| Reconciler stuck (SIGSTOP, hung NFS/virtual-FS stat) | peers see stale `reconciled_at`, warn once, point-check their own files and targets in memory without writing |
| Ten start after reboot | one reconciler; nine serve the stale DB at once |
| `SQLITE_BUSY` | reconciler: `busy_timeout=2s`; readers retry a few times during WAL recovery |
| WAL growth (leaked read txn) | periodic `wal_checkpoint(PASSIVE)`, `journal_size_limit`, warning above a cap |
| Different binaries | different `parser_ver` share the DB, newer re-parses; different schema uses separate files |
| GC or schema cleanup while peers run | needs `LOCK_EX\|LOCK_NB` on `<id>.open`; fails while any peer holds `LOCK_SH` |
| DB corruption | `PRAGMA quick_check` at reconciler start; on failure build a new generation file, never rename/unlink the open one |
| DB deleted or purged | peers see the inode or `generation` change on their next `data_version` check and reopen; next reconciler rebuilds |

### 1.7 Files and connections
Location: the first local (`statfs` `MNT_LOCAL`), writable dir of
`$XDG_CACHE_HOME/mdroots`, `~/Library/Caches/mdroots` (macOS) or
`~/.cache/mdroots` (Linux), `$XDG_RUNTIME_DIR/mdroots`,
`/var/tmp/mdroots-$UID`, else in-memory (D5).

| File | Held by | Rule |
|---|---|---|
| `roots.v<k>.db` | registry | versioned filename |
| `roots/<id>.v<schema>.db` | SQLite (WAL) | one per root and schema; a rebuild writes a new generation file |
| `<id>.lock` | reconciler, `LOCK_EX\|LOCK_NB` | elects the reconciler; never unlinked |
| `<id>.open` | every process with the DB open, `LOCK_SH` | GC, cleanup and rebuild need `LOCK_EX\|LOCK_NB`, because unlinking an open SQLite DB can corrupt it |
| `discover.lock` | discovering process | global, held during discovery stages 2–4 |

Each process has a capped read-connection pool; only the reconciler holds a writer connection, while it holds the flock. Page caches count in the 35 MB budget. The `Store` trait is in `mdroots-core`; `mdroots-index` implements it on SQLite ([library](library.md)).

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
| prose, heading | yes | yes | yes | yes | yes (explicit forms) | yes |
| frontmatter value | yes | in fm | yes | yes | no | `[[…]]` yes; plain values only if resolved |
| html `href`/`src` | yes | no | yes | yes | warn only | yes |
| code block / inline code | tagged `code` | no | if it resolves | separate "mentions in code" group | never | no (code action offers it) |
| comment (`<!-- -->`, Obsidian `%%…%%`) | tagged | no | yes | hidden | never | no |

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
normalisation produces `keys` and `links.target_key`, so steps 1–8 are indexed lookups.

**The first step with at least one hit stops the ladder**; later steps are not consulted.

1. relative to the linking file (CommonMark, Gollum)
2. relative to the root (zk `wiki` format, Obsidian "absolute")
3. site-rooted `/x/y` → root-relative, then under the docs dir (`docs/`, `content/`, `src/`) when a mkdocs/Hugo/Docusaurus marker exists
4. stem (Obsidian shortest path, Foam, marksman)
5. frontmatter `id`, org `:ID:`, id prefix of filename (`202101011200 title.md`)
6. frontmatter `title`, then H1, by slug
7. frontmatter `aliases`
8. dialect transforms: Logseq `a/b` → `a___b.md` / `a%2Fb.md` (Dendron `a.b.c` is covered by stem)
9. zk partial match (filename/path contains): not indexable, so goto, hover and completion only, never diagnostics; a hint, not rewritten on rename

**Ties** at the stopping step: pick the closest by path distance (Obsidian), flag `ambiguous`, emit an info diagnostic with related locations, list all in goto. Vault A's 39 duplicate stems never tie because its links stop at step 2.

**Piped wikis** `[[x|y]]`: Obsidian and Foam put the target left, Dendron and Gollum right. Resolve left; if it fails and right resolves, use right. The outcome feeds the vote, so completion inserts in the root's order.

**Org**: `file:` paths enter at step 1 (with `~` expansion); `::*Heading` →
heading slug, `::#id` → `:CUSTOM_ID:`, `::text` → text search in hover only;
`id:` → `:ID:` index. Targets outside the root are external (vault A's 96
`file:~/…` links must not produce errors).

Against zk, every zk-resolved link in both vaults agrees ([m1-differential](../research/m1-differential.md)).

## 3. Dialects without configuration (D8)

### 3.1 Markers
| Dialect | Marker | Reads (optional) | Specifics |
|---|---|---|---|
| zk | `.zk/` | `config.toml`: `[format.markdown]` link-format/hashtags/colon-tags/multiword-tags, `[note] extension`, group `paths`, `[lsp.diagnostics] dead-link` | id-prefixed filenames, partial match, `#multi word#`, `:colon:` tags; never touches `notebook.db` |
| Obsidian | `.obsidian/` | `app.json` (`useMarkdownLinks`, `newLinkFormat`, `attachmentFolderPath`), daily-notes plugin | shortest-path stem, `#^block`, `![[embed]]`, `aliases`, nested tags, `%%comments%%` |
| marksman | `.marksman.toml` | `core.title_from_heading`, `completion.wiki.style` | title-slug |
| Foam | `.foam/`, `.vscode/foam.json` | — | stem; generated link reference definitions at file end count as one link |
| Dendron | `dendron.yml` | vaults (= sub-roots) | dot hierarchy, `[[alias\|note]]`, fm `id`, epoch-ms dates |
| Logseq | `logseq/config.edn` | `:file/name-format` | `___`/`%2F` namespaces, `pages/` + `journals/`, `key:: value`, bullets |
| org / org-roam | `.org` files, `.orgids`, `org-roam.db` | per-file `#+LINK` | `#+TITLE`, `#+FILETAGS`, `:ID:`, `[[id:]]`, `[[file:]]` |
| Gollum / GitHub wiki | `Home.md` + `_Sidebar.md` | — | same dir first, `[[text\|Page]]`, spaces ↔ dashes |
| mkdocs / Docusaurus / Hugo / Jekyll / mdBook | `mkdocs.yml`, `docusaurus.config.*`, `hugo.toml`/`config.toml`+`content/`, `_config.yml`, `book.toml` | docs dir, `SUMMARY.md` | site-rooted links, `slug`/`permalink`, shortcodes, `SUMMARY.md` order |
| Zettlr | `.ztr-directory` | — | `[[id]]` against 14-digit ids |

Several markers (vault A: `.zk`, `.obsidian`) are merged, not ranked; the ladder accepts every style.

### 3.2 Vote (stored in `meta`, recomputed after each full reconcile)
- share of links resolved per ladder step → completion insert style (vault A: root-relative; vault B: stem)
- piped order; wiki vs md links; `.md` suffix or not
- tag syntaxes in prose; `#tag` needs ≥ 3 distinct tags in ≥ 2 files (to beat `#include`)
- H1-as-title if ≥ 70% of docs have exactly one H1
- filename scheme (slug, id prefix, date), for a future `mdroots.new`

### 3.3 Diagnostics
| Broken explicit links | Severity |
|---|---|
| default | warning |
| existing config says error (zk `dead-link = "error"`), or > 98% of the root's explicit links resolve | error |
| < 80% resolve (imported or messy vault) | hint, because a wall of red errors makes people uninstall |

Never diagnosed: code, comments, implicit links, external links (incl.
`#+LINK`), targets outside the root, targets on disk but not indexed.

**Policy as implemented** (`DiagnosticPolicy` in `mdroots-core`, used by
every front end):

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
the DB and, where the row is missing or stale, `stat`ed. Later `change_log`
entries re-publish affected documents. In lazy roots ([roots](roots.md)), only
`stat`-checkable links (relative, root-relative) are diagnosed.

## 4. Frontmatter

### 4.1 Formats and parsing
- YAML `---…---` (or `...` close), TOML `+++…+++`, JSON `{…}`; org `#+KEY:` and `:PROPERTIES:`; Logseq `key:: value` at the top; MultiMarkdown `Key: value` only with no other frontmatter and a matching first line.
- Tolerant: unparsable YAML → info diagnostic, the rest still indexed. Scalars and string lists kept; nested maps flattened (`a.b`).
- **Key case**: stored as written; mapping to a standard meaning is case-insensitive. On collision (`Title`/`title`, `title`/`linkTitle`) the exact lowercase standard name wins, else first in document order; the other gets an info diagnostic.
- **Placeholders** `—`, `–`, `-`, `n/a`, `N/A`, `TBD`, `none`, `""`, `~`, `null` mean no value: stored, but not links, ids or titles, and not offered in completion (vault B: 51 values).
- **Comma-joined lists**: a value containing `, ` is split and each part tried as an implicit link; links only if every non-placeholder part resolves, else a plain string (vault B: 39 files like `"gen/a.html, gen/a.txt"`). Tags split without the resolve check.

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

### 4.3 Schema-free features (from `frontmatter(file_id, key, value)`)
- key completion ranked by frequency in the root; value completion per key from used values (placeholders excluded)
- hover on a key: "used in 138/210 notes; top values …"
- references on a value: every note with the same value
- **missing-key hint** only when the key is in ≥ 95% of sibling notes in the same dir; no schema file. Excludes dirs named `cache`, `generated`, `_site`, `public`, `out` or listed in `.mdrootsignore`. Fires 7× in vault B and 3× in vault A.

## 5. Testing

Harnesses in [`bench/`](../../bench/): `mdsurvey.py` (corpus stats),
`mdresolve.py` (prototype ladder), `lspbench.py` (stdio client timing each
method to first non-empty result, plus diagnostics and RSS, against `marksman`
and `zk lsp`). `lspbench.py` still needs `--skip`, per-request `--timeout`, a
capability check (it waits 30 s on unimplemented methods), and `phys_footprint`.

### 5.1 Differential and churn tests
1. **Resolution parity**: per link, compare with zk's `links.target_id` → `notes.path` (`sqlite3 -readonly`) and marksman's `textDocument/definition`. Buckets: agree, mdroots-only, other-only (fix or justify). Results: [m1-differential](../research/m1-differential.md).
2. **Diagnostics parity**: vs marksman `publishDiagnostics` (58 files in vault A, triage each) and zk `target_id IS NULL AND external = 0` (57 / 138). Expected differences: `#+LINK` abbreviations, gitignored targets on disk.
3. **Feature smoke**: symbols, hover, reference counts on the 10 most-linked notes vs marksman; differences explained by context rules.
4. **Timing and memory**: cold (cache wiped), warm, warm after checkout of 50 files, 10 parallel instances; `phys_footprint` at N=10.
5. **Kill loop**: `kill -9` the reconciler at random batch boundaries; DB converges to a clean index. Rename/edit storms with 3 peers.
6. **Freshness rule**, each checked against a clean index: `cp -p`/`rsync -a`/`tar x` of an older version replaces indexed content; `parser_ver` bump re-parses each row once and an older reconciler does not undo it; `touch` updates stat columns only; NULL-stat `dirty` rows get filled; two writes in one second on an HFS+ image leave `dirty` then converge; takeover after `kill -9` resumes from `dirty` rows.
7. **GC while peers run**: with 3 peers open, schema cleanup, GC and forced rebuild unlink nothing while `<id>.open` is shared; peers detect the new generation and reopen. Repeat with the cache dir deleted.
8. **Single writer**: 3 editors on one root. No peer opens a writer connection or write txn (SQLite authorizer or `sqlite3_trace` in test builds). One peer save = one overlay parse + one reconciler parse/write. Cross-editor visibility p99 within the ~100–500 ms budget. FSEvents replay after offline edits, including a forced `MustScanSubDirs`.
9. **Org**: vault A's 11 `#+LINK` files give zero diagnostics and hover shows expanded URLs; fixtures for `[[t][d]]` in `.md` and multi-line `[[…]]`.
10. **Code context**: fenced `[[…]]` in a README, a zk template, Lean `[[]]` → zero diagnostics, goto still works; 44 gitignored targets quiet; `.m-reflow-*` never indexed.
11. **Frontmatter**: `"—"` makes no links; 39 comma-joined values give two links each; a `Title`/`title` collision resolves to `title` with an info diagnostic.

### 5.2 Thresholds still to validate
| Threshold | Where | Known so far |
|---|---|---|
| > 98% → error, < 80% → hint | §3.3 | vaults 94.7% / 93.5%: both get warnings without their zk config |
| missing-key hint at ≥ 95% | §4.3 | 7 / 3 hints, mostly generated dirs |
| batch ≤ 200 files / ≤ 50 ms | §1.1 | — |
| ~300 ms before sweeps of an existing DB | §1.5 | — |
| cold < 250 ms with background QoS | §0, §1.3 | walk + parse 104–129 ms on vault A without QoS limits |
| `change_log` ~10k entries; hot-cache cap | §1.4 | — |
| WAL > 4 MB exit checkpoint; periodic interval and cap | §1.5, §1.6 | — |
| `busy_timeout=2s`; read retry count | §1.6 | — |
| tag vote ≥ 3 in ≥ 2 files; H1-as-title 70% | §3.2 | — |
| 7-day hysteresis for root and lazy decisions | [roots](roots.md) | — |

Order of work: parser + scanner + ladder as a library checked offline against both vaults; then the index with the §5.1 tests; then the LSP.
