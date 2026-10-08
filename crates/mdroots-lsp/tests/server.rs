//! The server in-process over `Connection::memory()`, on copies of
//! tests/corpus in temp dirs.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use mdroots::{NoEnumerator, Options};
use serde_json::{Value, json};
use tempfile::TempDir;

type ServeResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// A running server and the client end of its connection.
struct Client {
    conn: Connection,
    server: Option<JoinHandle<ServeResult>>,
    server_conn: Option<Connection>,
    next_id: i32,
}

impl Client {
    fn start() -> Client {
        let mut c = Client::new();
        c.spawn();
        c
    }

    /// A client whose server is not running yet: messages sent now are
    /// all queued when it starts.
    fn new() -> Client {
        let (server, client) = Connection::memory();
        Client {
            conn: client,
            server: None,
            server_conn: Some(server),
            next_id: 1,
        }
    }

    fn spawn(&mut self) {
        let conn = self.server_conn.take().unwrap();
        let opts = Options::default().enumerator(std::sync::Arc::new(NoEnumerator));
        self.server = Some(std::thread::spawn(move || {
            mdroots_lsp::serve_with(conn, opts)
        }));
    }

    /// Sends `initialize` with `capabilities`, then `initialized`; returns
    /// the initialize result.
    fn initialize(&mut self, capabilities: Value) -> Value {
        let r = self.request("initialize", json!({ "capabilities": capabilities }));
        self.notify("initialized", json!({}));
        r.result.expect("initialize result")
    }

    fn send_request(&mut self, method: &str, params: Value) -> RequestId {
        let id: RequestId = self.next_id.into();
        self.next_id += 1;
        let req = Request::new(id.clone(), method.to_owned(), params);
        self.conn.sender.send(req.into()).unwrap();
        id
    }

    fn request(&mut self, method: &str, params: Value) -> Response {
        let id = self.send_request(method, params);
        self.response(&id)
    }

    fn response(&self, id: &RequestId) -> Response {
        loop {
            match self.recv() {
                Message::Response(r) if r.id == *id => return r,
                _ => {}
            }
        }
    }

    fn notify(&self, method: &str, params: Value) {
        let n = Notification::new(method.to_owned(), params);
        self.conn.sender.send(n.into()).unwrap();
    }

    fn recv(&self) -> Message {
        self.conn
            .receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("a message from the server")
    }

    /// The next publishDiagnostics for `uri`.
    fn diagnostics(&self, uri: &str) -> Vec<Value> {
        loop {
            if let Message::Notification(n) = self.recv()
                && n.method == "textDocument/publishDiagnostics"
                && n.params["uri"] == uri
            {
                return n.params["diagnostics"].as_array().unwrap().clone();
            }
        }
    }

    fn open(&self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": {
                "uri": uri, "languageId": "markdown", "version": 1, "text": text } }),
        );
    }

    fn change(&self, uri: &str, version: i32, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({ "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": text }] }),
        );
    }

    /// `shutdown` then `exit`; returns what `serve_with` returned.
    fn shutdown(mut self) -> ServeResult {
        let r = self.request("shutdown", Value::Null);
        assert!(r.error.is_none(), "{r:?}");
        self.notify("exit", Value::Null);
        self.server.take().unwrap().join().unwrap()
    }
}

/// A temp copy of one corpus vault at `<tmp>/vault`.
struct Vault {
    _tmp: TempDir,
    dir: PathBuf,
}

impl Vault {
    fn corpus(name: &str) -> Vault {
        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap().join("vault");
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/corpus")
            .join(name);
        copy_dir(&src, &dir);
        Vault { _tmp: tmp, dir }
    }

    fn uri(&self, rel: &str) -> String {
        format!("file://{}", self.dir.join(rel).display())
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.dir.join(rel)).unwrap()
    }
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            fs::copy(e.path(), to).unwrap();
        }
    }
}

#[test]
fn negotiates_utf8_when_offered() {
    let mut c = Client::start();
    let r = c.initialize(json!({ "general": { "positionEncodings": ["utf-8", "utf-16"] } }));
    assert_eq!(r["capabilities"]["positionEncoding"], "utf-8");
    assert_eq!(r["serverInfo"]["name"], "mdroots");
    assert_eq!(r["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    c.shutdown().unwrap();
}

#[test]
fn falls_back_to_utf16() {
    let mut c = Client::start();
    let r = c.initialize(json!({}));
    let caps = &r["capabilities"];
    assert_eq!(caps["positionEncoding"], "utf-16");
    assert_eq!(caps["textDocumentSync"]["change"], 1);
    assert_eq!(caps["textDocumentSync"]["openClose"], true);
    assert_eq!(
        caps["completionProvider"]["triggerCharacters"],
        json!(["[", "(", "#", ":"])
    );
    assert_eq!(caps["renameProvider"]["prepareProvider"], true);
    assert_eq!(
        caps["executeCommandProvider"]["commands"],
        json!(["mdroots.backlinks", "mdroots.info", "mdroots.renameFile"])
    );
    for p in [
        "definitionProvider",
        "referencesProvider",
        "hoverProvider",
        "documentSymbolProvider",
        "workspaceSymbolProvider",
    ] {
        assert_eq!(caps[p], true, "{p}");
    }
    c.shutdown().unwrap();
}

#[test]
fn publishes_the_broken_link_then_clears_it_after_the_fix() {
    let v = Vault::corpus("zk-min");
    let mut c = Client::start();
    c.initialize(json!({ "general": { "positionEncodings": ["utf-8"] } }));
    let uri = v.uri("broken.md");
    let text = v.read("broken.md");
    c.open(&uri, &text);
    let d = c.diagnostics(&uri);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0]["severity"], 2);
    assert_eq!(d[0]["code"], "broken-link");
    assert_eq!(d[0]["source"], "mdroots");
    assert_eq!(
        d[0]["range"]["start"],
        json!({ "line": 2, "character": 15 })
    );

    let fixed = text.replace("(missing-note)", "(a.md)");
    let sent = Instant::now();
    c.change(&uri, 2, &fixed);
    assert_eq!(c.diagnostics(&uri), Vec::<Value>::new());
    assert!(sent.elapsed() >= Duration::from_millis(500), "debounced");
    c.shutdown().unwrap();
}

#[test]
fn a_later_change_restarts_the_debounce() {
    let v = Vault::corpus("zk-min");
    let mut c = Client::start();
    c.initialize(json!({}));
    let uri = v.uri("broken.md");
    let text = v.read("broken.md");
    c.open(&uri, &text);
    assert_eq!(c.diagnostics(&uri).len(), 1);
    c.change(&uri, 2, &text.replace("(missing-note)", "(a.md)"));
    std::thread::sleep(Duration::from_millis(250));
    let last = Instant::now();
    c.change(&uri, 3, &text);
    // One publish, of the last text, 500 ms after the last change.
    assert_eq!(c.diagnostics(&uri).len(), 1);
    assert!(last.elapsed() >= Duration::from_millis(500), "restarted");
    c.shutdown().unwrap();
}

#[test]
fn settings_off_publishes_nothing_and_hint_overrides() {
    let v = Vault::corpus("zk-min");
    let mut c = Client::start();
    c.initialize(json!({}));
    let uri = v.uri("broken.md");
    c.open(&uri, &v.read("broken.md"));
    assert_eq!(c.diagnostics(&uri).len(), 1);

    let set = |s: &str| json!({ "settings": { "mdroots": { "diagnostics": s } } });
    c.notify("workspace/didChangeConfiguration", set("off"));
    assert_eq!(c.diagnostics(&uri), Vec::<Value>::new());
    c.notify("workspace/didChangeConfiguration", set("hint"));
    let d = c.diagnostics(&uri);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0]["severity"], 4);
    c.shutdown().unwrap();
}

#[test]
fn a_file_not_on_disk_gets_no_diagnostics_and_no_error() {
    let v = Vault::corpus("zk-min");
    let mut c = Client::start();
    c.initialize(json!({}));
    c.open(&v.uri("new.md"), "[x](missing)");
    // The next message is the reply to this request, not diagnostics.
    let id = c.send_request("textDocument/hover", json!({}));
    match c.recv() {
        Message::Response(r) => assert_eq!(r.id, id),
        m => panic!("unexpected {m:?}"),
    }
    c.shutdown().unwrap();
}

#[test]
fn a_cancelled_queued_request_is_answered_request_cancelled() {
    let mut c = Client::new();
    // Queued before the server runs, so the cancel is read ahead of the
    // request it cancels.
    let init = c.send_request("initialize", json!({ "capabilities": {} }));
    c.notify("initialized", json!({}));
    let id = c.send_request("textDocument/hover", json!({}));
    c.notify("$/cancelRequest", json!({ "id": 2 }));
    let next = c.send_request("textDocument/hover", json!({}));
    c.spawn();
    assert!(c.response(&init).result.is_some());
    assert_eq!(c.response(&id).error.expect("an error").code, -32800);
    // Later requests still run.
    assert!(c.response(&next).error.is_none());
    c.shutdown().unwrap();
}

#[test]
fn unknown_requests_are_method_not_found() {
    let mut c = Client::start();
    c.initialize(json!({}));
    let r = c.request("textDocument/frobnicate", json!({}));
    assert_eq!(r.error.expect("an error").code, -32601);
    c.shutdown().unwrap();
}

#[test]
fn exit_without_shutdown_is_an_error() {
    let mut c = Client::start();
    c.initialize(json!({}));
    c.notify("exit", Value::Null);
    assert!(c.server.take().unwrap().join().unwrap().is_err());
}

#[test]
fn a_disconnected_client_ends_the_server_cleanly() {
    let mut c = Client::start();
    c.initialize(json!({}));
    let server = c.server.take().unwrap();
    drop(c);
    server.join().unwrap().unwrap();
}

#[test]
fn a_change_cancels_queued_requests_on_that_document() {
    let mut c = Client::new();
    // All queued before the server runs, so the change is read ahead of
    // the request it makes stale.
    let init = c.send_request("initialize", json!({ "capabilities": {} }));
    c.notify("initialized", json!({}));
    let (x, y) = ("file:///nowhere/x.md", "file:///nowhere/y.md");
    let pos = |uri: &str| {
        json!({ "textDocument": { "uri": uri },
                                  "position": { "line": 0, "character": 0 } })
    };
    let on_x = c.send_request("textDocument/hover", pos(x));
    let on_y = c.send_request("textDocument/hover", pos(y));
    c.change(x, 2, "new text");
    let after = c.send_request("textDocument/hover", pos(x));
    c.spawn();
    assert!(c.response(&init).result.is_some());
    assert_eq!(c.response(&on_x).error.expect("an error").code, -32800);
    assert!(c.response(&on_y).error.is_none());
    // A request sent after the change runs on the new text.
    assert!(c.response(&after).error.is_none());
    c.shutdown().unwrap();
}

#[test]
fn after_shutdown_only_exit_counts() {
    let v = Vault::corpus("zk-min");
    let mut c = Client::start();
    c.initialize(json!({}));
    let uri = v.uri("broken.md");
    let text = v.read("broken.md");
    c.open(&uri, &text);
    assert_eq!(c.diagnostics(&uri).len(), 1);
    // A pending debounce is dropped by shutdown.
    c.change(&uri, 2, &text.replace("(missing-note)", "(a.md)"));
    let r = c.request("shutdown", Value::Null);
    assert!(r.error.is_none(), "{r:?}");
    c.open(&v.uri("a.md"), &v.read("a.md"));
    c.notify(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": uri } }),
    );
    let id = c.send_request("textDocument/hover", json!({}));
    // The next message is the error reply: the notifications did nothing.
    match c.recv() {
        Message::Response(r) => {
            assert_eq!(r.id, id);
            assert_eq!(r.error.expect("an error").code, -32600);
        }
        m => panic!("unexpected {m:?}"),
    }
    // Past the debounce: nothing more arrives.
    std::thread::sleep(Duration::from_millis(800));
    if let Ok(m) = c.conn.receiver.try_recv() {
        panic!("unexpected {m:?}");
    }
    c.notify("exit", Value::Null);
    c.server.take().unwrap().join().unwrap().unwrap();
}
