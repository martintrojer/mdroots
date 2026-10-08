"""Time an LSP server from `initialize` to the first non-empty result per method.

Usage:
  python3 lspbench.py --cmd "zk lsp" --skip documentSymbol,workspaceSymbol <vault> README.md
  python3 lspbench.py --cmd "marksman server" --log /tmp/marksman.jsonl <vault> README.md

Per-method result: `<ms>ms(n=<items>)` on success, otherwise
  unsupported  server returned an error, or still null when the timeout ran out
  empty        server kept returning an empty result until the timeout
  timeout      no response at all within the timeout
  skipped      excluded with --skip
Empty or null answers are retried until --timeout (the index may still be
loading); errors are not retried. RSS includes child processes.
"""
import argparse, json, os, queue, re, shlex, subprocess, threading, time

METHODS = {  # short name -> LSP method
    'documentSymbol': 'textDocument/documentSymbol',
    'definition': 'textDocument/definition',
    'completion': 'textDocument/completion',
    'workspaceSymbol': 'workspace/symbol',
}

ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
ap.add_argument('--cmd', required=True, help='server command line, e.g. "zk lsp"')
ap.add_argument('--skip', default='', help='comma-separated methods to skip: ' + ','.join(METHODS))
ap.add_argument('--timeout', type=float, default=5.0, help='seconds per request, retries included (default 5)')
ap.add_argument('--log', help='append JSON-RPC traffic to this file, one JSON object per line')
ap.add_argument('--query', default='ai', help='workspace/symbol query (default "ai")')
ap.add_argument('root')
ap.add_argument('doc', help='document path relative to root')
a = ap.parse_args()

skip = {s for s in a.skip.split(',') if s}
bad = skip - METHODS.keys()
if bad: ap.error(f"unknown --skip method(s): {','.join(sorted(bad))}")

root = os.path.abspath(os.path.expanduser(a.root))
cmd = shlex.split(a.cmd)
logf = open(a.log, 'a') if a.log else None
t0 = time.time()

def log(direction, msg):
    if logf:
        logf.write(json.dumps({'t_ms': round((time.time() - t0) * 1000, 1), 'dir': direction, 'msg': msg}) + '\n')
        logf.flush()

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
        log('<-', m)
        q.put(m)
threading.Thread(target=rd, daemon=True).start()

def write(msg):
    log('->', msg)
    b = json.dumps(msg).encode()
    p.stdin.write(b'Content-Length: %d\r\n\r\n' % len(b) + b); p.stdin.flush()

nid = [0]
def send(method, params, notif=False):
    msg = {'jsonrpc': '2.0', 'method': method, 'params': params}
    if not notif: nid[0] += 1; msg['id'] = nid[0]
    write(msg)
    return msg.get('id')

diags = {}
def wait(i, timeout):
    """Return the response to request i, or None after timeout. Handles notifications and server requests meanwhile."""
    end = time.time() + timeout
    while True:
        left = end - time.time()
        if left <= 0: return None
        try: m = q.get(timeout=left)
        except queue.Empty: return None
        if m.get('method') == 'textDocument/publishDiagnostics':
            diags[m['params']['uri']] = len(m['params']['diagnostics'])
        if m.get('id') == i and 'method' not in m: return m
        if 'method' in m and 'id' in m:  # server request -> reply null
            write({'jsonrpc': '2.0', 'id': m['id'], 'result': None})

uri = 'file://' + os.path.join(root, a.doc)
text = open(os.path.join(root, a.doc)).read()
init = wait(send('initialize', {
    'processId': os.getpid(), 'rootUri': 'file://' + root,
    'workspaceFolders': [{'uri': 'file://' + root, 'name': os.path.basename(root)}],
    'capabilities': {'textDocument': {'definition': {'linkSupport': True},
                                      'completion': {'completionItem': {'snippetSupport': True}}},
                     'workspace': {'workspaceFolders': True}}}), max(a.timeout, 30))
if init is None:
    p.kill(); raise SystemExit(f"{cmd[0]}: no initialize response")
t_init = time.time() - t0
send('initialized', {}, True)
send('textDocument/didOpen', {'textDocument': {'uri': uri, 'languageId': 'markdown', 'version': 1, 'text': text}}, True)

pos = None  # first wiki link in the document
for i, l in enumerate(text.splitlines()):
    m = re.search(r'\[\[([^\]|#]+)', l)
    if m: pos = {'line': i, 'character': m.start() + 3}; break

def count(v):
    if isinstance(v, list): return len(v)
    if isinstance(v, dict) and 'items' in v: return len(v['items'])
    return 1

res = {}
def timed(name, params):
    if name in skip: res[name] = 'skipped'; return
    if params is None: res[name] = 'no-link'; return
    s = time.time(); last = 'timeout'
    while True:
        left = s + a.timeout - time.time()
        if left <= 0: break
        r = wait(send(METHODS[name], params), left)
        if r is None: last = 'timeout'; break
        if 'error' in r: last = 'unsupported'; break
        v = r.get('result')
        if v is None: last = 'unsupported'
        elif count(v) == 0: last = 'empty'
        else:
            res[name] = f"{round((time.time() - t0) * 1000)}ms(n={count(v)})"; return
        time.sleep(0.1)
    res[name] = last

doc_id = {'textDocument': {'uri': uri}}
timed('documentSymbol', doc_id)
timed('definition', pos and {**doc_id, 'position': pos})
timed('completion', pos and {**doc_id, 'position': pos})
timed('workspaceSymbol', {'query': a.query})
time.sleep(1.5); wait(-1, 0.5)  # let diagnostics arrive

def rss_mb(pid):
    out = subprocess.run(['ps', '-o', 'rss=', '-p', str(pid)], capture_output=True, text=True).stdout.strip()
    return int(out or 0) // 1024
try:
    rss = rss_mb(p.pid) + sum(rss_mb(k) for k in subprocess.run(['pgrep', '-P', str(p.pid)], capture_output=True, text=True).stdout.split())
except Exception: rss = '?'
print(f"{cmd[0]:9} {os.path.basename(root):5} init={t_init * 1000:.0f}ms "
      + ' '.join(f"{k}={v}" for k, v in res.items())
      + f" diags_files={len(diags)} diags_open={diags.get(uri, '-')} rss={rss}MB")
p.kill()
if logf: logf.close()
