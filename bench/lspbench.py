"""Time an LSP server from spawn to the first useful result per method.

Usage:
  python3 lspbench.py --cmd "zk lsp" <notebook> note.md
  python3 lspbench.py --json --cmd "marksman server" --log traffic.jsonl <notebook> note.md

What it measures:
  * per method (definition, completion, documentSymbol, workspaceSymbol):
    `t`, ms since spawn of the first useful answer (the wait an editor user
    sees), and `lat`, ms of the request that succeeded
  * `init_ms`, ms since spawn of the `initialize` response
  * `diag_broken_t`, ms since spawn of the first error/warning diagnostic on
    the line of `[[missing-probe]]` in the opened page, and the last
    published diagnostics per file
  * peak RSS of the process and its children, sampled with `ps` every 20 ms
  * on macOS, phys_footprint and phys_footprint_peak from `footprint -j`,
    taken just before shutdown (null elsewhere)

A method missing from the server's `initialize` capabilities is reported
`nocap` and never sent. Pending methods are retried round-robin every 20 ms
until each is useful or --timeout runs out: definition must point at the
linked note, completion (at a bare trailing `[[`) needs --min-completion
items, the others any item. Other statuses: `error:<message>`, `timeout`
(no answer), `null`, `empty`, `partial(n=...)` (last answer seen), and
`skipped` (--skip). The run ends with a clean `shutdown` and `exit`, so
servers that persist state on exit (mdroots) do so.
"""
import argparse, json, os, queue, re, shlex, subprocess, tempfile, threading, time

METHODS = {  # short name -> (LSP method, capability key)
    'definition': ('textDocument/definition', 'definitionProvider'),
    'completion': ('textDocument/completion', 'completionProvider'),
    'documentSymbol': ('textDocument/documentSymbol', 'documentSymbolProvider'),
    'workspaceSymbol': ('workspace/symbol', 'workspaceSymbolProvider'),
}

ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
ap.add_argument('--cmd', required=True, help='server command line, e.g. "zk lsp"')
ap.add_argument('--skip', default='', help='comma-separated methods to skip: ' + ','.join(METHODS))
ap.add_argument('--timeout', type=float, default=30.0, help='seconds per method, retries included')
ap.add_argument('--diag-wait', type=float, default=5.0, help='max seconds to wait for diagnostics of the open page')
ap.add_argument('--log', help='append JSON-RPC traffic to this file, one JSON object per line')
ap.add_argument('--query', default='Note 12', help='workspace/symbol query (default "Note 12")')
ap.add_argument('--min-completion', type=int, default=10, help='completion counts once it has this many items')
ap.add_argument('--init-timeout', type=float, default=60.0, help='seconds to wait for the initialize response')
ap.add_argument('--json', action='store_true', help='print one JSON line instead of indented JSON')
ap.add_argument('root', help='notebook directory (the server runs with it as cwd)')
ap.add_argument('doc', help='document path relative to root')
a = ap.parse_args()

skip = {s for s in a.skip.split(',') if s}
bad = skip - METHODS.keys()
if bad: ap.error(f"unknown --skip method(s): {','.join(sorted(bad))}")
root = os.path.abspath(os.path.expanduser(a.root))
cmd = shlex.split(a.cmd)
logf = open(a.log, 'a') if a.log else None
t0 = time.time()

def log(d, msg):
    if logf: logf.write(json.dumps({'t_ms': round((time.time() - t0) * 1000, 1), 'dir': d, 'msg': msg}) + '\n')

p = subprocess.Popen(cmd, cwd=root, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
q = queue.Queue()

def rd():
    f = p.stdout
    while True:
        h = {}
        while True:
            l = f.readline()
            if not l: return
            l = l.decode().strip()
            if not l: break
            k, v = l.split(':', 1); h[k.lower()] = v.strip()
        m = json.loads(f.read(int(h['content-length'])))
        log('<-', m); q.put(m)
threading.Thread(target=rd, daemon=True).start()

def pids():
    out = [p.pid]
    for k in subprocess.run(['pgrep', '-P', str(p.pid)], capture_output=True, text=True).stdout.split():
        out.append(int(k))
    return out

def rss_kb(ps):
    out = subprocess.run(['ps', '-o', 'rss=', '-p', ','.join(map(str, ps))], capture_output=True, text=True).stdout
    return sum(int(x) for x in out.split())

peak = [0]; sampling = [True]
def sampler():
    while sampling[0] and p.poll() is None:
        try: peak[0] = max(peak[0], rss_kb(pids()))
        except Exception: pass
        time.sleep(0.02)
threading.Thread(target=sampler, daemon=True).start()

def write(msg):
    log('->', msg)
    b = json.dumps(msg).encode()
    p.stdin.write(b'Content-Length: %d\r\n\r\n' % len(b) + b); p.stdin.flush()

nid = [0]
def send(method, params, notif=False):
    msg = {'jsonrpc': '2.0', 'method': method, 'params': params}
    if not notif: nid[0] += 1; msg['id'] = nid[0]
    write(msg); return msg.get('id')

diags = {}
diag_t = {}
def wait(i, timeout):
    end = time.time() + timeout
    while True:
        left = end - time.time()
        if left <= 0: return None
        try: m = q.get(timeout=left)
        except queue.Empty: return None
        if m.get('method') == 'textDocument/publishDiagnostics':
            diags[m['params']['uri']] = m['params']['diagnostics']
            if m['params']['uri'] == uri and 'diag_t' not in diag_t and any(
                    d['range']['start']['line'] == broken_line and d.get('severity', 1) <= 2 for d in m['params']['diagnostics']):
                diag_t['diag_t'] = round((time.time() - t0) * 1000, 1)
        if m.get('id') == i and 'method' not in m: return m
        if 'method' in m and 'id' in m:
            write({'jsonrpc': '2.0', 'id': m['id'], 'result': None})

uri = 'file://' + os.path.join(root, a.doc)
text = open(os.path.join(root, a.doc)).read()
broken_line = next((i for i, l in enumerate(text.splitlines()) if '[[missing-probe]]' in l), -1)
init = wait(send('initialize', {
    'processId': os.getpid(), 'rootUri': 'file://' + root, 'rootPath': root,
    'workspaceFolders': [{'uri': 'file://' + root, 'name': os.path.basename(root)}],
    'capabilities': {'textDocument': {'definition': {'linkSupport': True},
                                      'completion': {'completionItem': {'snippetSupport': True}},
                                      'documentSymbol': {'hierarchicalDocumentSymbolSupport': True},
                                      'publishDiagnostics': {}},
                     'workspace': {'workspaceFolders': True, 'symbol': {}}}}), a.init_timeout)
if init is None:
    p.kill(); raise SystemExit(f"{cmd[0]}: no initialize response")
t_init = time.time() - t0
caps = init.get('result', {}).get('capabilities', {})
send('initialized', {}, True)
send('textDocument/didOpen', {'textDocument': {'uri': uri, 'languageId': 'markdown', 'version': 1, 'text': text}}, True)

lines = text.splitlines()
pos = None  # first wiki link: definition target
for i, l in enumerate(lines):
    m = re.search(r'\[\[([^\]|#]+)', l)
    if m: pos = {'line': i, 'character': m.start() + 3}; target = m.group(1); break
cpos = None  # a bare trailing `[[`: completion point (no filter text)
for i, l in enumerate(lines):
    if l.endswith('[['): cpos = {'line': i, 'character': len(l)}

def count(v):
    if isinstance(v, list): return len(v)
    if isinstance(v, dict) and 'items' in v: return len(v['items'])
    return 1

res = {}
def useful(name, v):
    """A result that answers the question, not just any non-empty one."""
    if name == 'completion': return count(v) >= a.min_completion
    if name == 'definition':
        locs = v if isinstance(v, list) else [v]
        return any((l.get('uri') or l.get('targetUri', '')).endswith('/' + target + '.md') for l in locs)
    return count(v) > 0

doc_id = {'textDocument': {'uri': uri}}
PARAMS = {
    'definition': {**doc_id, 'position': pos},
    'completion': {**doc_id, 'position': cpos, 'context': {'triggerKind': 2, 'triggerCharacter': '['}},
    'documentSymbol': doc_id,
    'workspaceSymbol': {'query': a.query},
}
st = {}  # name -> state while pending
for name, (meth, cap) in METHODS.items():
    if name in skip: res[name] = {'status': 'skipped'}
    elif not caps.get(cap): res[name] = {'status': 'nocap'}
    else: st[name] = {'last': 'timeout', 'tries': 0, 'first': None}
# Round-robin: each round sends every pending method once (waiting for its
# answer), so one slow method does not delay the others' timestamps.
deadline = time.time() + a.timeout
while st and time.time() < deadline:
    for name in list(st):
        x = st[name]; rs = time.time(); x['tries'] += 1
        r = wait(send(METHODS[name][0], PARAMS[name]), max(deadline - rs, 0.01))
        now = time.time()
        if r is None: x['last'] = 'timeout'; continue
        if 'error' in r:
            res[name] = {'status': 'error:' + str(r['error'].get('message'))[:60], 'tries': x['tries']}; del st[name]; continue
        v = r.get('result')
        if v is None: x['last'] = 'null'; continue
        if count(v) == 0: x['last'] = 'empty'; continue
        x['first'] = x['first'] or round((now - t0) * 1000, 1)
        if not useful(name, v): x['last'] = f'partial(n={count(v)})'; continue
        res[name] = {'status': 'ok', 't': round((now - t0) * 1000, 1), 'lat': round((now - rs) * 1000, 1),
                     'n': count(v), 'tries': x['tries'], 'first_nonempty': x['first']}
        del st[name]
    if st: time.sleep(0.02)
for name, x in st.items():
    res[name] = {'status': x['last'], 'tries': x['tries'], 'first_nonempty': x['first']}
res = {k: res[k] for k in METHODS}

# diagnostics for the open page: wait until a non-empty set arrives (or diag-wait), then 0.5 s more
end = time.time() + a.diag_wait
while time.time() < end and not diags.get(uri):
    wait(-1, 0.1)
wait(-1, 0.5)

fp = {'phys_footprint_mb': None, 'phys_footprint_peak_mb': None}
try:
    jf = os.path.join(tempfile.gettempdir(), f'lspbench-fp-{p.pid}.json')
    subprocess.run(['footprint', '-j', jf, '--noCategories'] + sum((['-p', str(x)] for x in pids()), []),
                   capture_output=True, timeout=30)
    d = json.load(open(jf)); os.unlink(jf)
    fp['phys_footprint_mb'] = round(sum(x['auxiliary']['phys_footprint'] for x in d['processes']) / 2**20, 1)
    fp['phys_footprint_peak_mb'] = round(sum(x['auxiliary']['phys_footprint_peak'] for x in d['processes']) / 2**20, 1)
except Exception as e:
    fp['error'] = str(e)
rss_end = rss_kb(pids())
sampling[0] = False

# clean shutdown
sid = send('shutdown', None)
wait(sid, 10)
try: send('exit', None, True)
except Exception: pass
try: p.wait(15); exited = 'clean'
except subprocess.TimeoutExpired: p.kill(); exited = 'killed'

d_open = diags.get(uri, [])
out = {'cmd': a.cmd, 'root': root, 'init_ms': round(t_init * 1000, 1), 'methods': res,
       'diag_files': len(diags), 'diag_broken_t': diag_t.get('diag_t'), 'diag_open_n': len(d_open),
       'diag_open_msgs': sorted({f"L{x['range']['start']['line']} sev{x.get('severity')}: " + x.get('message', '')[:60] for x in d_open}),
       'rss_peak_mb': round(max(peak[0], rss_end) / 1024, 1), 'rss_end_mb': round(rss_end / 1024, 1),
       **fp, 'exit': exited, 'caps': {k: bool(caps.get(c)) for k, (_, c) in METHODS.items()}}
if a.json: print(json.dumps(out))
else: print(json.dumps(out, indent=1))
if logf: logf.close()
