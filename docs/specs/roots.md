# Spec: root discovery, size budgets, and many instances

Related: [index spec](index.md), [library spec](library.md), D3, D4, D5 in [DECISIONS.md](../DECISIONS.md).

Hard rule: **mdroots never calls `readdir` on a tree before it has established that the tree is local and bounded.** A monorepo on a virtual filesystem such as [EdenFS](https://github.com/facebook/sapling) (from [Sapling](https://sapling-scm.com/)) can hold millions of on-demand files; a recursive walk there takes hours. Every rule below exists to prevent that walk.

Implementation:

- **Crate and traits.** `mdroots-roots` implements §1–§4, the §6 data model and §7 fixtures 1–11. Every discovery filesystem call goes through its `Probe` trait (`StdProbe` over `std::fs` and `statfs`, `FakeProbe` in memory, `Counting` to count and forbid `read_dir`), so tests can prove the hard rule. The registry is a trait with an in-memory `MemRegistry`; a vcs-enumerated root is listed through an `Enumerator`.
- **Outside `Probe`.** `DiscoverLock` opens `discover.lock` with `std::fs`; the `SlFiles` enumerator runs `sl files` in a child process.
- **Caller duties.** `discover()` takes a canonical absolute file path; the caller canonicalises (including `F_GETPATH` on macOS).
- **Re-listing.** `list_root` re-lists a known root per mode (budgeted walk, [git](https://git-scm.com) index scan, enumerator). It returns `None` for lazy and single-file roots, and on abort or over budget.
- **What `mdroots-index` adds.** The [SQLite](https://sqlite.org) registry (`SqliteRegistry`), the cache dir choice, the §5 flock roles and the per-root DB; the facade takes `discover.lock` around discovery.

## 0. Measurements (M-series Mac, APFS)

| What | Result |
|---|---|
| Unpruned walk of a projects folder (`rg --files --no-ignore --hidden`) | 662k entries, 1.64 s (~400k entries/s) |
| Same, honouring ignore files, `*.md` only | 1,023 md, 131 ms |
| Same, pruned at nested repos, dot dirs, `node_modules`/`target` | 506 files, 4 md, < 70 ms (490 files and 1 md are one unpacked source tarball) |
| `git ls-files` (twice) in a 3.8k-file repo ([Neovim](https://neovim.io)) | 90 ms, mostly process spawns |
| `.git/index` header (12 bytes) | tracked entry count, O(1) |
| `statfs` on a large virtual-filesystem checkout | `f_fstypename = "edenfs:"` (trailing colon), `MNT_LOCAL` unset |
| `statfs` on a build-output dir inside that checkout | `apfs`, `MNT_LOCAL` set, own `st_dev`: a local mount inside the virtual repo; a checkout can have several |
| Small notes repo on EdenFS | also `edenfs:`, `MNT_LOCAL` unset |
| `.eden/` in the checkout | present at every depth; `readlink(<dir>/.eden/root)` gives the checkout root in one call |
| 23 `stat`s of missing names | ~2–5 ms on Eden, ~0.06 ms on APFS |
| `statfs` on a Google Drive folder | `apfs`, `MNT_LOCAL` set, same `st_dev` as `$HOME`: statfs cannot see cloud folders |
| `readdir` per directory, warm cache | median 0.19–0.22 ms/dir |
| `mdroots-roots` budgeted walk of this repo (37 dirs, 163 entries; debug build, warm cache) | 11–40 ms, median ~0.16 ms/dir |
| Loose decision for a 30-note temp folder (debug build) | ~2 ms |
| One file in a large virtual-filesystem checkout (debug build) | lazy after 6 `stat`/`statfs` calls, 0 `readdir`, 1.4 ms |
| Rust walker on a cold cache, a cloud folder or EdenFS | not measured |

So: pruning makes walks ~12× cheaper and is required. A few `stat`/`statfs` calls detect big or virtual trees; test `MNT_LOCAL`, not the type name, and repeat it at mount boundaries inside a virtual tree. Cloud folders are caught by path, `SF_DATALESS` and walk speed.

## 1. Discovery stages, cheap to expensive

Any stage may stop with a decision. Only stage 4 calls `readdir` recursively. Stages 2–4 run under the global `discover.lock` (§5).

### Stage 1: registry lookup

Longest-prefix match of `realpath(file)` in the registry (`roots.v<k>.db`); on macOS also canonicalise with `F_GETPATH` (APFS is case-insensitive). Rows containing the file are tried nearest first. A row is skipped for the next one up when:

- its recorded marker is gone (one `stat`): a deleted nested `proj/.git` hands its files to the parent row, with no re-walk;
- it was decided by an LSP `workspaceFolders` entry (marker `workspaceFolders`) and that folder is not a workspace folder of this session. Such a row never makes a folder a root for a later session without it, and never blocks registering an enclosing root.

The first unskipped row is a hit only if:

1. the root's `st_dev` and `fs_type` match the recorded values, and `dir(file)` is on that `st_dev` (one more `stat`). So a filesystem mounted below the root, with or without its own row, is never served by the root (else re-decide from stage 2, lazy verdicts included);
2. no new marker sits between `dir(file)` and the root, nor, for a markerless (loose or lazy) root on a local FS, at the root itself. Each level is probed with the stage 2 marker list, or on a virtual FS a short list (below, ~0.1–0.2 ms per name). A workspace folder of this session at one of those levels (not the root) counts as a marker, as in the climb. The probe is not cached: every `open_for` probes again. So a `git clone` or new `.zk/` inside a loose or lazy root becomes its own root, and an `.mdroots` or `git init` in a loose or lazy root's own directory re-decides it at once. A markerless lazy root on a local FS whose parent climb finds a marker is also a miss: a loose search never runs inside a marker root, so the row is a workspace-folder decision written before (or predating) the `workspaceFolders` marker.

Otherwise it is a miss. After the first session a hit costs a few stats.

Also a miss: a rate verdict with fewer than two agreeing measurements (stage 4), and a budget verdict decided 7 or more days ago.

Other hit-path rules:

- A `.mdrootsignore` covering `dir(file)`, at any level up to the root, makes the file single-file on every filesystem.
- The virtual-FS probe checks 10 names per level: the explicit, notes-tool and VCS markers (`.git`, `.jj`, `.hg`, `.sl`), so a nested clone is found; a hit registers the new directory as a lazy root.
- On a local FS a new marker of any class is a miss, and stages 2–4 decide.

**Root moves.** Each row stores the marker's inode and a volume id, `dev:<st_dev hex>`, because `ATTR_VOL_UUID` (macOS) and `f_fsid` (Linux) need unsafe code. It is stable while the volume stays mounted, so move detection works within one boot or mount. On a miss where stage 2 finds a marker, a row with the same `(volume_id, marker_ino)` whose path no longer holds the marker is a move: update `path`, keep `root_id` and the DB file. Nothing is renamed on disk, and `files.path` is root-relative ([index spec](index.md)), so rows survive. A copy (both paths hold a marker) is a new root.

### Stage 2: `statfs`, then marker climb (no `readdir`)

**`statfs(dir(file))` first**, because a 10-level climb on Eden costs 10–30 ms. Virtual/remote (stage 3 test) → no climb; on Eden take the root from `readlink(dir(file)/.eden/root)` and go to stage 3. Local → climb.

**Climb** upward from `dir(file)`, `stat`ing ~15 names per level. Stop at the first of:

- a mount boundary (`st_dev` changes). Before stopping, `statfs` the mount point's parent and `stat` `<parent>/.eden`. If the parent is virtual, the file is in a local mount inside a virtual repo: **lazy** for a marker below the boundary, else **single-file**. Never walk it.
- `$HOME` (checked, never climbed past), or `/`.

| Class | Markers | Meaning |
|---|---|---|
| explicit | `.mdroots` (empty file), `.mdrootsignore` | "root here" / "never index here" |
| notes tool | `.zk/`, `.obsidian/`, `.marksman.toml`, `.iwe/` ([iwe](https://github.com/iwe-org/iwe)), `.foam/` | strong |
| docs tool | `mkdocs.yml` ([mkdocs](https://www.mkdocs.org)), `book.toml` ([mdBook](https://rust-lang.github.io/mdBook/)), `docusaurus.config.{js,ts,mjs,cjs}` ([Docusaurus](https://docusaurus.io)), `_config.yml` ([Jekyll](https://jekyllrb.com)), `hugo.toml` ([Hugo](https://gohugo.io)), `conf.py` + `index.md` | strong |
| VCS | `.git` (dir or file), `.jj`, `.hg`, `.sl` | medium |
| monorepo | `.eden/`, `.buckconfig` ([Buck2](https://buck2.build)), `WORKSPACE`/`MODULE.bazel` ([Bazel](https://bazel.build)) | tree is **huge** |
| editor | LSP `workspaceFolders` containing the file, when sent | strong |

Each marker costs one `lstat` and is typed (file or dir), so a `workspace/` dir does not match `WORKSPACE` on a case-insensitive volume. The nearest directory with any marker is the root. Within one directory the reported marker is the highest of explicit > notes tool > docs tool > editor > VCS > monorepo, and any monorepo marker there makes the tree huge. A `.mdrootsignore` stops the climb with no root. The nearest strong marker beats a farther VCS root (a `.zk/` notebook inside a git repo). A `.git` file (submodule, worktree) is a root of its own. Discovery must work from markers alone, because Neovim with `root_dir = nil` sends `workspaceFolders = null` and reused clients never get `didChangeWorkspaceFolders` ([library spec](library.md)).

### Stage 3: classify without walking

1. **Local or not**, by `statfs` on the candidate root:
   - `MNT_LOCAL` unset → virtual/remote.
   - Case-insensitive name prefix on the type and on `f_mntfromname` (so `edenfs:` matches); a match wins even with `MNT_LOCAL` set. Virtual: `edenfs`, `fuse`, `macfuse`, `osxfuse`, `virtiofs`, `9p`. Remote: `nfs`, `smbfs`, `afpfs`, `webdav`, `sshfs`, `cifs`, `smb3`. Kernel pseudo-filesystems (`proc`, `sysfs`, `devtmpfs`, `devfs`, `cgroup2`, `tracefs`, `rpc_pipefs` and the like, by exact type name) are flagged local but generated on read, so they count as virtual: never walked. Any other non-local mount is remote.
   - Linux has no `MNT_LOCAL`: parse `/proc/self/mountinfo`, take the mount whose mount point is the longest component-wise prefix of the canonical path (on a tie the later line wins, as an overmount does), and treat `nfs*`, `fuse`, `fuse.*`, `cifs`, `smb3`, `smbfs`, `9p`, `virtiofs`, `ceph` and `afs` as not local.
   - Cloud folders (`~/Library/CloudStorage/*`, `~/Library/Mobile Documents/*`) are `cloud`: walks allowed, `st_flags` checked per entry, rate check without relaxation.
2. `.eden/` at the root → virtual, whatever statfs says.
3. **Size estimate**, cheapest first: registry stats; the `.git/index` entry count (a `.git` file is followed to `<gitdir>/index`; colocated jj uses `.git/index`). A `sdir` sparse extension makes the count unreliable: index-driven, with the 200k cap applied while listing. A `link` split extension leaves most entries in another file, so the count is unknown: lazy. No index file, or an unreadable one, and non-colocated jj, hg or sl without Eden (no cheap count): budgeted walk.

| Condition | Mode |
|---|---|
| virtual/remote FS, Eden, or monorepo marker | **lazy** (§3): no walk, no watcher |
| Eden repo whose enumeration finishes in budget | **vcs-enumerated** (§3) |
| local mount inside a virtual repo | **lazy** if a marker is below the mount, else **single-file** |
| git root at `$HOME` or another denylisted dir (§2) | **tracked-only** (other VCS there: **single-file**) |
| git index > 200k entries or > 32 MB | **lazy** |
| git index split (`link` extension) | **lazy** |
| git index 20k–200k entries, or sparse (`sdir`) | **index-driven** |
| git index < 20k entries | **budgeted walk** (also finds untracked md) |
| no VCS | **loose root** search (§2), then budgeted walk |

**Index-driven** and **tracked-only** list `*.md`/`*.markdown`/`*.org` paths from the git index in-process, no `readdir`. A small hand parser of the [index format](https://git-scm.com/docs/index-format) replaces `gix-index` to fit the dependency budget: versions 2–4, SHA-1 only (a SHA-256 repo is unreadable and goes to the walk), at most 32 MiB read, every read bounds-checked so a corrupt index fails or yields a partial list, never a panic. The index's stat data is a snapshot, so each listed file is `stat`ed before it is trusted. Untracked md arrives via `didOpen`; these modes run no native watcher (§5). Planned: a background walk that adds untracked md when the tree proves small, never in tracked-only. Tracked-only exists so a dotfiles `$HOME/.git` does not make the home directory one root; it applies to a git root at any denylisted location (§2). A non-git VCS root there has no index: single-file.

### Stage 4: budgeted walk

Sequential breadth-first walk (shallow files are the likeliest link targets, so a partial result covers them) at background priority (§5), which the budgets are calibrated under and the caller sets. Sequential because the rate check times each `readdir` and every listing must go through the `Probe`; the `ignore` crate supplies only its gitignore matcher, since its walker would bypass the probe. The wall budget and cancel token are checked before every probe call, so an abort lands at most one call late.

Prune:

- ignore files, gitignore syntax, per directory in this order with later lines winning: `.gitignore`, `.ignore`, `.mdrootsignore`; plus the glob lines of `.hgignore` ([Mercurial](https://www.mercurial-scm.org)), read at the walk root only. As in git, the nearest directory's matcher with an opinion wins. A directory with an empty `.mdrootsignore` is never descended, even if it holds a marker;
- dot dirs and `node_modules`, `target`, `.venv`/`venv`, `__pycache__`, `dist`, `build`, `buck-out`, `bazel-out`, `.direnv`, `.cache`, `Pods`, `DerivedData`, `.next`, `vendor`;
- hidden and editor temp files (`.*`, `*~`, `#*#`, `*.swp`, `4913`), which would otherwise count as notes;
- directories with a different `st_dev` (Eden redirections are separate mounts);
- symlinked directories (not followed);
- dataless entries (`st_flags & SF_DATALESS`), because reading them triggers a download: files are recorded by path only and parsed when opened; directories are not descended;
- nested roots: a dir with a strong or VCS marker is recorded as its own root and not descended (§4).

| Budget (exceeding aborts) | Marker root | Loose root |
|---|---|---|
| entries visited | 200k | 10k |
| md files | 50k | 5k |
| wall time | 1.5 s | 300 ms |
| depth | 32 | 8 |

Depth counts from the walk root (its children are depth 1). Entries counts every listed entry, pruned or not.

**Rate check, per directory.** Entries/s depends on entries per directory (a local Documents folder measured 16–19k entries/s), so each `readdir` is timed instead. After the first 50 directories or 100 ms, whichever comes first, take the median ms/dir, but only once the walk knows at least 50 directories (listed or queued). A walk that finishes sooner, or a root with fewer than 50 directories, is never rate-aborted; the wall budget bounds it, because a few slow `readdir`s on a loaded machine say nothing about the filesystem. The check runs once, skipped if fewer than 5 listings were timed. Above the threshold the FS is slow (network, FUSE, cold disk, cloud): abort and go lazy. Threshold: 5 ms/dir (`DiscoverOptions::rate_ms_per_dir` overrides it). Warm-cache walks measure ~0.16–0.22 ms/dir (§0); not calibrated on a cold cache (after `sudo purge`), a cloud folder or EdenFS.

**Recording.** Abort or success records `{entries_seen, md_seen, dirs_seen, ms, ms_per_dir, reason}` in the registry, so the next start goes straight to reconcile. A lazy verdict from the rate check **alone** is saved only once a second measurement (later or by another session) agrees, so one slow walk during a herd start or backup does not stick: the first rate abort registers a pending row (`lazy pending: rate 7.1 ms/dir, 1 of 2 measurements`, one confirmation) that stage 1 treats as a miss; the next walk either succeeds and replaces it or aborts on rate and confirms it (2 of 2). Other lazy verdicts (virtual FS, Eden, budgets) are saved at once. A cancelled walk is never registered. The full walk is retried at most once per 7 days, or on `fs_type`/`st_dev` change.

## 2. Loose roots (no VCS or marker)

1. **Denylist**, compared lexically (no symlink resolution). Exact: `/`, `$HOME`, `/tmp`, `/private`, `/private/tmp`, `/var`, `/private/var`, a `/Volumes/<name>` volume root, `~/Downloads`, `~/Desktop` (so `/var/folders/...` temp dirs are allowed). Prefix: `~/Library` and everything under it, except inside an [Obsidian](https://obsidian.md) vault under `~/Library/Mobile Documents/iCloud~md~obsidian/Documents/<vault>`. Also denied: any virtual/remote FS, a local mount inside a virtual repo, a directory whose mount cannot be read. Denied → **single-file mode**: the current buffer plus its relative links resolved by `stat`.
2. **Lower bound from the buffer's links**: every existing relative link target directory must be inside the root. The file path and link dirs are cleaned lexically first (`.` dropped, `..` applied), so the denylist sees the real target.
3. **Grow upward.** Start at the deepest common ancestor of `dir(file)` and the existing link target dirs (accepted unless denied) and walk it with the loose budget. If that walk aborts, the start dir becomes a **lazy** root (verdict `budget`; a rate abort follows the §1 stage 4 two-measurement rule) and nothing grows. A start dir outside `$HOME`, or with no `$HOME`, never grows. Otherwise climb toward `$HOME` (never reaching it), walking each parent with the child's subtree skipped and its counts reused, within the remaining loose budget (wall time counted from the start of the search). Growth stops at a mount boundary: a parent on another `st_dev` is never walked, since its walk would skip the child's subtree on the other mount. Accept the parent only if it is not denied, its walk does not abort, the child's deepest directory stays within the loose depth budget below it, and either:
   - **(a)** its subtree has ≥ 20 md files and md density (`md_files / files`) ≥ 30%, or
   - **(b)** it adds more md files outside the child's subtree than the child holds.

   Example: a `scratch/` dir (10 files, 2 md, 6 nested repos pruned) under a projects folder (506 files, 4 md) is not grown: (a) fails at 4 md, (b) because 2 added is not more than 2. Still rejected without the tarball tree (16 files, 3 md). Notes folders are usually > 50% md.
4. The highest accepted ancestor is the loose root, registered with its reason (`loose root accepted at <dir>: <md> md / <files> files`, or the line of the first rejection). It is walked once more with the loose budget for its md list; over that budget (more than 5k md or 10k entries) it is lazy. To index a larger notes folder fully, add an empty `.mdroots`, making it a marker root with the marker budget.
5. **Hysteresis** (not applied yet; needs the reconcile's file counts): re-run the climb only if the recorded stats are > 7 days old or the reconcile sees the file count change > 2×, so roots do not flip around a threshold and rebuild.

The 20-file, 30%, 2× and 7-day numbers are validated only on the §7 fixtures ([ROADMAP §1](../ROADMAP.md#1-built-but-not-validated)). Results are staged: single-file features at once, then results published after each accepted level.

## 3. Lazy and vcs-enumerated modes

Lazy mode never enumerates the tree:

- **Working-set index**: each opened file's directory is listed one level (cap 2k entries, dataless skipped) and its md files indexed (the reconciler writes them to the DB; a peer parses them into memory only).
- **Links by `stat`**: relative and root-relative paths, each with and without `.md`. On Eden a stat is a metadata lookup, no content fetch.
- **`[[stem]]`**: working-set index only. Possible extension ([ROADMAP](../ROADMAP.md)), to measure first: ask the VCS with a 500 ms timeout (`git ls-files '*foo.md'`, Eden's glob API, or `sl files 'glob:**/foo.md'`).
- **No native watcher.** A process re-checks working-set files when its own editor opens or saves them. The reconciler point-checks and writes files from its own editor's saves and from the client's `didChangeWatchedFiles`. Another peer's save reaches the DB only when that peer later becomes reconciler, or the reconciler's own editor opens or saves that file.
- **Diagnostics** ([index spec](index.md)): only `stat`-checkable links are diagnosed; an unresolved `[[stem]]` is a hint ("not in indexed set"), never an error.

**vcs-enumerated** (small Eden repos, where lazy would mean `[[stem]]` never resolves): ask Eden once for `**/*.md` via its glob API, falling back to `sl files 'glob:**/*.md'`, killed after 500 ms or 20k paths. In budget → the path list feeds stem resolution and the reconcile queue as in index-driven mode, parsed at background priority, open and linked files first. Over budget → stays lazy, recorded (verdict `budget`), not retried for 7 days. No watcher either way.

The enumeration is an `Enumerator` trait: `SlFiles` runs `sl files 'glob:**/*.md'` (the glob API is not used yet); `NoEnumerator` turns the mode off. `discover()` runs it synchronously within the 500 ms budget; moving it to the background is the index layer's job. Only a virtual FS whose `.eden/root` resolves, and whose checkout root has no monorepo marker file (`.buckconfig`, `WORKSPACE`, `MODULE.bazel`), is offered to the enumerator. A remote FS with `.eden` is lazy; a virtual or remote FS without `.eden/root` is lazy with no root and not registered.

## 4. Nested roots

**Every file belongs to exactly one root**, its nearest marker ancestor. A root's scope is its subtree minus nested roots' subtrees (as git treats nested repos).

```
nb/              .zk    root A  scope = nb − nb/proj
nb/proj/         .git   root B  scope = nb/proj
nb/proj/docs/    —      → B
```

- Walks prune at nested markers, so DBs are disjoint and one root's GC never touches another.
- **The registry rejects overlapping inserts**, except a nested root at a marker (nearest wins; a workspace-folder root counts as one). A loose root never contains a marker root, and loose roots never nest; a new marker root may appear inside a loose root.
  - Discovery removes the rejecting row and inserts again if that row is invalid (marker gone, or `st_dev` or `fs_type` changed), is a workspace-folder row whose folder is not a workspace folder of this session, or, for a new root with a real marker, is a markerless row strictly inside it on the same device (a legacy workspace-folder row: a loose search never runs inside a marker; below a mount the climb never reaches that marker).
  - After registering (inserting or updating) a root with a real marker, discovery also removes markerless rows strictly inside it on the same device that never overlapped (a nestable legacy workspace-folder row, or one below an already-registered root). Rows with a marker are kept: nested roots, workspace-folder rows recorded with the `workspaceFolders` marker, and a moved root's row, which move detection needs until its new path is opened.
  - If a valid row rejects the insert, discovery uses the nearest valid registered root containing the file (as stage 1 picks it), or returns its decision unregistered with `(not registered: overlaps <dir>)`.
- **Cross-root links** resolve only by `stat` (relative paths), as Unindexed. Completion stays in the current root. Planned ([ROADMAP](../ROADMAP.md)): on a miss in the current scope, find the target's root in the registry and `ATTACH` its DB read-only (LRU, SQLite's default limit is 10).
- **New marker** (e.g. `git init` in a loose root): the stage 1 probe finds it on the next `open_for` of a file below it, which registers and indexes the new root in its own DB. The parent drops the subtree's rows on its next re-list (open or `refresh`), because the walk prunes at the new marker; a lazy parent keeps them. No rows move between DBs: each root parses its own files.
- **Marker removed**: the nested row fails the stage 1 marker check, so its files go to the parent row (no discovery walk), whose next re-list walks into the directory and indexes them. The stale row stays until it blocks an insert (then it is removed) or goes unseen for 30 days, when GC removes it with its orphan DB (§6).
- Moving rows between root DBs instead of re-parsing is planned ([ROADMAP](../ROADMAP.md)).
- **Editor folder vs markers**: a file in `nb/proj` belongs to B even if the client's workspace folder is `nb`. The process just opens a second DB.

## 5. Many processes on one root

Typical: tmux restoring many nvim instances at once. No daemon: each process runs mdroots in-process, coordinates only through the filesystem, and each root has one writer (D3 in [DECISIONS.md](../DECISIONS.md)). What the DB holds and how queries are served: D9 and [index §1](index.md#1-index-under-churn).

Built, except what is marked planned ([ROADMAP](../ROADMAP.md)).

### Files

Base dir: D5 (the chain and why it must be local; selection in [index §1.7](index.md#17-files-and-connections)). A registry that cannot be opened also leaves the session in memory (planned: also a full disk at write time). `Options::cache_dir` (the CLI's `MDROOTS_CACHE_DIR`) replaces the chain. Processes with different base dirs (e.g. `XDG_CACHE_HOME` set in some shells only) get separate reconcilers; `mdroots roots` prints the root's DB path (`cache:`) and this process's role (`role: reconciler|peer`, or `cache: memory`, `role: none`).

`<db>` = `<id>.v<schema>` (`<id>.v2` at schema 2), so each schema version has its own file and locks. One lock scope per root and schema: `<db>.lock` and `<db>.open` cover every generation file of `<db>`.

| Path | Purpose |
|---|---|
| `roots.v1.db` | registry (SQLite, WAL), versioned name |
| `discover.lock` | global flock (`DiscoverLock`, via `std::fs::File::lock`); `Workspace::open_for` holds it around the whole of `discover()` with a cache, so one process discovers at a time |
| `roots/<db>.db` | per-root index (SQLite, WAL) |
| `roots/<db>.lock` | reconciler flock; never deleted |
| `roots/<db>.open` | `LOCK_SH` held by every process with any `<db>*.db` open; never deleted |
| `roots/<db>-<gen8>.db` | a new generation after a corruption rebuild; `gen8` is the first 8 hex digits of its `meta.generation` |

The registry row's `db_file` names the current DB file: `<db>.db`, or the generation file after a rebuild. A process opens that file if it belongs to this root and schema, else `<db>.db`, which it then records. `root_id` (a hash of the first path and the decision time) never changes, even when the root moves.

### Roles

| Topic | Rule |
|---|---|
| Reconciler | holds `flock(LOCK_EX\|LOCK_NB)` on `<db>.lock`; one per root and schema. Sole writer: lists the root, reconciles and writes on open, `refresh` and `refresh_paths`, keeps the FTS table in sync; the only process that may run the native watcher. Each transaction `BEGIN IMMEDIATE`, `busy_timeout = 2s`. Runs `quick_check` on the DB it opens or takes over. Planned: convention inference |
| Peers | never write or list the root. On open and `refresh` they read the DB rows, re-stat those files and read changed ones into memory only. On `refresh` and `refresh_paths` they re-read the registry row and reopen if `db_file` names another file (a rebuild). Planned: a per-request check of `PRAGMA data_version` (~1.2 µs) and the DB inode, to catch deletion and cache purges between refreshes |
| Peer saves | the editor's text is the peer's overlay; on `didSave` the peer refreshes, re-reading changed files itself. The DB gets the change via the reconciler's watcher (if any) or its next refresh |
| Open buffers | per process, laid over the index; never written before save, never by a peer. Two editors see their own unsaved text |
| `SQLITE_BUSY` | `busy_timeout` 2 s on every connection. Planned: the reconciler runs `wal_checkpoint(PASSIVE)` periodically and sets `journal_size_limit`, because a leaked reader blocks checkpoints |
| Takeover | the kernel releases the flock on exit or crash; a peer retries `LOCK_EX\|LOCK_NB` on every `refresh` and, if free, becomes the reconciler and starts its watcher. No heartbeat, PID check or stale-lock cleanup. Planned: a 5 s retry timer |
| Stuck reconciler | alive but stuck (SIGSTOP, hung `stat` on Eden/NFS) keeps the lock; peers still open and answer from the DB plus their own reads, without waiting. Planned: a peer seeing a stale `reconciled_at` logs one `window/logMessage` warning |

### File events

`mdroots lsp` turns on the native watcher (`Options::watch(true)`; libraries default to off). Debounce, filtering, point refresh, client `didChangeWatchedFiles` handling, offline changes and planned FSEvents replay: [index §1.3](index.md#13-finding-what-changed).

- **Only the reconciler watches**, and only a DB-backed marker, VCS or loose root on a local filesystem: never a lazy, single-file, tracked-only, index-driven or vcs-enumerated root, nor a virtual, remote or cloud filesystem (so never EdenFS). Peers act only on their own `didOpen`/`didChange`/`didSave`, in overlays; otherwise N editors would mean N parses per save.
- Non-watching workspaces (peers, lazy roots, the other non-watching modes) refresh on the client's `didChangeWatchedFiles` (the server registers for it when the client offers dynamic registration, as Neovim does on macOS and Windows); a peer's refresh writes nothing. `didSave` refreshes the saved document's workspace in every process.
- **Offline changes and takeover.** A file changed while no reconciler watched (no mdroots running, or between a reconciler's exit and a peer's promotion) is caught by the next re-list and re-stat: on open, on `refresh` (every `didSave`), or on promotion.

### DB lifetime: never unlink an open DB

Unlinking or renaming an open SQLite file is a documented corruption path.

- Every process holds `LOCK_SH` on `<db>.open` while any `<db>*.db` is open.
- GC takes `LOCK_EX|LOCK_NB` on `<db>.open`, skips the root on failure, and deletes DB files with their `-wal` and `-shm`, never lock files.
- **Corruption**: the reconciler never renames or unlinks a corrupt DB; it builds `<db>-<gen8>.db` and repoints `db_file` after its first sync, and GC deletes the old file once nobody holds the root open (fixture 15). Steps: [index §1.6](index.md#16-failures-and-races).
- `<db>.lock` and `<db>.open` are never deleted, even when a whole root is GC-ed: they are tiny, and a waiter on an unlinked lock would elect a second reconciler. So a process need not re-check the locked inode.
- The registry is never deleted automatically; an old `roots.v<k-1>.db` is a few KB.

### Thundering herd

- **Discovery is serialised** under `discover.lock` (the per-root flock cannot help before the root is known), which is also the global walk semaphore: one discovery walk per user at a time. A process that does not get it blocks, then finds the row in the registry. `mdroots lsp` waits on its opener thread and serves the opened file single-file meanwhile ([library §3.6](library.md#36-embedding-the-server)); a one-shot command just waits.
- One process wins the root flock; the others serve at once from the index plus overlays. None waits on a write lock, because none writes.
- Only the flock holder creates a new root's DB (`CREATE ... IF NOT EXISTS` in `BEGIN IMMEDIATE`), indexing at once inside `open_for`. A peer that opens before the DB has rows indexes in memory from discovery's listing and writes nothing. Peers publish a document's diagnostics once it and its targets are checked locally.
- Rate-only lazy verdicts need two measurements (§1 stage 4), so herd slowness does not stick.

### Versions, memory, scheduling

- **Upgrades**: the schema version is in the DB filename and locks, so v1 and v2 processes never fight over migrations. Files of another schema are GC-ed once the newest is 7 days old by mtime and `<id>.v<old>.open` can be taken exclusively. The registry carries its own `k` for the same reason (still `roots.v1.db`).
- **Memory**: each process holds every note's bytes and parse in memory (D9); target and measurements: D3 and [ROADMAP](../ROADMAP.md); the [derived tables](index.md#derived-tables) would bound it for larger vaults. SQLite is `mmap`ed, so N processes share one page-cache copy; on macOS those pages show in every process's RSS, so measure with `footprint` or `proc_pid_rusage` (`ri_phys_footprint`), not RSS.
- **Scheduling** (planned, with a background thread): background work (walks, sweeps, FTS, inference, vcs-enumerated parsing) at `QOS_CLASS_BACKGROUND` (E-cores only); `QOS_CLASS_UTILITY` (prefers P-cores) only for the small link-target queue of open buffers; requests at default QoS. Linux: `nice 10` + `IOPRIO_CLASS_IDLE` for background, best-effort for the link-target queue.

## 6. Housekeeping

Registry table (`roots.v1.db`, `SqliteRegistry`; one row per `RootRecord`):

| Column | Holds |
|---|---|
| `root_id` | primary key; stable across moves |
| `path` | canonical root path, unique |
| `mode` | `marker`, `vcs`, `tracked`, `index-driven`, `loose`, `lazy`, `vcs-enumerated` (single-file decisions are never registered) |
| `marker` | the marker that decided the root |
| `marker_ino`, `volume_id` | move detection (§1 stage 1); `volume_id` is `dev:<st_dev hex>` |
| `dev`, `fs_type` | re-decide when either changes |
| `stats` | walk counts as one text value: entries, md, files, dirs, ms, ms/dir, max depth |
| `verdict_source` | `fs`, `eden`, `budget`, `rate`, `user` |
| `rate_confirmations` | the §1 stage 4 two-measurement rule |
| `decided_at_ms`, `last_seen_ms` | when the row was decided; when a process last opened or refreshed the root (stamped at most hourly), for GC |
| `reason` | the one-line explanation |
| `db_file` | the per-root DB's file name: `<root_id>.v2.db`, or `<root_id>.v2-<gen8>.db` after a rebuild |

plus `meta(key, value)` with the registry schema and `gc_at` (last GC run). Rows that do not decode (an unknown mode from a newer binary) are skipped; SQLite read errors count as a miss, since the registry is a cache. Overlap checks and inserts share one `BEGIN IMMEDIATE`, so two processes inserting the same root get one row.

- **GC** (`mdroots_index::gc`): at most daily, claimed through `meta.gc_at`; each deletion needs `LOCK_EX|LOCK_NB` on `<db>.open` (fixture 14). Candidates and rules: [index §1.7](index.md#17-files-and-connections).
- **OS cache purge** (macOS clears `~/Library/Caches` under disk pressure; cleaners) counts as deletion: the next process recreates the cache and becomes the reconciler; without a registry, discovery reruns under `discover.lock`. A running process keeps serving from memory (fixture 16); reopening on a purge before the next refresh is planned.
- **`mdroots roots PATH`** prints the root chosen for PATH with mode, indexed file count, DB path, this process's role, and why, e.g. "lazy: statfs edenfs:, MNT_LOCAL unset", "single-file: inside a local mount in a virtual repo", "loose root rejected at <dir>: 4 md (< 20), adds 2 md (<= 2 in child)", "lazy pending: rate 7.1 ms/dir, 1 of 2 measurements". `explain()` returns this line; `cargo run -p mdroots-roots --example roots -- <file>` prints it with the `readdir` count. In the editor, `mdroots.info` (`:MdrootsInfo` in Neovim) shows root, mode, reason and file count via `window/showMessage`, the main way to debug a wrong root.
- **Escape hatches**: empty `.mdroots` forces a root; `.mdrootsignore` excludes paths and stops loose climbing (empty: never index here). A variable forcing lazy mode per path is an idea in [ROADMAP](../ROADMAP.md).

## 7. Fixtures

"Zero readdir" is checked with an instrumented walker or `fs_usage -f filesys`. Timings logged.

- **Discovery, 1–11**: `crates/mdroots-roots/tests/fixtures.rs`. 1–4 and 9–11 build synthetic trees in a temp dir over `StdProbe`, wrapped in a probe that pins `home()` to the temp dir (setting `$HOME` would need unsafe code), so growth rules apply and no climb leaves the temp dir; `Counting` checks every `readdir`. 5–8 need a virtual FS or cloud folder and run on `FakeProbe`. No test touches a real home directory, network mount or virtual FS. Each asserts its `discover()` call takes under one second.
- **Many processes, 12–18**: `crates/mdroots-cli/tests/many_process.rs`, with a temporary cache dir. Real `mdroots` processes run on a copy of `tests/corpus/zk-min` in a temp dir, with `MDROOTS_CACHE_DIR`, `XDG_CACHE_HOME` and `HOME` pointed into it. Hidden `mdroots __open PATH --hold-ms N [--refresh-every MS]` opens a workspace, prints its role, file count and DB file, holds it open, and reprints them after each refresh; hidden `mdroots __gc --now-ms N --force` runs a forced GC on that cache dir. They run no watcher (only `mdroots lsp` does); the watcher is tested in-process in `crates/mdroots/tests/watch.rs`, on temp dirs only.

Each many-process item gives the target, then what is **Built** (as stated) or **Built, adapted** (narrower).

**Discovery**

1. A file in a no-VCS `scratch/` dir inside a projects folder of ~40 repos: loose root `scratch/`, climb rejected by (a) and (b); also on a synthetic copy without the tarball tree (16 files, 3 md). A file inside any repo gets that repo.
2. A mid-size git checkout (3.8k files, 13 md, the shape of a Neovim checkout): budgeted walk.
3. Synthetic notes folder, no VCS, 4,000 md among 6,000 files: loose root accepted by (a). A larger one (6,000 md) exceeds the loose md budget: lazy, verdict `budget`.
4. A `.zk` notebook inside a git repo.
5. A file in a large virtual-filesystem monorepo (Eden-shaped fake): lazy from `statfs` before any marker stat, root from `readlink(.eden/root)`, zero readdir outside the file's dir.
6. A file in a build-output mount inside that checkout: lazy or single-file, zero readdir outside the file's dir, no registry row at the mount.
7. A small notes repo on EdenFS: `vcs-enumerated` within 500 ms, zero readdir outside opened dirs (the facade indexing the enumerated files: `crates/mdroots/tests/workspace.rs`; a `[[stem]]` resolving across them is not tested separately).
8. A dataless cloud file (iCloud after `brctl evict`, or Drive online-only) in a loose root: recorded as dataless, not read during discovery, still `SF_DATALESS` afterwards; link existence by `stat`. Opening a dataless file reads it (`crates/mdroots/tests/workspace.rs`, on a lazy root).
9. A dotfiles repo at a temp `HOME` with an untracked tree: `tracked-only`, zero walk of it.
10. `git clone` into an existing lazy root, open a file in it: the stage 1 probe registers a nested root.
11. `mv notes notes2` between sessions: row re-keyed by marker inode + volume id, no cold rebuild.

**Many processes**

12. 10 processes on fixture 3 with an existing DB: one reconciler and writer; peers never open a writer connection; no `SQLITE_BUSY` reaches requests; all converge on one `data_version`; a save shows in other editors within watcher latency. **Built, adapted**: 10 at once on an existing DB: exactly one reconciler, the rest peers, all serve the same files, none errors.
13. Cold herd (no registry, no DB, `sudo purge`), 10 processes on a loose root: one discovery walk, one registry row, no overlaps, no rate-only lazy verdict saved; every `phys_footprint` < 35 MB. **Built, adapted**: 10 at once on a marker root with no registry or DB: one reconciler, exactly one registry row. Footprint measured by hand (D9).
14. GC with 3 peers holding an aged DB open: skipped, peers keep answering, `integrity_check` passes; repeat for an old schema file and a stale generation. **Built**: 3 processes hold an aged root open, refreshing every 100 ms; a forced GC 40 days later skips its DB and they keep serving the same files from an intact DB; after they exit, GC deletes the DB and registry row. Old-schema and stale-generation files: in-process in `crates/mdroots-index/tests/gc.rs`.
15. Corrupt the DB under 3 peers: new generation, peers reopen, old file deleted only after the last peer exits. **Built, adapted**: the processes stop before the header is overwritten (with a connection open, `quick_check` misses an overwritten header), then 3 new ones start: one reconciler builds `<db>-<gen8>.db`, all serve the same files, peers reopen the recorded file on refresh, the corrupt file stays while they run, and a GC after they exit deletes it.
16. `rm -rf` the base dir under 3 processes: all reopen, one rebuilds, nobody writes to an unlinked file. **Built**: the cache dir is deleted while a process holds the DB open; the next process recreates it and becomes the reconciler with the same files; the holder exits cleanly (reopening is planned).
17. mtime-regressing copy (`cp -p`, `rsync -a`, `tar x` of an older version): new content indexed (ctime advanced); a bare `touch` does not re-parse links. **Built**: same size, older mtime restored, new link target: the next `check` reports the new content. `touch`: `crates/mdroots-index/tests/reconcile.rs`.
18. `kill -STOP` the reconciler, save in a peer: one warning, peer fresh from its overlay, no DB writes. `kill -CONT`: reconciler picks up the save. `kill -9`: a peer takes over and re-checks its working set. **Built, adapted**: with the reconciler stopped, a new process opens as a peer within 5 s; after the reconciler is killed, the next process becomes the reconciler with the same files. The warning is planned.
