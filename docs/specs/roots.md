# Spec: root discovery, size budgets, and many instances

Related: [index spec](index.md), [library spec](library.md), D3, D4, D5 in [DECISIONS.md](../DECISIONS.md).

Hard rule: **mdroots never calls `readdir` on a tree before it has established that the tree is local and bounded.** A monorepo on a virtual filesystem such as [EdenFS](https://github.com/facebook/sapling) (the virtual filesystem from the [Sapling](https://sapling-scm.com/) project) can hold millions of files fetched on demand; a recursive walk there takes hours. Every rule below exists to make that walk impossible.

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
| `readdir` per directory, warm cache | median 0.19–0.22 ms/dir. Rust walker on a cold cache: not yet measured |

Consequences: pruning makes walks ~12× cheaper, so it is required. Big or virtual trees are detectable with a few `stat`/`statfs` calls; the test is `MNT_LOCAL`, not the type name, and it must be repeated at mount boundaries inside a virtual tree. Cloud folders are caught by path, `SF_DATALESS` and walk speed.

## 1. Discovery stages, cheap to expensive

Each stage may stop with a decision. Only stage 4 calls `readdir` recursively. Stages 2–4 run under the global `discover.lock` (§5).

### Stage 1: registry lookup

Longest-prefix match of `realpath(file)` in the registry (`roots.v<k>.db`). On macOS also canonicalise with `F_GETPATH`, because APFS is case-insensitive. A hit is used only if:

1. the recorded marker still exists (one `stat`);
2. the root's `st_dev` and `fs_type` match the recorded values (else re-decide from stage 2, lazy verdicts included);
3. no new marker sits between `dir(file)` and the root. Probe each level with the stage 2 marker list, cached per directory per session; on a virtual FS probe only the explicit and notes-tool markers (8 names, ~0.1–0.2 ms each). This is how a `git clone` or new `.zk/` inside a loose or lazy root becomes its own root.

Otherwise treat it as a miss. After the first session the hit path is a few stats.

**Root moves.** Each row stores the marker's inode and the volume UUID (`ATTR_VOL_UUID` on macOS, `f_fsid` on Linux). On a miss where stage 2 finds a marker, a row with the same `(volume_uuid, marker_ino)` whose path no longer holds the marker is a move: update `path`, keep `root_id` and the DB file. Nothing is renamed on disk, and `files.path` is root-relative ([index spec](index.md)), so rows survive. A copy (both paths hold a marker) is a new root.

### Stage 2: `statfs`, then marker climb (no `readdir`)

**`statfs(dir(file))` first**, because a 10-level climb on Eden costs 10–30 ms. Virtual/remote (stage 3 test) → no climb; on Eden take the root from `readlink(dir(file)/.eden/root)` and go to stage 3. Local → climb.

**Climb** upward from `dir(file)`, `stat`ing ~15 names per level. Stop at the first of:

- a mount boundary (`st_dev` changes). Before stopping, `statfs` the mount point's parent and `stat` `<parent>/.eden`. If the parent is virtual, the file is in a local mount inside a virtual repo: **lazy** for a marker below the boundary, else **single-file**. Never walk it.
- `$HOME` (checked, never climbed past), or `/`.

| Class | Markers | Meaning |
|---|---|---|
| explicit | `.mdroots` (empty file), `.mdrootsignore` | "root here" / "never index here" |
| notes tool | `.zk/`, `.obsidian/`, `.marksman.toml`, `.iwe/`, `.foam/` | strong |
| docs tool | `mkdocs.yml`, `book.toml`, `docusaurus.config.*`, `_config.yml`, `hugo.toml`, `conf.py` + `index.md` | strong |
| VCS | `.git` (dir or file), `.jj`, `.hg`, `.sl` | medium |
| monorepo | `.eden/`, `.buckconfig` ([Buck2](https://buck2.build)), `WORKSPACE`/`MODULE.bazel` ([Bazel](https://bazel.build)) | tree is **huge** |
| editor | LSP `workspaceFolders` containing the file, when sent | strong |

The nearest strong marker beats a farther VCS root (a `.zk/` notebook inside a git repo). A `.git` file (submodule, worktree) is a root of its own. Discovery must work from markers alone, because Neovim with `root_dir = nil` sends `workspaceFolders = null` and reused clients never get `didChangeWorkspaceFolders` (see [library spec](library.md)).

### Stage 3: classify without walking

1. **Local or not**, by `statfs` on the candidate root:
   - `MNT_LOCAL` unset → virtual/remote.
   - Fallback by name prefix (so `edenfs:` matches): `edenfs`, `nfs`, `smbfs`, `afpfs`, `webdav`, `macfuse`, `osxfuse`, `fuse`, `9p`, `virtiofs`, `sshfs`; check `f_mntfromname` too.
   - Linux: fstype from `/proc/self/mountinfo` (`fuse.*`, `nfs*`, `cifs`, `smb3`, `9p`, `virtiofs`).
   - Cloud folders (`~/Library/CloudStorage/*`, `~/Library/Mobile Documents/*`) are marked `cloud`: walks allowed, `st_flags` checked per entry, rate check without relaxation.
2. `.eden/` at the root → virtual, whatever statfs says.
3. **Size estimate**, cheapest first: registry stats; the `.git/index` header count (unreliable if the index has a `sdir` sparse or `link` split extension: treat as unknown, use index-driven mode with the 200k cap applied while listing); colocated jj uses `.git/index`. Non-colocated jj, hg, sl without Eden: no cheap count, go to the budgeted walk.

| Condition | Mode |
|---|---|
| virtual/remote FS, Eden, or monorepo marker | **lazy** (§3): no walk, no watcher |
| Eden repo whose enumeration finishes in budget | **vcs-enumerated** (§3) |
| local mount inside a virtual repo | **lazy** if a marker is below the mount, else **single-file** |
| VCS root at `$HOME` or another denylisted dir (§2) | **tracked-only** |
| git index > 200k entries or > 32 MB | **lazy** |
| git index 20k–200k entries, or count unreliable | **index-driven** |
| git index < 20k entries | **budgeted walk** (also finds untracked md) |
| no VCS | **loose root** search (§2), then budgeted walk |

**Index-driven** and **tracked-only** list `*.md`/`*.markdown`/`*.org` paths from the git index in-process (`gix-index`), no `readdir`. The index's stat data is a snapshot, so each listed file is `stat`ed before it is trusted. Untracked md arrives via `didOpen` and the watcher; a small background walk adds it only if the tree later proves small, never in tracked-only. Tracked-only exists so a dotfiles `$HOME/.git` does not make the home directory one root; it applies to any VCS marker at a denylisted location (`/`, `/Volumes/*`, `~/Library`).

### Stage 4: budgeted walk

Parallel breadth-first walk with the `ignore` crate (shallow files are the likeliest link targets, so a partial result covers them), at background priority (§5); budgets are calibrated under that priority.

Prune:

- ignore files: `.gitignore`, `.hgignore`, `.ignore`, `.mdrootsignore`;
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

**Rate check, per directory.** Entries/s depends on entries per directory (a local Documents folder measured 16–19k entries/s), so time each `readdir` instead: after the first 50 directories or 100 ms, whichever comes first, take the median ms/dir (a walk that finishes sooner is fast enough and is never rate-aborted). Above the threshold the FS is slow (network, FUSE, cold disk, cloud): abort and go lazy. The threshold is not yet calibrated (Rust walker, cold cache after `sudo purge`, on APFS, a cloud folder and EdenFS); until then it is 5 ms/dir and logged.

**Recording.** Abort or success records `{entries_seen, md_seen, dirs_seen, ms, ms_per_dir, reason}` in the registry, so the next start goes straight to reconcile. A lazy verdict from the rate check **alone** is saved only after a second measurement (later or by another session) agrees, so one slow walk during a herd start or backup does not stick. Other lazy verdicts (virtual FS, Eden, budgets) are saved at once. Retry the full walk at most once per 7 days, or on `fs_type`/`st_dev` change, or on `mdroots.reindex`.

## 2. Loose roots (no VCS or marker)

1. **Denylist**: `/`, `$HOME`, `/tmp`, `/private`, `/var`, `/Volumes/*` roots, `~/Downloads`, `~/Desktop`, `~/Library` (except an Obsidian vault under `~/Library/Mobile Documents/iCloud~md~obsidian/Documents/<vault>`), local mounts inside a virtual repo, any virtual/remote FS. Denied → **single-file mode**: the current buffer plus its relative links resolved by `stat`.
2. **Lower bound from the buffer's links**: every existing relative link target directory must be inside the root.
3. **Grow upward.** Start at `max(dir(file), link lower bound)`, always accepted unless denied. Climb toward `$HOME`, extending the walk incrementally (child results reused). Accept the parent only if it is not denied, the cumulative walk stays within the loose budget, and either:
   - **(a)** its subtree has ≥ 20 md files and md density (`md_files / files`) ≥ 30%, or
   - **(b)** it adds more md files outside the child's subtree than the child holds.

   Example: a `scratch/` dir (10 files, 2 md, 6 nested repos pruned) under a projects folder (506 files, 4 md) is not grown: (a) fails at 4 md, (b) fails because 2 added is not more than 2. Still rejected without the tarball tree (16 files, 3 md). Notes folders are usually > 50% md.
4. The highest accepted ancestor is the loose root, registered with its reason.
5. **Hysteresis**: re-run the climb only if the recorded stats are > 7 days old or the reconcile sees the file count change > 2×, so roots do not flip around a threshold and rebuild.

The 20-file, 30%, 2× and 7-day numbers are validated only on the fixtures in §7 (see [OPEN-QUESTIONS.md](../OPEN-QUESTIONS.md)). Results are staged: single-file features at once, then results published after each accepted level.

## 3. Lazy and vcs-enumerated modes

Lazy mode never enumerates the tree:

- **Working-set index**: each opened file's directory is listed one level (cap 2k entries, dataless skipped) and its md files parsed into the DB.
- **Links by `stat`**: relative and root-relative paths, each with and without `.md`. On Eden one stat is a metadata lookup, no content fetch.
- **`[[stem]]`**: working-set index only. Optional v2: ask the VCS with a 500 ms timeout (`git ls-files '*foo.md'`, Eden's glob API, or `sl files 'glob:**/foo.md'`); measure first.
- **No native watcher.** A process re-checks working-set files when its own editor opens or saves them. The reconciler point-checks and writes files from its own editor's saves and from the client's `didChangeWatchedFiles`. Another peer's save reaches the DB only when that peer later becomes reconciler, or at the next working-set sweep.
- **Diagnostics** per the [index spec](index.md): only `stat`-checkable links are diagnosed; an unresolved `[[stem]]` is a hint ("not in indexed set"), never an error.

**vcs-enumerated** (small Eden repos, where lazy would mean `[[stem]]` never resolves): ask Eden once, in the background, for `**/*.md` via its glob API, falling back to `sl files 'glob:**/*.md'`, killed after 500 ms or 20k paths. In budget → the path list feeds stem resolution and the reconcile queue as in index-driven mode, parsed at background priority, open and linked files first. Over budget → stays lazy, recorded, not retried for 7 days. No watcher either way.

## 4. Nested roots

**Every file belongs to exactly one root**, its nearest marker ancestor. A root's scope is its subtree minus nested roots' subtrees (as git treats nested repos).

```
nb/              .zk    root A  scope = nb − nb/proj
nb/proj/         .git   root B  scope = nb/proj
nb/proj/docs/    —      → B
```

- Walks prune at nested markers, so DBs are disjoint and one root's GC never touches another.
- **The registry rejects overlapping inserts**, except a nested root at a marker (nearest wins). A loose root never contains a marker root, and loose roots never nest.
- **Cross-root links**: resolve in the current scope first; on a miss, find the root containing the target in the registry and `ATTACH` its DB read-only (LRU, SQLite's default limit is 10). Completion stays in the current root.
- **New marker** (e.g. `git init` in a loose root): found by the next reconcile or the stage 1 probe; the subtree is re-parsed into the new root and its rows deleted from the parent.
- **Marker removed**: merged back on the parent's next reconcile; the orphan DB is GC-ed (§6).
- **Editor folder vs markers**: a file in `nb/proj` belongs to B even if the client's workspace folder is `nb`. The process just opens a second DB.

## 5. Many processes on one root

Typical: tmux with many nvim instances restored at once. No daemon; each process runs mdroots in-process, coordinates only through the filesystem, and each root has one writer (D3 in [DECISIONS.md](../DECISIONS.md)).

### Files

Base dir per D5: `$XDG_CACHE_HOME/mdroots`, else `~/Library/Caches/mdroots` (macOS, excluded from Time Machine) or `~/.cache/mdroots`. If not local (`MNT_LOCAL` unset) or not writable: `$XDG_RUNTIME_DIR/mdroots`, then `/var/tmp/mdroots-$UID` (`0700`, owner checked), then an in-memory index for the session, because `flock` and WAL are unsafe on NFS/SMB. A full disk at write time also drops to in-memory. Processes with different base dirs (e.g. `XDG_CACHE_HOME` set in some shells only) get separate reconcilers; `mdroots roots` prints the base dir and why.

`<db>` = `<id>.v<schema>`, so each schema version has its own file and locks.

| Path | Purpose |
|---|---|
| `roots.v<k>.db` | registry (SQLite, WAL), versioned name |
| `discover.lock` | global flock held during discovery stages 2–4 |
| `roots/<db>.db` | per-root index (SQLite, WAL) |
| `roots/<db>-<gen8>.db` | new generation after a corruption rebuild |
| `roots/<db>.lock` | reconciler flock; never unlinked |
| `roots/<db>.open` | `LOCK_SH` held by every process with the DB open |

The registry row stores the current DB filename and `meta.generation`. `root_id` never changes, even when the root moves.

### Roles

| Topic | Rule |
|---|---|
| Reconciler | holds `flock(LOCK_EX\|LOCK_NB)` on `<db>.lock`; one per root and schema. Sole writer: startup reconcile, watcher, FTS, convention inference, full diagnostics. Each transaction `BEGIN IMMEDIATE`, `busy_timeout = 2s` |
| Peers | read-only, never open a writer connection. Per request check `PRAGMA data_version` (~1.2 µs) and drop hot caches on change; also compare DB inode and `meta.generation` with values at open and reopen on change (catches deletion, cache purge, rebuild). `change_log.seq` restarts per generation, so a reopening peer drops its cursor |
| Peer saves | parsed into the peer's in-memory overlay; reach the DB via the reconciler's watcher (~100–500 ms) or next sweep. Overlay dropped once the DB row's hash matches. Re-parse rule: [index spec](index.md) |
| Open buffers | per process, laid over DB results; never written before save, never by a peer. Two editors see their own unsaved text |
| `SQLITE_BUSY` | readers retry (possible during WAL recovery after a crash). Reconciler runs `wal_checkpoint(PASSIVE)` periodically and sets `journal_size_limit`, because a leaked reader blocks checkpoints |
| Takeover | the kernel releases the flock on exit or crash. Peers retry `LOCK_NB` every 5 s and on any request that sees a stale `meta.reconciled_at`; a peer wanting freshness with no holder takes it. No heartbeat, PID check or stale-lock cleanup |
| Stuck reconciler | alive but stuck (SIGSTOP, hung `stat` on Eden/NFS) keeps the lock. A peer seeing `reconciled_at` older than the sweep interval while the lock is taken logs one `window/logMessage` warning and point-checks its open files and their link targets in memory, writing nothing. Others' saves and background work wait |

### File events

- Only the reconciler acts on native watcher events; peers act only on their own `didOpen`/`didChange`/`didSave`, in overlays. Otherwise N editors mean N parses per save.
- Client `didChangeWatchedFiles` (Neovim registers it on macOS and Windows only) is used only by the reconciler and only when it runs no native watcher (lazy mode, `watch`-less build).
- macOS uses `fsevent-sys`, because `notify` hard-codes `kFSEventStreamEventIdSinceNow`. Replay from `meta.fsevents_last_id`; `MustScanSubDirs`, `UserDropped`, `KernelDropped`, `EventIdsWrapped` fall back to a `dir_state` diff of the subtree (or root). The new ID is committed after `HistoryDone`, in the same transaction as its batch.
- **Takeover gap** (up to 5 s unwatched): macOS replays FSEvents from the last committed ID; Linux runs one `dir_state` diff before starting inotify.

### DB lifetime: never unlink an open DB

Unlinking or renaming an open SQLite file is a documented corruption path.

- Every process holds `LOCK_SH` on `<db>.open` while any `<db>*.db` is open.
- GC, schema cleanup and corruption cleanup take `LOCK_EX|LOCK_NB` on `<db>.open`, skip on failure, and delete the DB with its `-wal` and `-shm`.
- **Corruption** (`quick_check` fails at reconciler start, or `SQLITE_CORRUPT`/`SQLITE_NOTADB`): the reconciler never renames; it builds `<db>-<gen8>.db` with a new `meta.generation` and repoints the registry row in one transaction. Peers re-read the row on their 5 s timer and on corruption errors, and reopen. GC deletes the old file later.
- `<db>.lock` is never unlinked (a waiter on an unlinked lock would elect a second reconciler). `.lock` and `.open` are removed only when the whole root is GC-ed, under `LOCK_EX` on both. After taking either lock, a process checks `stat` vs `fstat` and retries if the path names another inode.
- The registry is never deleted automatically; an old `roots.v<k-1>.db` is a few KB.

### Thundering herd

- **Discovery is serialised** under `discover.lock` (the per-root flock cannot help before the root is known). A process that does not get it serves single-file features, waits on a background thread, then re-checks the registry. One process walks; the rest find the row. This is also the global walk semaphore: one discovery walk per user at a time.
- One process wins the root flock; the others serve immediately from the index plus overlays. None waits on a write lock, because none writes.
- A new root's DB is created by the flock holder only (`CREATE ... IF NOT EXISTS` in `BEGIN IMMEDIATE`). With no DB it indexes at once, open buffer first; the ~300 ms start delay applies only to background sweeps of an existing DB ([index spec](index.md)). Peers publish diagnostics per document once that document and its targets are checked locally, without waiting for `meta.initial_index_done`.
- Rate-only lazy verdicts need two measurements (§1 stage 4), so herd slowness does not stick.

### Versions, memory, scheduling

- **Upgrades**: schema version in the DB filename and locks, so v2 and v3 processes never fight over migrations. A v2 file is GC-ed after 7 days once `<id>.v2.open` can be taken exclusively. The registry carries its own `k` for the same reason.
- **Memory**: < 35 MB `phys_footprint` per process with N=10 concurrent instances (buffers, overlays, hot cache, page caches of the capped read pool, plus the writer connection in the reconciler). SQLite is `mmap`ed, so N processes share one page-cache copy; on macOS those pages show in every process's RSS, so measure with `footprint` or `proc_pid_rusage` (`ri_phys_footprint`), not RSS. For comparison, 10 marksman instances are 10 full workspaces at 134–145 MB RSS each.
- **Scheduling**: background work (walks, sweeps, FTS, inference, vcs-enumerated parsing) at `QOS_CLASS_BACKGROUND` (E-cores only). `QOS_CLASS_UTILITY` only for the small link-target queue of open buffers (it prefers P-cores). Requests at default QoS. Linux: `nice 10` + `IOPRIO_CLASS_IDLE` for background, best-effort for the link-target queue.

## 6. Housekeeping

Registry columns: `root_id, path, kind (marker|vcs|tracked|loose|single|lazy|vcs-enumerated), marker, marker_ino, volume_uuid, st_dev, fs_type, db_file, generation, entries, md_files, dirs, walk_ms, ms_per_dir, verdict_source (fs|eden|budget|rate|user), rate_confirmations, decided_at, decision_reason, last_seen, schema`.

- **GC**: at most daily, claimed by the process that updates `meta.gc_at` in a `BEGIN IMMEDIATE` registry transaction. Candidates: roots not seen for 30 days or whose path is gone (after the move check of §1), old-schema DBs older than 7 days, stale generations. Each needs `LOCK_EX|LOCK_NB` on `<db>.open`, else skipped until the next run.
- **OS cache purge** (macOS clears `~/Library/Caches` under disk pressure; cleaners) is treated as deletion: peers see the inode/generation change and reopen, the next reconciler rebuilds; without a registry, discovery reruns under `discover.lock`.
- **`mdroots roots`** lists roots with mode, counts, walk time, base dir and why, e.g. "lazy: statfs edenfs:, MNT_LOCAL unset", "single-file: inside a local mount in an Eden repo", "loose root rejected at <dir>: 4 md (< 20), adds 2 md (≤ 2 in child)", "lazy pending: rate 7.1 ms/dir, 1 of 2 measurements". The same line goes to `window/logMessage` on first open; it is the main way to debug a wrong root.
- **Escape hatches** (optional): empty `.mdroots` forces a root; `.mdrootsignore` excludes paths and stops loose climbing; `MDROOTS_LAZY=path1:path2` forces lazy.

## 7. Fixtures

Each uses a temporary `XDG_CACHE_HOME`. "Zero readdir" is checked with an instrumented walker or `fs_usage -f filesys`. Timings logged.

**Discovery**

1. A file in a no-VCS `scratch/` dir inside a projects folder of ~40 repos: loose root `scratch/`, climb rejected by (a) and (b); also on a synthetic copy without the tarball tree (16 files, 3 md). A file inside any of the repos gets that repo.
2. A neovim checkout (3.8k files, 13 md): budgeted walk.
3. Synthetic 10k-md folder, no VCS: loose root accepted by (a).
4. A `.zk` notebook inside a git repo.
5. A file in a large EdenFS monorepo: lazy from `statfs` before any marker stat, root from `readlink(.eden/root)`, zero readdir outside the file's dir.
6. A file in a build-output mount inside that checkout: lazy or single-file, zero readdir outside the file's dir, no registry row at the mount.
7. A small notes repo on EdenFS: `vcs-enumerated` within 500 ms, `[[stem]]` resolves, zero readdir outside opened dirs.
8. A dataless cloud file (iCloud after `brctl evict`, or Drive online-only), opened and linked: link existence works, not read during the walk, still `SF_DATALESS` afterwards.
9. A dotfiles repo at a temp `HOME` with a large untracked tree: `tracked-only`, zero walk.
10. `git clone` into an existing lazy root, open a file in it: stage 1 probe registers a nested root.
11. `mv notes notes2` between sessions: row re-keyed by marker inode + volume UUID, no cold rebuild.

**Many processes**

12. 10 processes on fixture 3 with an existing DB: one reconciler and writer; peers never open a writer connection; no `SQLITE_BUSY` reaches requests; all converge on one `data_version`; a save shows in other editors within watcher latency.
13. Cold herd (no registry, no DB, `sudo purge`), 10 processes on a loose root: one discovery walk, one registry row, no overlaps, no rate-only lazy verdict saved; every `phys_footprint` < 35 MB.
14. GC with 3 peers holding an aged DB open: skipped, peers keep answering, `integrity_check` passes. Repeat for an old schema file and a stale generation.
15. Corrupt the DB under 3 peers: new generation, peers reopen, old file deleted only after the last peer exits.
16. `rm -rf` the base dir under 3 processes: all reopen, one rebuilds, nobody writes to an unlinked file.
17. mtime-regressing copy (`cp -p`, `rsync -a`, `tar x` of an older version): new content indexed (ctime advanced); a bare `touch` does not re-parse links.
18. `kill -STOP` the reconciler, save in a peer: one warning, peer fresh from its overlay, no DB writes. `kill -CONT`: reconciler picks up the save. `kill -9`: a peer takes over and re-checks its working set.
19. Kill the reconciler, change files within 5 s, peer takes over: changes arrive via FSEvents replay (macOS) or the takeover `dir_state` diff (Linux).
