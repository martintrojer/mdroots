#!/usr/bin/env python3
"""Print markdown tables from run.py's results.jsonl: median [min–max] over runs.

usage: report.py WORK/results.jsonl
"""
import json, statistics, sys
from collections import defaultdict

if len(sys.argv) != 2: sys.exit(__doc__.strip())
rows = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
by = defaultdict(list)
for r in rows: by[(r['cfg'], r['size'])].append(r)

def fmt(xs, unit):
    xs = [x for x in xs if x is not None]
    if not xs: return '—'
    m = statistics.median(xs)
    f = (lambda v: f'{v / 1000:.2f} s') if unit == 'ms' and m >= 1000 else (lambda v: f'{v:.0f} ms' if unit == 'ms' else f'{v:.0f} MB')
    if unit == 'ms' and m >= 1000: return f'{f(m)} [{min(xs)/1000:.2f}–{max(xs)/1000:.2f}]'
    return f'{f(m)} [{min(xs):.0f}–{max(xs):.0f}]'

def cell(cfg, size, key):
    rs = by.get((cfg, size), [])
    if not rs: return 'n/a'
    if key in ('definition', 'completion', 'documentSymbol', 'workspaceSymbol', 'completion_first'):
        meth = 'completion' if key == 'completion_first' else key
        sts = {r['methods'][meth]['status'] for r in rs if 'methods' in r}
        if sts == {'nocap'}: return 'not supported'
        if sts != {'ok'}: return '/'.join(sorted(sts))
        f = 'first_nonempty' if key == 'completion_first' else 't'
        return fmt([r['methods'][meth].get(f) for r in rs if 'methods' in r], 'ms')
    if key == 'wall_ms': return fmt([r['wall_ms'] for r in rs], 'ms')
    if key == 'diag_broken_t': return fmt([r.get('diag_broken_t') for r in rs], 'ms')
    if key in ('rss_peak_mb', 'phys_footprint_mb', 'phys_footprint_peak_mb'): return fmt([r.get(key) for r in rs], 'MB')
    if key == 'init_ms': return fmt([r.get('init_ms') for r in rs], 'ms')
    if key == 'diag_open_n':
        v = sorted({str(r.get('diag_open_n', '-')) for r in rs}); return '/'.join(map(str, v))
    if key == 'diag_files':
        v = sorted({str(r.get('diag_files', '-')) for r in rs}); return '/'.join(map(str, v))
    if key == 'n':
        return '/'.join(str(sorted({r['methods'][m].get('n', '-') for r in rs}) ) for m in ('completion',))

CFGS = [('mdroots-cold', 'mdroots cold'), ('mdroots-warm', 'mdroots warm'), ('zk', 'zk (indexed)'),
        ('zk-noindex', 'zk (no index)'), ('marksman', 'marksman')]
MEASURES = [
    ('init_ms', '`initialize` response'),
    ('definition', 'first definition (correct target)'),
    ('completion_first', 'first non-empty completion'),
    ('completion', 'first full completion (≥10 items)'),
    ('documentSymbol', 'first documentSymbol'),
    ('workspaceSymbol', 'first workspaceSymbol ("Note 12")'),
    ('diag_broken_t', 'broken-link error on open page'),
    ('diag_open_n', 'diagnostics on probe page (count)'),
    ('diag_files', 'files with published diagnostics'),
    ('rss_peak_mb', 'peak RSS (sampled 20 ms)'),
    ('phys_footprint_mb', 'phys_footprint at end'),
    ('phys_footprint_peak_mb', 'phys_footprint_peak'),
]
def notes(size):
    s = size.lower()
    return int(float(s[:-1]) * 1000) if s.endswith('k') else int(s)

for size in sorted({s for _, s in by}, key=notes):
    n = len(by.get(('mdroots-cold', size), []))
    print(f'\n### {notes(size):,} notes (median [min–max] of {n} runs; times are ms since spawn)\n')
    print('| measure | ' + ' | '.join(h for _, h in CFGS) + ' |')
    print('|---|' + '---|' * len(CFGS))
    for k, h in MEASURES:
        print(f'| {h} | ' + ' | '.join(cell(c, size, k) for c, _ in CFGS) + ' |')
    print(f'| `zk index` from scratch | | | {cell("zk-index", size, "wall_ms")} | | |')
    print(f'| `zk index` no-op | | | {cell("zk-index-noop", size, "wall_ms")} | | |')
