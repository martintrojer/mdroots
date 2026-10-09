# Spec: root discovery, size budgets, and many instances

Related: [index spec](index.md), [library spec](library.md), D3, D4, D5 in [DECISIONS.md](../DECISIONS.md).

Hard rule: **mdroots never calls `readdir` on a tree before it has established that the tree is local and bounded.** A monorepo on a virtual filesystem such as [EdenFS](https://github.com/facebook/sapling) (the virtual filesystem from the [Sapling](https://sapling-scm.com/) project) can hold millions of files fetched on demand; a recursive walk there takes hours. Every rule below exists to make that walk impossible.

Implementation: the `mdroots-roots` crate implements §1–§4, the §6 data model and fixtures 1–11 of §7. Every discovery filesystem call goes through its `Probe` trait (`StdProbe` over `std::fs` and `statfs`, `FakeProbe` in memory, `Counting` to count and forbid `read_dir`), so tests can prove the hard rule. Two things sit outside `Probe`: `DiscoverLock` opens `discover.lock` with `std::fs`, and the `SlFiles` enumerator runs `sl files` in a child process. `discover()` takes a canonical absolute file path; canonicalising (including `F_GETPATH` on macOS) is the caller's job. The registry is a trait with an in-memory `MemRegistry`. `list_root` re-lists a known root per mode (budgeted walk, [git](https://git-scm.com) index scan, enumerator; `None` for lazy and single-file roots, or on abort or over budget). `mdroots-index` adds the [SQLite](https://sqlite.org) registry (`SqliteRegistry`), the cache dir choice and the flock roles of §5, and the per-root DB; the facade takes `discover.lock` around discovery. §5 and §7 say which parts are built.

## 0. Measurements (M-series Mac, APFS)

| What | Result |
|---|---|
| Unpruned walk of a projects folder (`rg --files --no-ignore --hidden`) | 662k entries, 1.64 s (~400k entries/s) |
| Same, honouring ignore files, `*.md` only | 1,023 md, 131 ms |
| Same, pruned at nested repos, dot dirs, `node_modules`/`target` | 506 files, 4 md, < 70 ms (490 files and 1 md are one unpacked source tarball) |
| `git ls-files` (twice) in a 3.8k-file repo (neovim) | 90 ms, mostly process spawns |
| `.git/index` header (12 bytes) | tracked entry count, O(1) |
| `statfs(<eden checkout>)` | `f_fstypename = "edenfs:"` (trailing colon), `MNT_LOCAL` unset |
| `statfs(<build-output dir inside eden checkout>)` | `apfs`, `MNT_LOCAL` set, own `st_dev`: a local mount inside the virtual repo. Several such redirections can exist per checkout |
| Small notes repo on EdenFS | also `edenfs:`, `MNT_LOCAL` unset |
| `<eden checkout>/.eden/` | present at every depth; `readlink(<dir>/.eden/root)` gives the checkout root in one call |
| 23 `stat`s of missing names | ~2–5 ms on Eden, ~0.06 ms on APFS |
| `statfs` on a Google Drive folder | `apfs`, `MNT_LOCAL` set, same `st_dev` as `$HOME`: statfs cannot see cloud folders |
| `readdir` per directory, warm cache | median 0.19–0.22 ms/dir |
| `mdroots-roots` budgeted walk of this repo (37 dirs, 163 entries; debug build, warm cache) | 11–40 ms, median ~0.16 ms/dir |
| Loose decision for a 30-note temp folder (debug build) | ~2 ms |
| One file in a large EdenFS checkout (debug build) | lazy after 6 `stat`/`statfs` calls, 0 `readdir`, 1.4 ms |
| Rust walker on a cold cache, a cloud folder or EdenFS | not measured |

Consequences: pruning makes walks ~12× cheaper, so it is required. Big or virtual trees are detectable with a few `stat`/`statfs` calls; the test is `MNT_LOCAL`, not the type name, and it must be repeated at mount boundaries inside a virtual tree. Cloud folders are caught by path, `SF_DATALESS` and walk speed.

## 1. Discovery stages, cheap to expensive

Each stage may stop with a decision. Only stage 4 calls `readdir` recursively. Stages 2–4 run under the global `discover.lock` (§5).

### Stage 1: registry lookup

Longest-prefix match of `realpath(file)` in the registry (`roots.v<k>.db`). On macOS also canonicalise with `F_GETPATH`, because APFS is case-insensitive. A hit is used only if:

1. the recorded marker still exists (one `stat`);
2. the root's `st_dev` and `fs_type` match the recorded values (else re-decide from stage 2, lazy verdicts included);
3. no new marker sits between `dir(file)` and the root. Probe each level with the stage 2 marker list; on a virtual FS probe a short list instead (below, ~0.1–0.2 ms per name). Caching the probe per directory per session is not done yet: every `open_for` probes again. This is how a `git clone` or new `.zk/` inside a loose or lazy root becomes its own root.

Otherwise treat it as a miss. After the first session the hit path is a few stats.

Also a miss: a rate verdict with fewer than two agreeing measurements (stage 4), and a budget verdict decided 7 or more days ago. A `.mdrootsignore` that covers `dir(file)`, at any level from `dir(file)` up to the root, makes the file single-file on every filesystem. The implementation's virtual-FS probe checks 10 names per level: the explicit and notes-tool markers plus the VCS markers (`.git`, `.jj`, `.hg`, `.sl`), so a nested clone is found; a hit there registers the new directory as a lazy root. On a local FS a new marker of any class is a miss, and stages 2–4 decide.

**Root moves.** Each row stores the marker's inode and a volume id. The implementation's volume id is `dev:<st_dev hex>`, because `ATTR_VOL_UUID` (macOS) and `f_fsid` (Linux) are not reachable without unsafe code; it is stable while the volume stays mounted, so move detection works within one boot or mount. On a miss where stage 2 finds a marker, a row with the same `(volume_uuid, marker_ino)` whose path no longer holds the marker is a move: update `path`, keep `root_id` and the DB file. Nothing is renamed on disk, and `files.path` is root-relative ([index spec](index.md)), so rows survive. A copy (both paths hold a marker) is a new root.

### Stage 2: `statfs`, then marker climb (no `readdir`)

**`statfs(dir(file))` first**, because a 10-level climb on Eden costs 10–30 ms. Virtual/remote (stage 3 test) → no climb; on Eden take the root from `readlink(dir(file)/.eden/root)` and go to stage 3. Local → climb.

**Climb** upward from `dir(file)`, `stat`ing ~15 names per level. Stop at the first of:

- a mount boundary (`st_dev` changes). Before stopping, `statfs` the mount point's parent and `stat` `<parent>/.eden`. If the parent is virtual, the file is in a local mount inside a virtual repo: **lazy** for a marker below the boundary, else **single-file**. Never walk it.
- `$HOME` (checked, never climbed past), or `/`.

| Class | Markers | Meaning |
|---|---|---|
| explicit | `.mdroots` (empty file), `.mdrootsignore` | "root here" / "never index here" |
| notes tool | `.zk/`, `.obsidian/`, `.marksman.toml`, `.iwe/`, `.foam/` | strong |
| docs tool | `mkdocs.yml`, `book.toml`, `docusaurus.config.{js,ts,mjs,cjs}`, `_config.yml`, `hugo.toml`, `conf.py` + `index.md` | strong |
| VCS | `.git` (dir or file), `.jj`, `.hg`, `.sl` | medium |
| monorepo | `.eden/`, `.buckconfig` ([Buck2](https://buck2.build)), `WORKSPACE`/`MODULE.bazel` ([Bazel](https://bazel.build)) | tree is **huge** |
| editor | LSP `workspaceFolders` containing the file, when sent | strong |

Each marker costs one `lstat` and is typed (file or dir), so a `workspace/` dir does not match `WORKSPACE` on a case-insensitive volume. The nearest directory with any marker is the root; within one directory the reported marker is the highest of explicit > notes tool > docs tool > editor > VCS > monorepo, and any monorepo marker there makes the tree huge. A `.mdrootsignore` stops the climb with no root. The nearest strong marker beats a farther VCS root (a `.zk/` notebook inside a [git](https://git-scm.com) repo). A `.git` file (submodule, worktree) is a root of its own. Discovery must work from markers alone, because [Neovim](https://neovim.io) with `root_dir = nil` sends `workspaceFolders = null` and reused clients never get `didChangeWorkspaceFolders` (see [library spec](library.md)).

### Stage 3: classify without walking

1. **Local or not**, by `statfs` on the candidate root:
   - `MNT_LOCAL` unset → virtual/remote.
   - By name prefix, case-insensitive, on the type and on `f_mntfromname` (so `edenfs:` matches); a match wins even when `MNT_LOCAL` is set. Virtual: `edenfs`, `fuse`, `macfuse`, `osxfuse`, `virtiofs`, `9p`. Remote: `nfs`, `smbfs`, `afpfs`, `webdav`, `sshfs`, `cifs`, `smb3`. Any other non-local mount is remote.
   - Linux has no `MNT_LOCAL`: the implementation parses `/proc/self/mountinfo`, takes the mount whose mount point is the longest component-wise prefix of the canonical path (the later line wins a tie, as an overmount does), and treats `nfs*`, `fuse`, `fuse.*`, `cifs`, `smb3`, `smbfs`, `9p`, `virtiofs`, `ceph` and `afs` as not local.
   - Cloud folders (`~/Library/CloudStorage/*`, `~/Library/Mobile Documents/*`) are marked `cloud`: walks allowed, `st_flags` checked per entry, rate check without relaxation.
2. `.eden/` at the root → virtual, whatever statfs says.
3. **Size estimate**, cheapest first: registry stats; the `.git/index` entry count (a `.git` file is followed to `<gitdir>/index`); colocated jj uses `.git/index`. A `sdir` sparse extension makes the count unreliable: index-driven, with the 200k cap applied while listing. A `link` split extension leaves most entries in another file, so the count is unknown: lazy. No index file, or an unreadable one: budgeted walk. Non-colocated jj, hg, sl without Eden: no cheap count, go to the budgeted walk.

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

**Index-driven** and **tracked-only** list `*.md`/`*.markdown`/`*.org` paths from the git index in-process, no `readdir`. The implementation uses a small hand parser of the [index format](https://git-scm.com/docs/index-format) instead of `gix-index`, to stay within its dependency budget: versions 2–4, SHA-1 only (a SHA-256 repo is unreadable and goes to the walk), at most 32 MiB read, every read bounds-checked so a corrupt index fails or yields a partial list, never a panic. The index's stat data is a snapshot, so each listed file is `stat`ed before it is trusted. Untracked md arrives via `didOpen`; these modes run no native watcher (§5). A later background walk that adds it when the tree proves small is planned, never in tracked-only. Tracked-only exists so a dotfiles `$HOME/.git` does not make the home directory one root; it applies to a git root at any denylisted location (§2). A non-git VCS root at a denylisted location has no index to read: single-file.

### Stage 4: budgeted walk

Sequential breadth-first walk (shallow files are the likeliest link targets, so a partial result covers them), at background priority (§5); budgets are calibrated under that priority. Sequential because the rate check times each `readdir`, and because every listing must go through the `Probe`; the `ignore` crate is used only for its gitignore matcher, since its walker would bypass the probe. Setting background priority is the caller's job. The wall budget and the cancel token are checked before every probe call, so an abort lands at most one call late.

Prune:

- ignore files, gitignore syntax, per directory in this order with later lines winning: `.gitignore`, `.ignore`, `.mdrootsignore`; plus the glob lines of `.hgignore` ([Mercurial](https://www.mercurial-scm.org)), read at the walk root only. The nearest directory's matcher with an opinion wins, as in git. A directory with an empty `.mdrootsignore` is never descended, even if it holds a marker;
- dot dirs and `node_modules`, `target`, `.venv`/`venv`, `__pycache__`, `dist`, `build`, `buck-out`, `bazel-out`, `.direnv`, `.cache`, `Pods`, `DerivedData`, `.next`, `vendor`;
- hidden and editor temp files (`.*`, `*~`, `#*#`, `*.swp`, `4913`), which otherwise get counted as notes;
- directories with a different `st_dev` (Eden redirections are separate mounts);
- symlinked directories (not followed);
- dataless entries (`st_flags & SF_DATALESS`), because reading them triggers a download. Dataless files are recorded by path only and parsed when opened; dataless directories are not descended;
- nested roots: a dir with a strong or VCS marker is recorded as its own root and not descended (§4).

| Budget (exceeding aborts) | Marker root | Loose root |
|---|---|---|
| entries visited | 200k | 10k |
| md files | 50k | 5k |
| wall time | 1.5 s | 300 ms |
| depth | 32 | 8 |

A count going above its budget aborts; depth counts from the walk root (its children are depth 1). Entries counts every listed entry, pruned or not.

**Rate check, per directory.** Entries/s depends on entries per directory (a local Documents folder measured 16–19k entries/s), so time each `readdir` instead: after the first 50 directories or 100 ms, whichever comes first, take the median ms/dir (a walk that finishes sooner is fast enough and is never rate-aborted). Above the threshold the FS is slow (network, FUSE, cold disk, cloud): abort and go lazy. The threshold is 5 ms/dir (`DiscoverOptions::rate_ms_per_dir` overrides it). Warm-cache walks measure ~0.16–0.22 ms/dir (§0), far below it; it is not calibrated on a cold cache (after `sudo purge`), a cloud folder or EdenFS.

**Recording.** Abort or success records `{entries_seen, md_seen, dirs_seen, ms, ms_per_dir, reason}` in the registry, so the next start goes straight to reconcile. A lazy verdict from the rate check **alone** is saved only after a second measurement (later or by another session) agrees, so one slow walk during a herd start or backup does not stick. In the implementation the first rate abort registers a pending row (`lazy pending: rate 7.1 ms/dir, 1 of 2 measurements`, one confirmation) that stage 1 treats as a miss; the next walk either succeeds and replaces it or aborts on rate again and confirms it (2 of 2). Other lazy verdicts (virtual FS, Eden, budgets) are saved at once. A cancelled walk is never registered. Retry the full walk at most once per 7 days, or on `fs_type`/`st_dev` change, or on `mdroots.reindex`.

## 2. Loose roots (no VCS or marker)

1. **Denylist**, compared lexically (no symlink resolution). Exact matches only: `/`, `$HOME`, `/tmp`, `/private`, `/private/tmp`, `/var`, `/private/var`, a `/Volumes/<name>` volume root, `~/Downloads`, `~/Desktop`; so `/var/folders/...` temp dirs are allowed. By prefix: `~/Library` and everything under it, except inside an [Obsidian](https://obsidian.md) vault under `~/Library/Mobile Documents/iCloud~md~obsidian/Documents/<vault>`. Also denied: any virtual/remote FS, a local mount inside a virtual repo, and a directory whose mount cannot be read. Denied → **single-file mode**: the current buffer plus its relative links resolved by `stat`.
2. **Lower bound from the buffer's links**: every existing relative link target directory must be inside the root. The file path and link dirs are cleaned lexically first (`.` dropped, `..` applied), so the denylist sees the real target.
3. **Grow upward.** Start at the deepest common ancestor of `dir(file)` and the existing link target dirs, always accepted unless denied, and walk it with the loose budget. If that walk aborts, the start dir becomes a **lazy** root (verdict `budget`; a rate abort follows the two-measurement rule of §1 stage 4) and nothing grows. A start dir outside `$HOME`, or with no `$HOME`, never grows. Otherwise climb toward `$HOME` (never reaching it), walking each parent with the child's subtree skipped and the child's counts reused, within what is left of the loose budget (wall time counted from the start of the search). Accept the parent only if it is not denied, its walk does not abort, the child's deepest directory stays within the loose depth budget below it, and either:
   - **(a)** its subtree has ≥ 20 md files and md density (`md_files / files`) ≥ 30%, or
   - **(b)** it adds more md files outside the child's subtree than the child holds.

   Example: a `scratch/` dir (10 files, 2 md, 6 nested repos pruned) under a projects folder (506 files, 4 md) is not grown: (a) fails at 4 md, (b) fails because 2 added is not more than 2. Still rejected without the tarball tree (16 files, 3 md). Notes folders are usually > 50% md.
4. The highest accepted ancestor is the loose root, registered with its reason (`loose root accepted at <dir>: <md> md / <files> files`, or the line of the first rejection). It is then walked once more with the loose budget for its md list; a folder over that budget (more than 5k md or 10k entries) is lazy. To index a larger notes folder fully, add an empty `.mdroots`, which makes it a marker root with the marker budget.
5. **Hysteresis** (not applied yet; needs the reconcile's file counts): re-run the climb only if the recorded stats are > 7 days old or the reconcile sees the file count change > 2×, so roots do not flip around a threshold and rebuild.

The 20-file, 30%, 2× and 7-day numbers are validated only on the fixtures in §7 (see [OPEN-QUESTIONS.md](../OPEN-QUESTIONS.md)). Results are staged: single-file features at once, then results published after each accepted level.

## 3. Lazy and vcs-enumerated modes

Lazy mode never enumerates the tree:

- **Working-set index**: each opened file's directory is listed one level (cap 2k entries, dataless skipped) and its md files parsed into the DB.
- **Links by `stat`**: relative and root-relative paths, each with and without `.md`. On Eden one stat is a metadata lookup, no content fetch.
- **`[[stem]]`**: working-set index only. Optional v2: ask the VCS with a 500 ms timeout (`git ls-files '*foo.md'`, Eden's glob API, or `sl files 'glob:**/foo.md'`); measure first.
- **No native watcher.** A process re-checks working-set files when its own editor opens or saves them. The reconciler point-checks and writes files from its own editor's saves and from the client's `didChangeWatchedFiles`. Another peer's save reaches the DB only when that peer later becomes reconciler, or at the next working-set sweep.
- **Diagnostics** per the [index spec](index.md): only `stat`-checkable links are diagnosed; an unresolved `[[stem]]` is a hint ("not in indexed set"), never an error.

**vcs-enumerated** (small Eden repos, where lazy would mean `[[stem]]` never resolves): ask Eden once for `**/*.md` via its glob API, falling back to `sl files 'glob:**/*.md'`, killed after 500 ms or 20k paths. In budget → the path list feeds stem resolution and the reconcile queue as in index-driven mode, parsed at background priority, open and linked files first. Over budget → stays lazy, recorded (verdict `budget`), not retried for 7 days. No watcher either way.

In the implementation the enumeration is an `Enumerator` trait: `SlFiles` runs `sl files 'glob:**/*.md'` (the glob API is not used yet) and `NoEnumerator` turns the mode off. `discover()` runs it synchronously within the 500 ms budget; moving it to the background is the index layer's job. Only a virtual FS whose `.eden/root` resolves, and whose checkout root has no monorepo marker file (`.buckconfig`, `WORKSPACE`, `MODULE.bazel`), is offered to the enumerator; a remote FS with `.eden` is lazy, and a virtual or remote FS without `.eden/root` is lazy with no root and is not registered.

## 4. Nested roots

**Every file belongs to exactly one root**, its nearest marker ancestor. A root's scope is its subtree minus nested roots' subtrees (as git treats nested repos).

```
nb/              .zk    root A  scope = nb − nb/proj
nb/proj/         .git   root B  scope = nb/proj
nb/proj/docs/    —      → B
```

- Walks prune at nested markers, so DBs are disjoint and one root's GC never touches another.
- **The registry rejects overlapping inserts**, except a nested root at a marker (nearest wins). A loose root never contains a marker root, and loose roots never nest; a new marker root may appear inside a loose root. When an insert is rejected, discovery uses the nearest registered root containing the file, or returns its decision unregistered with `(not registered: overlaps <dir>)`.
- **Cross-root links** (planned, M7): resolve in the current scope first; on a miss, find the root containing the target in the registry and `ATTACH` its DB read-only (LRU, SQLite's default limit is 10). Today a link into another root resolves only by `stat` (relative paths) as Unindexed. Completion stays in the current root.
- **New marker** (e.g. `git init` in a loose root): found by the next reconcile or the stage 1 probe; the subtree is re-parsed into the new root and its rows deleted from the parent.
- **Marker removed**: merged back on the parent's next reconcile; the orphan DB is GC-ed (§6).
- **Editor folder vs markers**: a file in `nb/proj` belongs to B even if the client's workspace folder is `nb`. The process just opens a second DB.

## 5. Many processes on one root

Typical: tmux with many nvim instances restored at once. No daemon; each process runs mdroots in-process, coordinates only through the filesystem, and each root has one writer (D3 in [DECISIONS.md](../DECISIONS.md)). What the DB holds and how a process serves queries from it is D9 and [index spec](index.md) §1.

**Built**: the cache dir chain, the registry, `discover.lock`, the per-root DB, the `.lock`/`.open` roles, peers that never write, promotion on `Workspace::refresh` (M4); the reconciler's native watcher, GC, and the corruption rebuild into generation files (M6). Everything marked planned or M7 below is not built yet.

### Files

Base dir per D5: `$XDG_CACHE_HOME/mdroots`, else `~/Library/Caches/mdroots` (macOS, excluded from Time Machine) or `~/.cache/mdroots`. If not local (`MNT_LOCAL` unset) or not writable: `$XDG_RUNTIME_DIR/mdroots`, then `/var/tmp/mdroots-$UID` (`0700`, owner checked), then an in-memory index for the session, because `flock` and WAL are unsafe on NFS/SMB. A registry that cannot be opened also leaves the session in memory (planned: also a full disk at write time). `Options::cache_dir` (the CLI's `MDROOTS_CACHE_DIR`) replaces the chain. Processes with different base dirs (e.g. `XDG_CACHE_HOME` set in some shells only) get separate reconcilers; `mdroots roots` prints the root's DB path (`cache:`) and this process's role (`role: reconciler|peer`, or `cache: memory`, `role: none`).

`<db>` = `<id>.v<schema>` (`<id>.v2` today), so each schema version has its own file and locks. One lock scope per root and schema: `<db>.lock` and `<db>.open` cover every generation file of `<db>`.

| Path | Purpose |
|---|---|
| `roots.v1.db` | registry (SQLite, WAL), versioned name |
| `discover.lock` | global flock (`DiscoverLock`, via `std::fs::File::lock`); `Workspace::open_for` holds it around the whole of `discover()` with a cache, so one process discovers at a time |
| `roots/<db>.db` | per-root index (SQLite, WAL) |
| `roots/<db>.lock` | reconciler flock; never deleted |
| `roots/<db>.open` | `LOCK_SH` held by every process with any `<db>*.db` open; never deleted |
| `roots/<db>-<gen8>.db` | a new generation after a corruption rebuild; `gen8` is the first 8 hex digits of its `meta.generation` |

The registry row stores the current DB file name (`db_file`): `<db>.db`, or the generation file after a rebuild. A process opens the file `db_file` names when it is one of this schema for the root, else `<db>.db`, which it then records. `root_id` (a hash of the first path and the decision time) never changes, even when the root moves.

### Roles

| Topic | Rule |
|---|---|
| Reconciler | holds `flock(LOCK_EX\|LOCK_NB)` on `<db>.lock`; one per root and schema. Sole writer: it lists the root, reconciles and writes on open, on `refresh` and on `refresh_paths`, keeps the FTS table in sync, and is the only process that may run the native watcher. Each transaction `BEGIN IMMEDIATE`, `busy_timeout = 2s`. It runs `quick_check` on the DB it opens or takes over. Planned: convention inference |
| Peers | never write; they never list the root either. On open and `refresh` they read the DB rows, re-stat those files and read changed ones into memory only. On `refresh` and `refresh_paths` they re-read the registry row and reopen when `db_file` names another file (a rebuild). Planned: a per-request check of `PRAGMA data_version` (~1.2 µs) and the DB inode, to catch deletion and cache purges between refreshes |
| Peer saves | the editor's text is the peer's overlay; on `didSave` the peer refreshes, re-reading changed files from disk itself. The DB gets the change through the reconciler's watcher (when it runs one) or its next refresh |
| Open buffers | per process, laid over the index; never written before save, never by a peer. Two editors see their own unsaved text |
| `SQLITE_BUSY` | `busy_timeout` 2 s on every connection. Planned: the reconciler runs `wal_checkpoint(PASSIVE)` periodically and sets `journal_size_limit`, because a leaked reader blocks checkpoints |
| Takeover | the kernel releases the flock on exit or crash; a peer tries `LOCK_EX\|LOCK_NB` again on every `refresh` and becomes the reconciler if it is free. No heartbeat, PID check or stale-lock cleanup. A promoted peer starts its watcher then. Planned: a 5 s retry timer |
| Stuck reconciler | alive but stuck (SIGSTOP, hung `stat` on Eden/NFS) keeps the lock; peers still open and answer from the DB plus their own reads, without waiting on it. Planned: a peer seeing a stale `reconciled_at` logs one `window/logMessage` warning |

### File events

`mdroots lsp` turns on the native watcher (`Options::watch(true)`; libraries default to off). As built:

- **Only the reconciler watches**, and only a DB-backed marker, VCS or loose root on a local filesystem. Never a lazy, single-file, tracked-only, index-driven or vcs-enumerated root, and never a virtual, remote or cloud filesystem, so a root on EdenFS is never watched. Peers act only on their own `didOpen`/`didChange`/`didSave`, in overlays; otherwise N editors would mean N parses per save.
- [notify](https://crates.io/crates/notify) 8 watches the root recursively (FSEvents on macOS, inotify on Linux). Events are debounced (200 ms quiet, 1 s cap), filtered by root-relative components (hidden and pruned dirs dropped), and point-refreshed with `Workspace::refresh_paths`; a lost-events rescan or an event on the root itself re-lists the root ([index spec](index.md) §1.3).
- Client `didChangeWatchedFiles` (the server registers a `**/*.{md,markdown,org}` watcher right after `initialize` when the client offers dynamic registration; Neovim does on macOS and Windows) refreshes only workspaces that do not watch: peers, lazy roots and the other non-watching modes. A peer's refresh writes nothing. `didSave` refreshes the saved document's workspace in every process.
- **Offline changes and takeover.** A file changed while no reconciler watched (no mdroots running, or between a reconciler's exit and a peer's promotion) is picked up by the next re-list and re-stat: on open, on `refresh` (every `didSave`), or on promotion.
- **Planned (M7): FSEvents replay.** `notify` hard-codes `kFSEventStreamEventIdSinceNow`, so replay from `meta.fsevents_last_id` needs `fsevent-sys` or direct FFI, which the workspace's `forbid(unsafe_code)` rules out for now. `MustScanSubDirs`, `UserDropped`, `KernelDropped`, `EventIdsWrapped` would fall back to a `dir_state` diff of the subtree (or root); the new ID would be committed after `HistoryDone`, in the same transaction as its batch. On Linux, a takeover would run one `dir_state` diff before starting inotify.

### DB lifetime: never unlink an open DB

Unlinking or renaming an open SQLite file is a documented corruption path.

- Every process holds `LOCK_SH` on `<db>.open` while any `<db>*.db` is open.
- GC takes `LOCK_EX|LOCK_NB` on `<db>.open`, skips the root on failure, and deletes DB files with their `-wal` and `-shm`, never lock files.
- **Corruption** (`quick_check` fails when the reconciler opens or takes over the DB, or opening or a sync returns `Corrupt`, e.g. `SQLITE_NOTADB`): the reconciler never renames or unlinks; it builds `<db>-<gen8>.db` with a new `meta.generation`, fills it, and repoints the registry row's `db_file` after that first sync succeeds, so peers never switch to an empty file. A peer that opens a corrupt file serves from an empty in-memory DB (it indexes as the peer of an empty DB does) and never fails. On `refresh` and `refresh_paths` every process re-reads the row and reopens the file it names. GC deletes the old file once nobody holds the root open (fixture 15).
- `<db>.lock` and `<db>.open` are never deleted, not even when a whole root is GC-ed: they are tiny, and a waiter on an unlinked lock would elect a second reconciler. Because the files are never unlinked, a process need not re-check the locked inode.
- The registry is never deleted automatically; an old `roots.v<k-1>.db` is a few KB.

### Thundering herd

- **Discovery is serialised** under `discover.lock` (the per-root flock cannot help before the root is known). As built, a process that does not get it blocks until it does, then finds the row in the registry (planned, M7: serve single-file features meanwhile and finish the open on a background thread). One process walks; the rest find the row. This is also the global walk semaphore: one discovery walk per user at a time.
- One process wins the root flock; the others serve immediately from the index plus overlays. None waits on a write lock, because none writes.
- A new root's DB is created by the flock holder only (`CREATE ... IF NOT EXISTS` in `BEGIN IMMEDIATE`). With no DB it indexes at once, inside `open_for`. A peer that opens before the DB has rows indexes in memory from discovery's listing and writes nothing. Peers publish diagnostics per document once that document and its targets are checked locally.
- Rate-only lazy verdicts need two measurements (§1 stage 4), so herd slowness does not stick.

### Versions, memory, scheduling

- **Upgrades**: schema version in the DB filename and locks, so v1 and v2 processes never fight over migrations. Files of another schema are GC-ed once the newest of them is 7 days old by mtime and `<id>.v<old>.open` can be taken exclusively. The registry carries its own `k` for the same reason (still `roots.v1.db`).
- **Memory**: < 35 MB `phys_footprint` per process with N=10 concurrent instances. As built (D9) each process holds every note's bytes and parse in memory: measured peak footprint 2.2 MB on an 11-note vault and 23–31 MB on a synthetic 3,000-note notebook, depending on the path (D9, [OPEN-QUESTIONS.md](../OPEN-QUESTIONS.md)). The derived tables (M7) would bound it for larger vaults. SQLite is `mmap`ed, so N processes share one page-cache copy; on macOS those pages show in every process's RSS, so measure with `footprint` or `proc_pid_rusage` (`ri_phys_footprint`), not RSS. For comparison, 10 [marksman](https://github.com/artempyanykh/marksman) instances are 10 full workspaces at 134–145 MB RSS each.
- **Scheduling** (planned, with a background thread): background work (walks, sweeps, FTS, inference, vcs-enumerated parsing) at `QOS_CLASS_BACKGROUND` (E-cores only). `QOS_CLASS_UTILITY` only for the small link-target queue of open buffers (it prefers P-cores). Requests at default QoS. Linux: `nice 10` + `IOPRIO_CLASS_IDLE` for background, best-effort for the link-target queue.

## 6. Housekeeping

Registry table as built (`roots.v1.db`, `SqliteRegistry`; one row per `RootRecord`):

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
| `rate_confirmations` | the two-measurement rule of §1 stage 4 |
| `decided_at_ms`, `last_seen_ms` | when the row was decided; when a process last opened or refreshed the root (stamped at most hourly), for GC |
| `reason` | the one-line explanation |
| `db_file` | the per-root DB's file name: `<root_id>.v2.db`, or `<root_id>.v2-<gen8>.db` after a rebuild |

plus `meta(key, value)` with the registry schema and `gc_at`, the time of the last GC run. Rows that do not decode (an unknown mode from a newer binary) are skipped; SQLite errors on reads count as a miss, because the registry is a cache. Overlap checks and inserts run in one `BEGIN IMMEDIATE`, so two processes inserting the same root get one row.

- **GC** (`mdroots_index::gc`): at most daily, claimed by the process that updates `meta.gc_at` in a `BEGIN IMMEDIATE` registry transaction; `open_for` runs it when due. Candidates: generation files the row's `db_file` does not name, DB files of a root with no row, old-schema files 7 days old, roots not seen for 30 days or whose path is gone (their row too), and, over a 1 GiB budget, the least recently seen roots. Each needs `LOCK_EX|LOCK_NB` on `<db>.open`, else skipped until the next run (fixture 14). Details: [index spec](index.md) §1.7.
- **OS cache purge** (macOS clears `~/Library/Caches` under disk pressure; cleaners) is treated as deletion: the next process recreates the cache and becomes the reconciler; without a registry, discovery reruns under `discover.lock`. A process already running keeps serving from memory (fixture 16); reopening on a purge before the next refresh is planned.
- **`mdroots roots PATH`** prints the root chosen for PATH with mode, indexed file count, the DB path and this process's role, and why, e.g. "lazy: statfs edenfs:, MNT_LOCAL unset", "single-file: inside a local mount in an Eden repo", "loose root rejected at <dir>: 4 md (< 20), adds 2 md (<= 2 in child)", "lazy pending: rate 7.1 ms/dir, 1 of 2 measurements". `explain()` returns this line; `cargo run -p mdroots-roots --example roots -- <file>` prints it with the `readdir` count. In the editor, `mdroots.info` (`:MdrootsInfo` in Neovim) shows root, mode, reason and file count through `window/showMessage`; it is the main way to debug a wrong root.
- **Escape hatches**: empty `.mdroots` forces a root; `.mdrootsignore` excludes paths and stops loose climbing (empty: never index here); `MDROOTS_LAZY=path1:path2` forces lazy (optional, not implemented yet).

## 7. Fixtures

Many-process fixtures use a temporary cache dir. "Zero readdir" is checked with an instrumented walker or `fs_usage -f filesys`. Timings logged.

Discovery fixtures 1–11 live in `crates/mdroots-roots/tests/fixtures.rs`. Fixtures 1–4 and 9–11 build synthetic trees in a temp dir and run over `StdProbe`, wrapped in a probe that pins `home()` to the temp dir (setting `$HOME` would need unsafe code), so growth rules apply and no climb leaves the temp dir; `Counting` checks every `readdir`. Fixtures 5–8 need a virtual FS or a cloud folder and run on `FakeProbe`. No test touches a real home directory, network mount or virtual FS. Each asserts its `discover()` call stays under one second. Many-process fixtures 12–18 live in `crates/mdroots-cli/tests/many_process.rs`: they run real `mdroots` processes on a copy of `tests/corpus/zk-min` in a temp dir, with `MDROOTS_CACHE_DIR`, `XDG_CACHE_HOME` and `HOME` pointed into it. A hidden `mdroots __open PATH --hold-ms N [--refresh-every MS]` opens a workspace, prints its role, file count and DB file, holds it open, and prints them again after each refresh; a hidden `mdroots __gc --now-ms N --force` runs a forced GC on that cache dir. Those processes run no watcher (only `mdroots lsp` turns it on); the watcher is tested in-process in `crates/mdroots/tests/watch.rs`, on temp dirs only. Fixture 19 is replaced by the watcher and the re-list on open.

**Discovery**

1. A file in a no-VCS `scratch/` dir inside a projects folder of ~40 repos: loose root `scratch/`, climb rejected by (a) and (b); also on a synthetic copy without the tarball tree (16 files, 3 md). A file inside any of the repos gets that repo.
2. A mid-size git checkout (3.8k files, 13 md, the shape of a [Neovim](https://neovim.io) checkout): budgeted walk.
3. Synthetic notes folder, no VCS, 4,000 md among 6,000 files: loose root accepted by (a). A larger one (6,000 md) exceeds the loose md budget: lazy, verdict `budget`.
4. A `.zk` notebook inside a git repo.
5. A file in a large EdenFS monorepo: lazy from `statfs` before any marker stat, root from `readlink(.eden/root)`, zero readdir outside the file's dir.
6. A file in a build-output mount inside that checkout: lazy or single-file, zero readdir outside the file's dir, no registry row at the mount.
7. A small notes repo on EdenFS: `vcs-enumerated` within 500 ms, zero readdir outside opened dirs (that `[[stem]]` then resolves is checked with the index, M3).
8. A dataless cloud file (iCloud after `brctl evict`, or Drive online-only), in a loose root: recorded as dataless and not read during discovery, still `SF_DATALESS` afterwards; link existence by `stat` (opening it is M3).
9. A dotfiles repo at a temp `HOME` with a large untracked tree: `tracked-only`, zero walk.
10. `git clone` into an existing lazy root, open a file in it: stage 1 probe registers a nested root.
11. `mv notes notes2` between sessions: row re-keyed by marker inode + volume UUID, no cold rebuild.

**Many processes**

12. 10 processes on fixture 3 with an existing DB: one reconciler and writer; peers never open a writer connection; no `SQLITE_BUSY` reaches requests; all converge on one `data_version`; a save shows in other editors within watcher latency. **Built, adapted**: 10 processes at once on an existing DB: exactly one reconciler, the rest peers, all serve the same files, none errors.
13. Cold herd (no registry, no DB, `sudo purge`), 10 processes on a loose root: one discovery walk, one registry row, no overlaps, no rate-only lazy verdict saved; every `phys_footprint` < 35 MB. **Built, adapted**: 10 processes at once on a marker root with no registry and no DB: one reconciler, exactly one registry row. Footprint is measured by hand (D9).
14. GC with 3 peers holding an aged DB open: skipped, peers keep answering, `integrity_check` passes. Repeat for an old schema file and a stale generation. **Built**: 3 processes hold an aged root open and refresh every 100 ms; a forced GC 40 days later skips its DB and the processes keep serving the same files from an intact DB; after they exit, GC deletes the DB and the registry row. Old-schema and stale-generation files are covered in-process in `crates/mdroots-index/tests/gc.rs`.
15. Corrupt the DB under 3 peers: new generation, peers reopen, old file deleted only after the last peer exits. **Built, adapted**: the processes are stopped before the header is overwritten (with a connection open, `quick_check` does not see an overwritten header), then 3 new ones start: one reconciler builds `<db>-<gen8>.db`, all serve the same files, peers reopen the recorded file on refresh, the corrupt file stays while they run, and a GC after they exit deletes it.
16. `rm -rf` the base dir under 3 processes: all reopen, one rebuilds, nobody writes to an unlinked file. **Built**: the cache dir is deleted while a process holds the DB open; the next process recreates it and becomes the reconciler with the same files, and the holder exits cleanly (reopening is planned).
17. mtime-regressing copy (`cp -p`, `rsync -a`, `tar x` of an older version): new content indexed (ctime advanced); a bare `touch` does not re-parse links. **Built**: same size, older mtime restored, new link target: the next `check` reports the new content. `touch` is covered in `crates/mdroots-index/tests/reconcile.rs`.
18. `kill -STOP` the reconciler, save in a peer: one warning, peer fresh from its overlay, no DB writes. `kill -CONT`: reconciler picks up the save. `kill -9`: a peer takes over and re-checks its working set. **Built, adapted**: with the reconciler stopped, a new process opens as a peer within 5 s; after the reconciler is killed, the next process becomes the reconciler with the same files. The warning is planned.
19. **Replaced.** Kill the reconciler, change files, a peer takes over: the changes arrive by the re-list and re-stat the promoted peer runs on `refresh` (and every later open), and from then on through its watcher. FSEvents replay and the takeover `dir_state` diff, which would avoid the re-list, are M7.
