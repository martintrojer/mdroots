#!/usr/bin/env python3
"""Benchmark mdroots, zk and marksman as language servers on generated notebooks.

usage: run.py [--work DIR] [--runs N] [--mdroots PATH] [SIZE ...]

SIZE is a note count such as 200, 1k or 10k (default: 1k 10k). Everything
happens inside the work dir (default: a fresh temp dir): the notebooks
(gen.py), mdroots' cache dirs, a redirected HOME, XDG_* dirs and TMPDIR, and
results.jsonl, one JSON object per run. When the work dir is given it is
reused, so an existing notebook of the same size is kept; pass a fresh dir
for a clean run. A non-empty dir that run.py did not create (it has no
.mdroots-compare marker) is refused. The servers are only ever pointed at notebooks this script
generated inside the work dir.

Per size there are N rounds (default 5). Each round runs every config once,
interleaved, so drift on a loaded machine hits all configs alike:

  mdroots-cold   `mdroots lsp` on an empty MDROOTS_CACHE_DIR
  mdroots-warm   a second `mdroots lsp` on the same cache dir
  marksman       `marksman server` (no persistent state, always cold)
  zk-noindex     .zk/notebook.db deleted, then `zk lsp` (indexes first)
  zk-index       .zk/notebook.db deleted, then `zk index` timed
  zk-index-noop  `zk index` again, nothing changed
  zk             `zk lsp` on the index just built

zk, marksman and mdroots (or --mdroots) must be on PATH. Print the tables
with `report.py WORK/results.jsonl`; run.py prints them at the end.
"""
import argparse, json, os, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
LSPBENCH = os.path.join(HERE, '..', 'lspbench.py')

ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
ap.add_argument('--work', help='work dir (default: a fresh temp dir)')
ap.add_argument('--runs', type=int, default=5, help='rounds per size (default 5)')
ap.add_argument('--mdroots', default='mdroots', help='mdroots binary (default: mdroots on PATH)')
ap.add_argument('--timeout', type=float, default=120, help='lspbench --timeout per server (default 120 s)')
ap.add_argument('--init-timeout', type=float, default=400, help='lspbench --init-timeout (default 400 s; zk lsp with no index indexes before it answers)')
ap.add_argument('sizes', nargs='*', default=['1k', '10k'])
a = ap.parse_args()

def count(size):
    s = size.lower()
    return int(float(s[:-1]) * 1000) if s.endswith('k') else int(s)

for tool in (a.mdroots, 'zk', 'marksman'):
    if not shutil.which(tool): sys.exit(f'{tool}: not found on PATH')
MD = os.path.abspath(shutil.which(a.mdroots))

work = os.path.realpath(a.work) if a.work else tempfile.mkdtemp(prefix='mdroots-compare-')
os.makedirs(work, exist_ok=True)
MARKER = os.path.join(work, '.mdroots-compare')
if os.listdir(work) and not os.path.exists(MARKER):
    sys.exit(f'refusing: {work} is not empty and was not created by run.py')
open(MARKER, 'a').close()

def inside(path):
    path = os.path.realpath(path)
    return os.path.commonpath([path, work]) == work and path != work

# Isolation: every tool sees a HOME, XDG dirs and TMPDIR inside the work dir.
home = os.path.join(work, 'home')
iso = {'HOME': home, 'DOTNET_CLI_HOME': home, 'TMPDIR': os.path.join(work, 'tmp')}
for k in ('CONFIG', 'DATA', 'CACHE', 'STATE'):
    iso[f'XDG_{k}_HOME'] = os.path.join(work, 'xdg', k.lower())
for d in iso.values(): os.makedirs(d, exist_ok=True)
ENV = dict(os.environ, **iso)
ENV.pop('MDROOTS_CACHE_DIR', None)
ENV.pop('ZK_NOTEBOOK_DIR', None)  # zk would otherwise fall back to a notebook outside the work dir

results = os.path.join(work, 'results.jsonl')
out = open(results, 'a')
print(f'work dir: {work}', flush=True)
print(f'versions: {subprocess.run([MD, "--version"], capture_output=True, text=True).stdout.strip()}; '
      f'zk {subprocess.run(["zk", "--version"], capture_output=True, text=True).stdout.strip()}; '
      f'marksman {subprocess.run(["marksman", "--version"], capture_output=True, text=True).stdout.strip()}', flush=True)

def record(d):
    out.write(json.dumps(d) + '\n'); out.flush()

def bench(cfg, size, run, nb, cmd, env=None):
    assert inside(nb), nb
    r = subprocess.run([sys.executable, LSPBENCH, '--json', '--timeout', str(a.timeout),
                        '--init-timeout', str(a.init_timeout), '--cmd', cmd, nb, 'note-00000.md'],
                       capture_output=True, text=True, env=dict(ENV, **(env or {})))
    d = json.loads(r.stdout) if r.returncode == 0 else {'error': r.stderr[-300:]}
    d.update(cfg=cfg, size=size, run=run)
    record(d)
    m = d.get('methods', {})
    print(cfg, size, run, d.get('init_ms'), {k: v.get('t', v.get('status')) for k, v in m.items()},
          d.get('rss_peak_mb'), d.get('phys_footprint_mb'), d.get('error', ''), flush=True)

def timed(cfg, size, run, argv, nb):
    assert inside(nb), nb
    s = time.time(); r = subprocess.run(argv, cwd=nb, capture_output=True, text=True, env=ENV); t = time.time() - s
    d = {'cfg': cfg, 'size': size, 'run': run, 'wall_ms': round(t * 1000, 1), 'rc': r.returncode, 'stderr': r.stderr[-200:]}
    record(d); print(cfg, size, run, d['wall_ms'], flush=True)

def drop_zk_db(nb):
    db = os.path.join(nb, '.zk', 'notebook.db')
    for f in (db, db + '-wal', db + '-shm'):
        if os.path.exists(f): os.unlink(f)

for size in a.sizes:
    n = count(size)
    nb = os.path.join(work, f'nb-{size}')
    if not inside(nb): sys.exit(f'refusing: {nb} is outside the work dir {work}')
    if not os.path.exists(os.path.join(nb, 'note-00000.md')):
        subprocess.run([sys.executable, os.path.join(HERE, 'gen.py'), str(n), nb], check=True, env=ENV)
    for f in os.listdir(nb):  # page cache warm for everyone
        if f.endswith('.md'):
            with open(os.path.join(nb, f), 'rb') as fh: fh.read()
    for run in range(1, a.runs + 1):
        cache = os.path.join(work, f'cache-{size}-{run}')
        shutil.rmtree(cache, ignore_errors=True)
        env = {'MDROOTS_CACHE_DIR': cache}
        bench('mdroots-cold', size, run, nb, f'{MD} lsp', env)
        bench('mdroots-warm', size, run, nb, f'{MD} lsp', env)
        bench('marksman', size, run, nb, 'marksman server')
        drop_zk_db(nb)
        bench('zk-noindex', size, run, nb, 'zk lsp')
        drop_zk_db(nb)
        timed('zk-index', size, run, ['zk', 'index', '--no-input', '-q'], nb)
        timed('zk-index-noop', size, run, ['zk', 'index', '--no-input', '-q'], nb)
        bench('zk', size, run, nb, 'zk lsp')

out.close()
subprocess.run([sys.executable, os.path.join(HERE, 'report.py'), results])
