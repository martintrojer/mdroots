# gopls: what makes it fast, and what mdroots copies

Source: [gopls](https://go.dev/gopls) v0.20.0 source (paths relative to
`internal/`), the v0.21.1 binary run as a daemon on a `/tmp` socket, the
[daemon docs](https://go.dev/gopls/daemon) and the
[scalability post](https://go.dev/blog/gopls-scalability).

gopls's speed comes from its cache and scheduling design, not its daemon.
The daemon (`-remote=auto`) is off by default, documented as new, not
enabled by [vscode-go](https://github.com/golang/vscode-go), and a maintainer called making it the default "too
early" (golang/go#81721).

## Speed practices

| Practice | gopls | Evidence |
|---|---|---|
| Immutable snapshots | each change clones the snapshot (persistent copy-on-write maps), fine-grained invalidation | `cache/snapshot.go` |
| Cancel stale work | a new snapshot cancels the previous one's context and in-flight requests | `cache/view.go` |
| Overlays | overlay FS over a memoized disk FS; file identity = URI + content hash | `fs_overlay.go`, `fs_memoized.go` |
| Recent-mtime guard | files modified < 2 s ago are not cached (coarse mtimes) | `fs_memoized.go` |
| I/O cap | 128-slot read semaphore | `fs_memoized.go` |
| Memoization | cancellable promises; if the computing context is cancelled another waiter takes over | `util/memoize`, `cache/future.go` |
| Persistent cache | `UserCacheDir/gopls/<exe hash>/`, content-addressed, integrity check on read, cache error = miss, mtime touched at most hourly | `filecache/filecache.go` |
| Cache GC | any process GCs all versions; every 5 min active, up to 6 h idle; delete after 5 days unused; 1 GB budget | `filecache.go` |
| Memory | open packages and direct imports in memory, the rest serialized (−75% in v0.12) | scalability blog |
| Request order | in order by default; three handlers opt out to run concurrently | `protocol/protocol.go`, `jsonrpc2/handler.go` |
| Cancellation | `$/cancelRequest` cancels the context; already-cancelled requests get `RequestCancelled` | `protocol.go` |
| Two-phase diagnostics | changed open files at once; everything after `DiagnosticsDelay` (1 s); each edit cancels the previous pass; optional save trigger | `server/diagnostics.go` |
| Progress | `workDoneProgress` with `showMessage` fallback | `progress/progress.go` |
| File watching | LSP mode relies on client `didChangeWatchedFiles` (register new before unregistering old) | `server/general.go` |
| Zero-config views | a view per workspace folder plus nearest `go.work`/`go.mod` for uncovered files; best view memoized per URI; no new view for files reached outside the workspace | `cache/session.go` |
| Early start | file-cache warm-up runs concurrently with launch | `cmd/cmd.go` |

## Adopted

Built in `mdroots lsp` ([library spec §4](../specs/library.md#4-scheduling)):

1. Requests run in order, so `didChange` → query ordering holds without locks.
2. `$/cancelRequest` marks a queued request cancelled and sets the running
   one's cancel token; a `didChange` also cancels the queued requests on
   that document.
3. Diagnostics are debounced: one pass, 500 ms after the last edit.
4. Root-per-URI is memoized (`Workspaces` caches a workspace per root).
   Requests never open a root: a file reached outside any open root is
   served in single-file mode, and an opened document stays single-file
   until its root has opened in the background.
5. Cache files are version-namespaced; any process may GC old versions
   under a size and age budget, and stamps a root's last-seen at most
   hourly. A cache dir or registry that does not open falls back to an
   in-memory index, and a corrupt per-root DB is rebuilt.
6. `workDoneProgress` with a `showMessage` fallback, for opens over 1 s.

Adopted as target, not built ([ROADMAP](../ROADMAP.md)):

1. A new snapshot (edit or `change_log` seq) cancels the previous view's
   work, including the pending diagnostics pass.
2. Two-phase diagnostics: the edited document at once, the cross-file pass
   after ~500 ms, plus a save-only trigger.
3. Slow requests (workspace symbols, full text, pull diagnostics) run
   concurrently on a small worker pool; everything else stays in order.
4. The cheap mtime scan treats files modified < 2 s ago as maybe changed.
5. A global read semaphore, for NFS and virtual filesystems such as
   [EdenFS](https://github.com/facebook/sapling) from the
   [Sapling](https://sapling-scm.com/) project.
6. The DB opens concurrently with the LSP handshake (the server starts
   opening a root on the first `didOpen` of a file in it).
7. Any cache read error after open is a miss.
8. Where client watchers are re-registered, the new ones are registered
   before the old ones are unregistered (the server registers its watchers
   once and never unregisters them).

## Skipped

| gopls feature | Why not |
|---|---|
| Lock-free content-addressed file cache | [SQLite](https://sqlite.org) WAL gives atomicity |
| GC ballast | Go-specific |
| Memoize/future graph | no type-check graph to share |
| Client-only file watching | [Neovim](https://neovim.io) registers watchers on macOS/Windows only; the reconciler's own watcher stays |
| Daemon | see below and D3 in [DECISIONS.md](../DECISIONS.md) |

## The daemon's flaws (why D3 has no daemon)

Observed in the source and by experiment; any future daemon would have to fix them:

- **Start-up race:** probe, `os.Remove(socket)` and bind have no lock (the source has a "TODO: there is probably a race here"). 8 cold forwarders at once started 8 daemons; 7 died on bind, and remove-then-bind can orphan a live daemon.
- **Crash takes everyone down:** forwarders exit with `remote disconnected`; no reconnect, no fallback.
- **No version check:** the handshake only logs a binary mismatch.
- **Ownership check fails open:** a stat error skips the uid comparison; the socket is not chmodded and relies on a `0700` TMPDIR.
- **Environment:** the daemon has the starter's environment, so forwarders must inject their own `go env` on `initialize`.
