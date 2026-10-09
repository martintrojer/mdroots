//! The server in-process over `Connection::memory()`, on copies of
//! tests/corpus in temp dirs.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use mdroots::{IndexMode, NoEnumerator, Options};
use serde_json::{Value, json};
use tempfile::TempDir;

type ServeResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// A running server and the client end of its connection.
struct Client {
    conn: Connection,
    server: Option<JoinHandle<ServeResult>>,
    server_conn: Option<Connection>,
    next_id: i32,
    /// Messages read ahead by [`Client::ready`], handed out first.
    backlog: RefCell<VecDeque<Message>>,
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
            backlog: RefCell::default(),
        }
    }

    /// Starts the server with an in-memory index: tests never touch the
    /// user's cache dir.
    fn spawn(&mut self) {
        self.spawn_with(Options::default().index(IndexMode::Memory));
    }

    fn spawn_with(&mut self, opts: Options) {
        let conn = self.server_conn.take().unwrap();
        let opts = opts.enumerator(std::sync::Arc::new(NoEnumerator));
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
        if let Some(m) = self.backlog.borrow_mut().pop_front() {
            return m;
        }
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

    /// Waits until the open document `uri` is served by its root's
    /// workspace (the background open finished), polling `mdroots.info`
    /// (no fixed sleep). Its publishes before the post-open one are
    /// dropped, so the next [`Client::diagnostics`] for `uri` returns the
    /// root's diagnostics, as are the open's code lens refreshes; other
    /// messages stay queued in order.
    fn ready(&mut self, uri: &str) {
        let mut seen = Vec::new();
        let start = Instant::now();
        loop {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "{uri}: root not open within 10 s"
            );
            let args = json!({ "command": "mdroots.info", "arguments": [uri] });
            let (r, more) = self.request_seeing("workspace/executeCommand", args);
            seen.extend(more);
            let info = r.result.unwrap_or(Value::Null);
            let info = info.as_str().unwrap_or_default();
            if !info.is_empty() && !info.contains(SINGLE_REASON) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let is_pub = |m: &Message| {
            matches!(m, Message::Notification(n)
                if n.method == "textDocument/publishDiagnostics" && n.params["uri"] == uri)
        };
        let last = seen.iter().rposition(is_pub);
        let mut backlog = self.backlog.borrow_mut();
        for (i, m) in seen.into_iter().enumerate() {
            let info_msg = matches!(&m, Message::Notification(n)
                if n.method == "window/showMessage"
                    && n.params["message"].as_str().is_some_and(|t| t.starts_with("root: ")));
            if info_msg || is_lens_refresh(&m) || (is_pub(&m) && Some(i) != last) {
                continue;
            }
            backlog.push_back(m);
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
        json!([
            "mdroots.anchorLinks",
            "mdroots.backlinks",
            "mdroots.info",
            "mdroots.renameFile"
        ])
    );
    assert_eq!(caps["foldingRangeProvider"], true);
    assert_eq!(caps["codeLensProvider"]["resolveProvider"], false);
    assert_eq!(
        caps["codeActionProvider"]["codeActionKinds"],
        json!(["refactor.extract.note"])
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
    c.ready(&uri);
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
    c.ready(&uri);
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
    c.ready(&uri);
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
    c.ready(&uri);
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

/// The reason of the single-file workspace serving a document until its
/// root is open.
const SINGLE_REASON: &str = "opened without discovery";

// ---- feature requests ----

const UTF8: &str = r#"{ "general": { "positionEncodings": ["utf-8"] } }"#;

/// The note the editor smoke test writes: headings and in-document links.
const LOOSE: &str = "# Loose note\n\nSee [the setup](#setup) and [[#Usage]].\n\n## Setup\n\nText.\n\n## Usage\n\nBack to [setup](#setup).\n";

impl Vault {
    fn write(&self, rel: &str, text: &str) {
        fs::write(self.dir.join(rel), text).unwrap();
    }
}

/// A started, initialized client on `vault` with `rel` open.
fn session(v: &Vault, caps: &str, open: &[&str]) -> Client {
    let mut c = Client::start();
    c.initialize(serde_json::from_str(caps).unwrap());
    for rel in open {
        c.open(&v.uri(rel), &v.read(rel));
    }
    for rel in open {
        c.ready(&v.uri(rel));
    }
    c
}

fn at(uri: &str, line: u32, character: u32) -> Value {
    json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } })
}

fn ok(r: Response) -> Value {
    assert!(r.error.is_none(), "{:?}", r.error);
    r.result.unwrap_or(Value::Null)
}

/// (file name, start line, start character) of each location.
fn spots(v: &Value) -> Vec<(String, u64, u64)> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|l| {
            let name = l["uri"].as_str().unwrap().rsplit('/').next().unwrap();
            let s = &l["range"]["start"];
            (
                name.to_owned(),
                s["line"].as_u64().unwrap(),
                s["character"].as_u64().unwrap(),
            )
        })
        .collect()
}

fn spot(name: &str, line: u64, character: u64) -> (String, u64, u64) {
    (name.to_owned(), line, character)
}

fn labels(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["label"].as_str().unwrap().to_owned())
        .collect()
}

fn range(l0: u64, c0: u64, l1: u64, c1: u64) -> Value {
    json!({ "start": { "line": l0, "character": c0 }, "end": { "line": l1, "character": c1 } })
}

#[test]
fn definition_follows_links_and_anchors() {
    let v = Vault::corpus("notesvault");
    let mut c = session(&v, UTF8, &["notes/project-scope.md"]);
    let uri = v.uri("notes/project-scope.md");
    // [[note-a]] -> line 0 of the note.
    let r = ok(c.request("textDocument/definition", at(&uri, 10, 25)));
    assert_eq!(spots(&r), [spot("note-a.md", 0, 0)]);
    // [[note-b#Details]] -> the heading line.
    let r = ok(c.request("textDocument/definition", at(&uri, 10, 39)));
    assert_eq!(spots(&r), [spot("note-b.md", 13, 0)]);
    assert_eq!(r[0]["range"], range(13, 0, 13, 10));
    // Not on a link: null.
    let r = ok(c.request("textDocument/definition", at(&uri, 10, 2)));
    assert_eq!(r, Value::Null);
    // Goto works in code: README's fenced [[project-scope]].
    let readme = v.uri("README.md");
    c.open(&readme, &v.read("README.md"));
    c.ready(&readme);
    let r = ok(c.request("textDocument/definition", at(&readme, 7, 16)));
    assert_eq!(spots(&r), [spot("project-scope.md", 0, 0)]);
    c.shutdown().unwrap();
}

#[test]
fn definition_of_in_document_anchors() {
    let v = Vault::corpus("zk-min");
    v.write("loose-note.md", LOOSE);
    let mut c = session(&v, UTF8, &["loose-note.md"]);
    let uri = v.uri("loose-note.md");
    let r = ok(c.request("textDocument/definition", at(&uri, 2, 8)));
    assert_eq!(spots(&r), [spot("loose-note.md", 4, 0)]);
    let r = ok(c.request("textDocument/definition", at(&uri, 2, 32)));
    assert_eq!(spots(&r), [spot("loose-note.md", 8, 0)]);
    c.shutdown().unwrap();
}

#[test]
fn positions_follow_the_negotiated_encoding() {
    // emoji.md line 2: "😀 日本 [Note B](b)": the link starts at byte 12,
    // UTF-16 unit 6.
    for (caps, col) in [(UTF8, 12), ("{}", 6)] {
        let v = Vault::corpus("zk-min");
        let mut c = session(&v, caps, &["emoji.md", "b.md"]);
        let r = ok(c.request(
            "textDocument/definition",
            at(&v.uri("emoji.md"), 2, col + 1),
        ));
        assert_eq!(spots(&r), [spot("b.md", 0, 0)], "{caps}");
        // One unit before the link: nothing.
        let r = ok(c.request(
            "textDocument/definition",
            at(&v.uri("emoji.md"), 2, col - 1),
        ));
        assert_eq!(r, Value::Null, "{caps}");
        let r = ok(c.request("textDocument/references", at(&v.uri("b.md"), 0, 0)));
        assert_eq!(
            spots(&r),
            [spot("a.md", 2, 4), spot("emoji.md", 2, col.into())],
            "{caps}"
        );
        c.shutdown().unwrap();
    }
}

#[test]
fn references_on_a_link_and_elsewhere() {
    let v = Vault::corpus("zk-min");
    let mut c = session(&v, UTF8, &["a.md"]);
    let uri = v.uri("a.md");
    // On [Note B](b): links to b.md.
    let r = ok(c.request("textDocument/references", at(&uri, 2, 6)));
    assert_eq!(spots(&r), [spot("a.md", 2, 4), spot("emoji.md", 2, 12)]);
    // Elsewhere: links to a.md.
    let r = ok(c.request("textDocument/references", at(&uri, 0, 0)));
    assert_eq!(spots(&r), [spot("b.md", 2, 8), spot("tagged.md", 5, 9)]);
    // On a broken link: none.
    c.open(&v.uri("broken.md"), &v.read("broken.md"));
    c.ready(&v.uri("broken.md"));
    let r = ok(c.request("textDocument/references", at(&v.uri("broken.md"), 2, 18)));
    assert_eq!(r, json!([]));
    c.shutdown().unwrap();
}

#[test]
fn hover_previews_the_target_or_says_broken() {
    let v = Vault::corpus("zk-min");
    let mut c = session(&v, UTF8, &["a.md", "broken.md"]);
    let r = ok(c.request("textDocument/hover", at(&v.uri("a.md"), 2, 6)));
    assert_eq!(r["contents"]["kind"], "markdown");
    assert_eq!(
        r["contents"]["value"],
        "**Note B**\n\n# Note B\n\nBack to [Note A](a)."
    );
    assert_eq!(r["range"], range(2, 4, 2, 15));
    let r = ok(c.request("textDocument/hover", at(&v.uri("broken.md"), 2, 18)));
    assert_eq!(r["contents"]["value"], "broken link");
    let r = ok(c.request("textDocument/hover", at(&v.uri("a.md"), 0, 0)));
    assert_eq!(r, Value::Null);
    c.shutdown().unwrap();
}

#[test]
fn document_symbols_nest_by_level() {
    let v = Vault::corpus("zk-min");
    v.write("loose-note.md", LOOSE);
    let mut c = session(&v, UTF8, &["loose-note.md"]);
    let r = ok(c.request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": v.uri("loose-note.md") } }),
    ));
    let top = r.as_array().unwrap();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0]["name"], "Loose note");
    assert_eq!(top[0]["kind"], 15); // SymbolKind::STRING
    assert_eq!(top[0]["range"], range(0, 0, 11, 0));
    assert_eq!(top[0]["selectionRange"], range(0, 0, 0, 12));
    let kids = top[0]["children"].as_array().unwrap();
    let names: Vec<&str> = kids.iter().map(|k| k["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Setup", "Usage"]);
    assert_eq!(kids[0]["range"], range(4, 0, 8, 0));
    assert_eq!(kids[1]["range"], range(8, 0, 11, 0));
    assert!(kids[0].get("children").is_none_or(Value::is_null));
    c.shutdown().unwrap();
}

#[test]
fn workspace_symbols_search_notes() {
    let v = Vault::corpus("zk-min");
    let mut c = session(&v, UTF8, &["a.md"]);
    let r = ok(c.request("workspace/symbol", json!({ "query": "note" })));
    let got: Vec<(&str, &str, i64)> = r
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap(),
                s["location"]["uri"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap(),
                s["kind"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("Note A", "a.md", 1),
            ("Note B", "b.md", 1),
            ("zk-min: a minimal zk notebook fixture", "README.md", 1),
        ]
    );
    c.shutdown().unwrap();
}

#[test]
fn completion_of_notes_headings_paths_and_tags() {
    let v = Vault::corpus("zk-min");
    v.write("loose-note.md", LOOSE);
    let mut c = session(&v, UTF8, &["loose-note.md"]);
    let uri = v.uri("loose-note.md");
    let complete = |c: &mut Client, text: &str| -> Value {
        c.change(&uri, 2, &format!("{LOOSE}{text}"));
        ok(c.request("textDocument/completion", at(&uri, 11, text.len() as u32)))
    };
    // [[: notes by stem, best match first (title prefix, then substring;
    // ties by path), title as detail.
    let r = complete(&mut c, "see [[no");
    assert_eq!(r["isIncomplete"], false);
    assert_eq!(labels(&r), ["a", "b", "README", "loose-note"]);
    assert_eq!(r["items"][0]["detail"], "Note A");
    assert_eq!(r["items"][0]["textEdit"]["newText"], "a");
    assert_eq!(r["items"][0]["textEdit"]["range"], range(11, 6, 11, 8));
    // [[#: this note's headings without its title.
    assert_eq!(labels(&complete(&mut c, "[[#")), ["Setup", "Usage"]);
    // [[note#: that note's headings; b.md has only its title.
    assert!(labels(&complete(&mut c, "[[b#")).is_empty());
    // A tag after a blank, not at line start.
    assert_eq!(labels(&complete(&mut c, "text #pr")), ["project"]);
    let r = complete(&mut c, "#");
    assert!(labels(&r).is_empty(), "{r}");
    assert!(labels(&complete(&mut c, "  #pro")).is_empty());
    // ](: paths relative to this note, extension kept.
    assert_eq!(
        labels(&complete(&mut c, "[x](")),
        [
            "README.md",
            "a.md",
            "b.md",
            "broken.md",
            "emoji.md",
            "tagged.md"
        ]
    );
    c.shutdown().unwrap();
}

#[test]
fn completion_of_paths_from_a_subdirectory_and_headings_of_another_note() {
    let v = Vault::corpus("notesvault");
    let mut c = session(&v, UTF8, &["notes/note-a.md"]);
    let uri = v.uri("notes/note-a.md");
    let base = v.read("notes/note-a.md");
    let line = base.lines().count() as u32;
    let mut complete = |text: &str| -> Value {
        c.change(&uri, 2, &format!("{base}{text}"));
        ok(c.request("textDocument/completion", at(&uri, line, text.len() as u32)))
    };
    assert_eq!(
        labels(&complete("[p](")),
        [
            "../AGENTS.md",
            "../README.md",
            "../concepts/b.md",
            "../concepts/c.md",
            "note-b.md",
            "project-scope.md",
            "../references/paper-1.md",
            "../references/paper-2.md",
        ]
    );
    assert_eq!(
        labels(&complete("[p](../con")),
        ["../concepts/b.md", "../concepts/c.md"]
    );
    assert_eq!(labels(&complete("[[note-b#")), ["Details"]);
    c.shutdown().unwrap();
}

/// Every file of the vault and its text.
fn snapshot(v: &Vault) -> Vec<(PathBuf, String)> {
    let mut out: Vec<_> = fs::read_dir(&v.dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .map(|p| {
            let t = fs::read_to_string(&p).unwrap();
            (p, t)
        })
        .collect();
    out.sort();
    out
}

/// (file name, [(start line, start char, new text)]) of each text edit,
/// then the rename op's (old, new) file names.
type Edits = (
    Vec<(String, Vec<(u64, u64, String)>)>,
    Option<(String, String)>,
);

fn edits(we: &Value) -> Edits {
    let name = |u: &Value| u.as_str().unwrap().rsplit('/').next().unwrap().to_owned();
    let mut files = Vec::new();
    let mut rename = None;
    for op in we["documentChanges"].as_array().unwrap() {
        if op["kind"] == "rename" {
            rename = Some((name(&op["oldUri"]), name(&op["newUri"])));
            continue;
        }
        assert_eq!(op["textDocument"]["version"], Value::Null);
        let es = op["edits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let s = &e["range"]["start"];
                (
                    s["line"].as_u64().unwrap(),
                    s["character"].as_u64().unwrap(),
                    e["newText"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        files.push((name(&op["textDocument"]["uri"]), es));
    }
    (files, rename)
}

#[test]
fn rename_returns_edits_and_a_file_rename_without_writing() {
    let v = Vault::corpus("zk-min");
    let before = snapshot(&v);
    let mut c = session(&v, UTF8, &["a.md"]);
    let uri = v.uri("a.md");
    // On a link: the link range, the target's stem as placeholder.
    let r = ok(c.request("textDocument/prepareRename", at(&uri, 2, 6)));
    assert_eq!(
        r,
        json!({ "range": range(2, 4, 2, 15), "placeholder": "b" })
    );
    // On the H1: this note.
    let r = ok(c.request("textDocument/prepareRename", at(&uri, 0, 3)));
    assert_eq!(r, json!({ "range": range(0, 0, 0, 8), "placeholder": "a" }));
    // In prose: nothing to rename.
    let r = ok(c.request("textDocument/prepareRename", at(&uri, 2, 1)));
    assert_eq!(r, Value::Null);

    let mut p = at(&uri, 2, 6);
    p["newName"] = json!("bee");
    let r = ok(c.request("textDocument/rename", p));
    assert_eq!(
        edits(&r),
        (
            vec![
                ("a.md".to_owned(), vec![(2, 13, "bee".to_owned())]),
                ("emoji.md".to_owned(), vec![(2, 21, "bee".to_owned())]),
            ],
            Some(("b.md".to_owned(), "bee.md".to_owned())),
        )
    );
    // On the H1: the file only; heading text untouched.
    let mut p = at(&uri, 0, 3);
    p["newName"] = json!("alpha");
    let (files, rename) = edits(&ok(c.request("textDocument/rename", p)));
    assert_eq!(rename, Some(("a.md".to_owned(), "alpha.md".to_owned())));
    assert!(
        files
            .iter()
            .all(|(_, es)| es.iter().all(|e| e.2 == "alpha"))
    );
    assert_eq!(files.len(), 2, "{files:?}");

    for bad in ["sub/bee", "bee.md"] {
        let mut p = at(&uri, 2, 6);
        p["newName"] = json!(bad);
        let r = c.request("textDocument/rename", p);
        assert_eq!(r.error.expect("an error").code, -32602, "{bad}");
    }
    assert_eq!(snapshot(&v), before, "rename wrote files");
    c.shutdown().unwrap();
}

impl Client {
    /// Sends a request; returns its response and the messages that came
    /// before it.
    fn request_seeing(&mut self, method: &str, params: Value) -> (Response, Vec<Message>) {
        let id = self.send_request(method, params);
        let mut seen = Vec::new();
        loop {
            match self.recv() {
                Message::Response(r) if r.id == id => return (r, seen),
                m => seen.push(m),
            }
        }
    }
}

fn command(name: &str, args: Value) -> Value {
    json!({ "command": name, "arguments": args })
}

#[test]
fn backlinks_command_skips_the_note_itself() {
    let v = Vault::corpus("zk-min");
    let mut loose = LOOSE.to_owned();
    loose.push_str("\nSee [b](b) and [b again](b).\n");
    v.write("loose-note.md", &loose);
    v.write("c.md", "Twice: [x](b) [y](b).\n");
    let mut c = session(&v, UTF8, &["b.md", "loose-note.md"]);
    let pos = json!({ "line": 0, "character": 0 });
    let r = ok(c.request(
        "workspace/executeCommand",
        command("mdroots.backlinks", json!([v.uri("b.md"), pos])),
    ));
    // a.md, c.md (one per line), emoji.md, loose-note.md (one per line).
    assert_eq!(
        spots(&r),
        [
            spot("a.md", 2, 4),
            spot("c.md", 0, 7),
            spot("emoji.md", 2, 12),
            spot("loose-note.md", 12, 4),
        ]
    );
    // The loose note links only to itself: no backlinks.
    let r = ok(c.request(
        "workspace/executeCommand",
        command("mdroots.backlinks", json!([v.uri("loose-note.md"), pos])),
    ));
    assert_eq!(r, json!([]));
    c.shutdown().unwrap();
}

#[test]
fn info_command_shows_and_returns_the_root() {
    let v = Vault::corpus("zk-min");
    let mut c = session(&v, UTF8, &["a.md"]);
    let (r, seen) = c.request_seeing(
        "workspace/executeCommand",
        command("mdroots.info", json!([v.uri("a.md")])),
    );
    let text = ok(r);
    let text = text.as_str().unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    assert_eq!(lines[0], format!("root: {}", v.dir.display()));
    assert_eq!(lines[1], "mode: marker");
    assert!(
        lines[2].starts_with("why: ") && lines[2].len() > 5,
        "{text}"
    );
    assert_eq!(lines[3], "files: 6");
    let shown = seen.iter().any(|m| {
        matches!(m, Message::Notification(n)
            if n.method == "window/showMessage" && n.params["type"] == 3 && n.params["message"] == text)
    });
    assert!(shown, "{seen:?}");
    c.shutdown().unwrap();
}

#[test]
fn rename_file_command_sends_apply_edit() {
    let v = Vault::corpus("zk-min");
    let before = snapshot(&v);
    let mut c = session(&v, UTF8, &["b.md"]);
    let (r, seen) = c.request_seeing(
        "workspace/executeCommand",
        command(
            "mdroots.renameFile",
            json!([v.uri("b.md"), v.uri("bee.md")]),
        ),
    );
    assert_eq!(ok(r), Value::Null);
    let apply = seen
        .iter()
        .find_map(|m| match m {
            Message::Request(r) if r.method == "workspace/applyEdit" => Some(r.clone()),
            _ => None,
        })
        .expect("an applyEdit request");
    let (files, rename) = edits(&apply.params["edit"]);
    assert_eq!(rename, Some(("b.md".to_owned(), "bee.md".to_owned())));
    let names: Vec<&str> = files.iter().map(|f| f.0.as_str()).collect();
    assert_eq!(names, ["a.md", "emoji.md"]);
    // The client's answer is accepted and ignored.
    let resp = Response::new_ok(apply.id, json!({ "applied": false }));
    c.conn.sender.send(resp.into()).unwrap();
    assert_eq!(snapshot(&v), before);
    c.shutdown().unwrap();
}

// ---- refresh from disk ----

/// A client on a temp cache dir (not the user's), initialized with
/// `capabilities`, and the initialize result.
fn cached_session(cache: &Path, capabilities: Value) -> Client {
    let mut c = Client::new();
    c.spawn_with(Options::default().cache_dir(cache.to_path_buf()));
    c.initialize(capabilities);
    c
}

#[test]
fn did_save_refreshes_from_disk_and_republishes() {
    let v = Vault::corpus("zk-min");
    let cache = tempfile::tempdir().unwrap();
    let mut c = cached_session(cache.path(), json!({}));
    let uri = v.uri("a.md");
    let text = "# Note A\n\nSee [the part](b.md#part).\n";
    v.write("a.md", text);
    c.open(&uri, text);
    c.ready(&uri);
    let d = c.diagnostics(&uri);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0]["code"], "broken-anchor");
    // Another program adds the heading to b.md; the save of a.md picks it up.
    v.write("b.md", "# Note B\n\n## Part\n");
    c.notify(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert_eq!(c.diagnostics(&uri), Vec::<Value>::new());
    // The DB lives in the cache dir, never in the vault.
    assert!(cache.path().join("roots.v1.db").exists());
    assert!(!v.dir.join("roots.v1.db").exists());
    let _ = c.request("shutdown", Value::Null);
    c.notify("exit", Value::Null);
    c.server.take().unwrap().join().unwrap().unwrap();
}

#[test]
fn did_change_watched_files_refreshes_affected_workspaces() {
    let v = Vault::corpus("zk-min");
    let mut c = Client::start();
    c.initialize(json!({}));
    let uri = v.uri("a.md");
    let text = "See [the part](b.md#part).\n";
    v.write("a.md", text);
    c.open(&uri, text);
    c.ready(&uri);
    assert_eq!(c.diagnostics(&uri).len(), 1);
    v.write("b.md", "## Part\n");
    c.notify(
        "workspace/didChangeWatchedFiles",
        json!({ "changes": [{ "uri": v.uri("b.md"), "type": 2 }] }),
    );
    assert_eq!(c.diagnostics(&uri), Vec::<Value>::new());
    c.shutdown().unwrap();
}

#[test]
fn a_watching_server_republishes_when_a_missing_target_appears_on_disk() {
    let v = Vault::corpus("zk-min");
    let cache = tempfile::tempdir().unwrap();
    let mut c = Client::new();
    c.spawn_with(
        Options::default()
            .cache_dir(cache.path().to_path_buf())
            .watch(true),
    );
    c.initialize(json!({}));
    let uri = v.uri("broken.md");
    c.open(&uri, &v.read("broken.md"));
    c.ready(&uri);
    let d = c.diagnostics(&uri);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0]["code"], "broken-link");
    // Another program creates the target; no save, no client watcher.
    v.write("missing-note.md", "# Found\n");
    let start = Instant::now();
    loop {
        let left = Duration::from_secs(5).saturating_sub(start.elapsed());
        let m = c
            .conn
            .receiver
            .recv_timeout(left)
            .expect("diagnostics without the broken link within 5 s");
        if let Message::Notification(n) = m
            && n.method == "textDocument/publishDiagnostics"
            && n.params["uri"] == uri.as_str()
            && n.params["diagnostics"] == json!([])
        {
            break;
        }
    }
    c.shutdown().unwrap();
}

#[test]
fn registers_a_watcher_when_the_client_offers_dynamic_registration() {
    let mut c = Client::start();
    let caps = json!({ "workspace": { "didChangeWatchedFiles": { "dynamicRegistration": true } } });
    c.initialize(caps);
    let req = loop {
        if let Message::Request(r) = c.recv() {
            break r;
        }
    };
    assert_eq!(req.method, "client/registerCapability");
    let reg = &req.params["registrations"][0];
    assert_eq!(reg["method"], "workspace/didChangeWatchedFiles");
    assert_eq!(
        reg["registerOptions"]["watchers"][0]["globPattern"],
        "**/*.{md,markdown,org}"
    );
    c.shutdown().unwrap();
}

#[test]
fn no_watcher_without_dynamic_registration() {
    let mut c = Client::start();
    c.initialize(json!({}));
    let id = c.send_request("shutdown", Value::Null);
    // The first message after initialize is the shutdown reply.
    match c.recv() {
        Message::Response(r) => assert_eq!(r.id, id),
        m => panic!("unexpected {m:?}"),
    }
    c.notify("exit", Value::Null);
    c.server.take().unwrap().join().unwrap().unwrap();
}

// ---- folding ranges and code lenses ----

/// (start line, end line, kind) of each folding range.
fn folds(v: &Value) -> Vec<(u64, u64, Option<String>)> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|f| {
            let kind = f.get("kind").and_then(Value::as_str).map(str::to_owned);
            (
                f["startLine"].as_u64().unwrap(),
                f["endLine"].as_u64().unwrap(),
                kind,
            )
        })
        .collect()
}

#[test]
fn folding_ranges_for_sections_and_frontmatter() {
    let v = Vault::corpus("zk-min");
    let text = "---\ntitle: F\ntags: [x]\n---\n# F\n\nIntro.\n\n## A\n\nText.\n### A1\nDeep.\n## B\n# G\n\nEnd.\n";
    v.write("f.md", text);
    let mut c = session(&v, UTF8, &["f.md"]);
    let uri = v.uri("f.md");
    let r = ok(c.request(
        "textDocument/foldingRange",
        json!({ "textDocument": { "uri": uri } }),
    ));
    // Lines: 0-3 frontmatter, 4 # F, 8 ## A, 11 ### A1, 13 ## B (one
    // line: no range), 14 # G to the end (line 16).
    let region = Some("region".to_owned());
    assert_eq!(
        folds(&r),
        [
            (4, 13, None),
            (8, 12, None),
            (11, 12, None),
            (14, 16, None),
            (0, 3, region),
        ]
    );
    c.shutdown().unwrap();
}

fn lenses(c: &mut Client, uri: &str) -> Vec<(u64, String, String, Value)> {
    let r = ok(c.request(
        "textDocument/codeLens",
        json!({ "textDocument": { "uri": uri } }),
    ));
    r.as_array()
        .unwrap()
        .iter()
        .map(|l| {
            let cmd = &l["command"];
            (
                l["range"]["start"]["line"].as_u64().unwrap(),
                cmd["title"].as_str().unwrap().to_owned(),
                cmd["command"].as_str().unwrap().to_owned(),
                cmd["arguments"].clone(),
            )
        })
        .collect()
}

#[test]
fn code_lenses_count_backlinks_and_anchor_links() {
    let v = Vault::corpus("zk-min");
    v.write(
        "b.md",
        "# Note B\n\nBack to [Note A](a).\n\n## Part\n\n## Quiet\n\nSee [[#Quiet]].\n",
    );
    v.write("c.md", "See [[b#Part]].\n");
    let mut c = session(&v, UTF8, &["b.md"]);
    let uri = v.uri("b.md");
    // a.md, emoji.md and c.md link to b.md; c.md names "Part"; "Quiet" is
    // only named by b.md itself.
    let got = lenses(&mut c, &uri);
    assert_eq!(
        got,
        [
            (
                0,
                "3 backlinks".to_owned(),
                "mdroots.backlinks".to_owned(),
                json!([uri, { "line": 0, "character": 0 }]),
            ),
            (
                4,
                "1 link".to_owned(),
                "mdroots.anchorLinks".to_owned(),
                json!([uri, "part"]),
            ),
        ]
    );
    let r = ok(c.request(
        "workspace/executeCommand",
        command("mdroots.anchorLinks", json!([uri, "part"])),
    ));
    assert_eq!(spots(&r), [spot("c.md", 0, 4)]);
    let r = ok(c.request(
        "workspace/executeCommand",
        command("mdroots.anchorLinks", json!([uri, "quiet"])),
    ));
    assert_eq!(r, json!([]));
    c.shutdown().unwrap();
}

#[test]
fn code_lenses_two_backlinks_and_none() {
    let v = Vault::corpus("zk-min");
    let mut c = session(&v, UTF8, &["b.md", "broken.md"]);
    let uri = v.uri("b.md");
    let got = lenses(&mut c, &uri);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].1, "2 backlinks");
    // No note links to broken.md: no lens at all.
    assert!(lenses(&mut c, &v.uri("broken.md")).is_empty());
    // A note without a level-1 heading gets the lens on line 0.
    v.write("d.md", "Intro.\n\n## Sub\n");
    v.write("e.md", "[[d]]\n");
    let mut c2 = session(&v, UTF8, &["d.md", "e.md"]);
    let got = lenses(&mut c2, &v.uri("d.md"));
    assert_eq!((got[0].0, got[0].1.as_str()), (0, "1 backlink"));
    c2.shutdown().unwrap();
    c.shutdown().unwrap();
}

/// Messages up to the shutdown reply, then exit.
fn shutdown_seeing(mut c: Client) -> Vec<Message> {
    let (r, seen) = c.request_seeing("shutdown", Value::Null);
    assert!(r.error.is_none(), "{r:?}");
    c.notify("exit", Value::Null);
    c.server.take().unwrap().join().unwrap().unwrap();
    seen
}

fn is_lens_refresh(m: &Message) -> bool {
    matches!(m, Message::Request(r) if r.method == "workspace/codeLens/refresh")
}

#[test]
fn a_refresh_asks_for_code_lenses_only_when_the_client_supports_it() {
    for support in [true, false] {
        let v = Vault::corpus("zk-min");
        let mut c = Client::start();
        c.initialize(json!({ "workspace": { "codeLens": { "refreshSupport": support } } }));
        let uri = v.uri("a.md");
        c.open(&uri, &v.read("a.md"));
        c.ready(&uri);
        assert_eq!(c.diagnostics(&uri), Vec::<Value>::new());
        v.write("c.md", "[[a]]\n");
        c.notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": [{ "uri": v.uri("c.md"), "type": 1 }] }),
        );
        assert_eq!(c.diagnostics(&uri), Vec::<Value>::new());
        if support {
            let req = loop {
                if let Message::Request(r) = c.recv() {
                    break r;
                }
            };
            assert_eq!(req.method, "workspace/codeLens/refresh");
            // The client's answer is accepted and ignored.
            let resp = Response::new_ok(req.id, Value::Null);
            c.conn.sender.send(resp.into()).unwrap();
            c.shutdown().unwrap();
        } else {
            let seen = shutdown_seeing(c);
            assert!(!seen.iter().any(is_lens_refresh), "{seen:?}");
        }
    }
}

#[test]
fn a_watching_server_asks_for_code_lenses_after_a_change_on_disk() {
    let v = Vault::corpus("zk-min");
    let cache = tempfile::tempdir().unwrap();
    let mut c = Client::new();
    c.spawn_with(
        Options::default()
            .cache_dir(cache.path().to_path_buf())
            .watch(true),
    );
    c.initialize(json!({ "workspace": { "codeLens": { "refreshSupport": true } } }));
    let uri = v.uri("a.md");
    c.open(&uri, &v.read("a.md"));
    c.ready(&uri);
    assert_eq!(c.diagnostics(&uri), Vec::<Value>::new());
    v.write("c.md", "[[a]]\n");
    let start = Instant::now();
    loop {
        let left = Duration::from_secs(5).saturating_sub(start.elapsed());
        let m = c
            .conn
            .receiver
            .recv_timeout(left)
            .expect("a codeLens refresh within 5 s");
        if is_lens_refresh(&m) {
            break;
        }
    }
    c.shutdown().unwrap();
}

// ---- extract-note code action ----

fn code_action(c: &mut Client, uri: &str, range: Value, only: Option<&str>) -> Value {
    let mut context = json!({ "diagnostics": [] });
    if let Some(k) = only {
        context["only"] = json!([k]);
    }
    ok(c.request(
        "textDocument/codeAction",
        json!({ "textDocument": { "uri": uri }, "range": range, "context": context }),
    ))
}

#[test]
fn extract_note_creates_fills_then_links_without_writing() {
    let v = Vault::corpus("zk-min");
    v.write("x.md", "# X\n\n## Big idea\n\nDetails here.\n\nEnd.\n");
    let before = snapshot(&v);
    let mut c = session(&v, UTF8, &["x.md"]);
    let uri = v.uri("x.md");
    let new_uri = v.uri("big-idea.md");
    // Lines 2-4, the end at the last character (as a linewise selection
    // arrives from [Neovim](https://neovim.io)).
    let r = code_action(&mut c, &uri, range(2, 0, 4, 13), Some("refactor"));
    assert_eq!(
        r,
        json!([{
            "title": "Extract to new note: big-idea.md",
            "kind": "refactor.extract.note",
            "edit": { "documentChanges": [
                { "kind": "create", "uri": new_uri,
                  "options": { "overwrite": false, "ignoreIfExists": false } },
                { "textDocument": { "uri": new_uri, "version": null },
                  "edits": [{ "range": range(0, 0, 0, 0),
                              "newText": "## Big idea\n\nDetails here.\n" }] },
                { "textDocument": { "uri": uri, "version": null },
                  "edits": [{ "range": range(2, 0, 4, 13),
                              "newText": "[big-idea](big-idea)" }] },
            ] },
        }])
    );
    // The name exists on disk now: the next offer avoids it.
    v.write("big-idea.md", "");
    let r = code_action(&mut c, &uri, range(2, 0, 4, 13), None);
    assert_eq!(r[0]["title"], "Extract to new note: big-idea-2.md");
    let mut after = snapshot(&v);
    after.retain(|(p, _)| !p.ends_with("big-idea.md"));
    assert_eq!(after, before, "the server wrote files");
    c.shutdown().unwrap();
}

#[test]
fn no_extract_for_empty_selections_other_kinds_or_unknown_files() {
    let v = Vault::corpus("zk-min");
    let before = snapshot(&v);
    let mut c = session(&v, UTF8, &["a.md"]);
    let uri = v.uri("a.md");
    assert_eq!(
        code_action(&mut c, &uri, range(2, 3, 2, 3), None),
        json!([])
    );
    assert_eq!(
        code_action(&mut c, &uri, range(0, 0, 2, 3), Some("quickfix")),
        json!([])
    );
    let none = format!("file://{}/nowhere/x.md", v.dir.display());
    assert_eq!(
        code_action(&mut c, &none, range(0, 0, 0, 1), None),
        json!([])
    );
    assert_eq!(snapshot(&v), before);
    c.shutdown().unwrap();
}

// ---- background open ----

/// A [`StdProbe`](mdroots::StdProbe) whose calls block until the test
/// opens the gate: discovery, and so the background open, waits for it.
/// The single-file path never probes, so it is not held up.
struct Gate {
    open: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

impl Gate {
    fn new() -> std::sync::Arc<Gate> {
        std::sync::Arc::new(Gate {
            open: std::sync::Mutex::new(false),
            cv: std::sync::Condvar::new(),
        })
    }

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }

    fn wait(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.cv.wait(open).unwrap();
        }
    }
}

struct GatedProbe(std::sync::Arc<Gate>);

impl mdroots::Probe for GatedProbe {
    fn stat(&self, p: &Path) -> std::io::Result<mdroots::FsStat> {
        self.0.wait();
        mdroots::StdProbe.stat(p)
    }
    fn lstat(&self, p: &Path) -> std::io::Result<mdroots::FsStat> {
        self.0.wait();
        mdroots::StdProbe.lstat(p)
    }
    fn mount(&self, p: &Path) -> std::io::Result<mdroots::MountInfo> {
        self.0.wait();
        mdroots::StdProbe.mount(p)
    }
    fn read_dir(&self, p: &Path) -> std::io::Result<Vec<(String, mdroots::FsStat)>> {
        self.0.wait();
        mdroots::StdProbe.read_dir(p)
    }
    fn read_link(&self, p: &Path) -> std::io::Result<PathBuf> {
        self.0.wait();
        mdroots::StdProbe.read_link(p)
    }
    fn read_small(&self, p: &Path, cap: usize) -> std::io::Result<Vec<u8>> {
        self.0.wait();
        mdroots::StdProbe.read_small(p, cap)
    }
    fn read_prefix(&self, p: &Path, n: usize) -> std::io::Result<Vec<u8>> {
        self.0.wait();
        mdroots::StdProbe.read_prefix(p, n)
    }
    fn volume_id(&self, p: &Path) -> std::io::Result<String> {
        self.0.wait();
        mdroots::StdProbe.volume_id(p)
    }
    fn home(&self) -> Option<PathBuf> {
        mdroots::StdProbe.home()
    }
    fn now(&self) -> Duration {
        mdroots::StdProbe.now()
    }
}

/// A client whose background opens wait for `gate`, on a temp cache dir.
fn gated_session(gate: &std::sync::Arc<Gate>, cache: &Path, capabilities: Value) -> Client {
    let mut c = Client::new();
    c.spawn_with(
        Options::default()
            .fs(std::sync::Arc::new(mdroots::StdFs))
            .probe(std::sync::Arc::new(GatedProbe(gate.clone())))
            .cache_dir(cache.to_path_buf()),
    );
    c.initialize(capabilities);
    c
}

fn codes(d: &[Value]) -> Vec<&str> {
    d.iter().map(|d| d["code"].as_str().unwrap()).collect()
}

/// A note with a wiki link to a missing note, an in-document anchor and a
/// wiki link to b.md.
const PENDING: &str = "# P\n\nSee [[gone]] and [x](#p) and [[b]].\n";

#[test]
fn a_slow_open_publishes_single_file_diagnostics_then_the_roots() {
    let v = Vault::corpus("zk-min");
    v.write("p.md", PENDING);
    let (gate, cache) = (Gate::new(), tempfile::tempdir().unwrap());
    let c = gated_session(&gate, cache.path(), json!({}));
    let uri = v.uri("p.md");
    c.open(&uri, PENDING);
    // The open is held at the gate: this publish is the single-file one,
    // where no other note is indexed (lazy rules: a hint, not broken).
    assert_eq!(codes(&c.diagnostics(&uri)), ["not-in-working-set"]);
    gate.release();
    // The root is open: every note is indexed, so the link is broken.
    assert_eq!(codes(&c.diagnostics(&uri)), ["broken-link"]);
    c.shutdown().unwrap();
}

#[test]
fn requests_during_a_slow_open_answer_from_the_single_file() {
    let v = Vault::corpus("zk-min");
    v.write("p.md", PENDING);
    let (gate, cache) = (Gate::new(), tempfile::tempdir().unwrap());
    let mut c = gated_session(&gate, cache.path(), serde_json::from_str(UTF8).unwrap());
    let uri = v.uri("p.md");
    c.open(&uri, PENDING);
    assert_eq!(c.diagnostics(&uri).len(), 1);
    // In-document anchors resolve; a wiki link finds its target on disk.
    let r = ok(c.request("textDocument/definition", at(&uri, 2, 22)));
    assert_eq!(spots(&r), [spot("p.md", 0, 0)]);
    let r = ok(c.request("textDocument/definition", at(&uri, 2, 33)));
    assert_eq!(spots(&r), [spot("b.md", 0, 0)]);
    let r = ok(c.request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": uri } }),
    ));
    assert_eq!(r[0]["name"], "P");
    let r = ok(c.request(
        "workspace/executeCommand",
        command(
            "mdroots.backlinks",
            json!([uri, { "line": 0, "character": 0 }]),
        ),
    ));
    assert_eq!(r, json!([]));
    // Only open workspaces are searched: none yet.
    let r = ok(c.request("workspace/symbol", json!({ "query": "note" })));
    assert_eq!(r, json!([]));
    // A note that is not open is served alone too, never opened here:
    // its link target is not indexed (hover shows only the path) and no
    // note links to it.
    let a = v.uri("a.md");
    let r = ok(c.request("textDocument/hover", at(&a, 2, 6)));
    let preview = r["contents"]["value"].as_str().unwrap();
    assert!(
        preview.starts_with('`') && preview.ends_with("b.md`"),
        "{preview}"
    );
    let top = json!({ "line": 0, "character": 0 });
    let backlinks = json!([a, top]);
    let r = ok(c.request(
        "workspace/executeCommand",
        command("mdroots.backlinks", backlinks.clone()),
    ));
    assert_eq!(r, json!([]));
    gate.release();
    c.ready(&uri);
    // The root is open: the whole root answers.
    let r = ok(c.request(
        "workspace/executeCommand",
        command("mdroots.backlinks", backlinks),
    ));
    assert_eq!(spots(&r), [spot("b.md", 2, 8), spot("tagged.md", 5, 9)]);
    let r = ok(c.request("workspace/symbol", json!({ "query": "note b" })));
    assert_eq!(r[0]["name"], "Note B");
    c.shutdown().unwrap();
}

/// Messages until one matches `want` (10 s), and that message.
fn until(c: &Client, want: impl Fn(&Message) -> bool) -> (Message, Vec<Message>) {
    let mut seen = Vec::new();
    loop {
        let m = c.recv();
        if want(&m) {
            return (m, seen);
        }
        seen.push(m);
    }
}

fn is_progress(m: &Message, kind: &str) -> bool {
    matches!(m, Message::Notification(n)
        if n.method == "$/progress" && n.params["value"]["kind"] == kind)
}

#[test]
fn a_slow_open_reports_progress_when_the_client_supports_it() {
    let v = Vault::corpus("zk-min");
    let (gate, cache) = (Gate::new(), tempfile::tempdir().unwrap());
    let c = gated_session(
        &gate,
        cache.path(),
        json!({ "window": { "workDoneProgress": true } }),
    );
    let uri = v.uri("a.md");
    c.open(&uri, &v.read("a.md"));
    // Held at the gate past a second: the token is created, then begun.
    let (create, _) = until(&c, |m| matches!(m, Message::Request(_)));
    let Message::Request(create) = create else {
        unreachable!()
    };
    assert_eq!(create.method, "window/workDoneProgress/create");
    let token = create.params["token"].clone();
    assert!(token.as_str().unwrap().starts_with("mdroots/index/"));
    c.conn
        .sender
        .send(Response::new_ok(create.id, Value::Null).into())
        .unwrap();
    let (begin, _) = until(&c, |m| is_progress(m, "begin"));
    let Message::Notification(begin) = begin else {
        unreachable!()
    };
    assert_eq!(begin.params["token"], token);
    let title = begin.params["value"]["title"].as_str().unwrap();
    assert_eq!(title, format!("mdroots: indexing {}", v.dir.display()));
    gate.release();
    let (end, _) = until(&c, |m| is_progress(m, "end"));
    let Message::Notification(end) = end else {
        unreachable!()
    };
    assert_eq!(end.params["token"], token);
    c.shutdown().unwrap();
}

#[test]
fn a_slow_open_shows_a_message_without_progress_support() {
    let v = Vault::corpus("zk-min");
    let (gate, cache) = (Gate::new(), tempfile::tempdir().unwrap());
    let c = gated_session(&gate, cache.path(), json!({}));
    let uri = v.uri("a.md");
    c.open(&uri, &v.read("a.md"));
    let (shown, seen) = until(
        &c,
        |m| matches!(m, Message::Notification(n) if n.method == "window/showMessage"),
    );
    let Message::Notification(shown) = shown else {
        unreachable!()
    };
    assert_eq!(shown.params["type"], 3);
    assert_eq!(
        shown.params["message"],
        format!("mdroots: indexing {}\u{2026}", v.dir.display())
    );
    assert!(!seen.iter().any(|m| is_progress(m, "begin")), "{seen:?}");
    gate.release();
    let seen = shutdown_seeing(c);
    assert!(!seen.iter().any(|m| is_progress(m, "end")), "{seen:?}");
}

#[test]
fn a_document_closed_during_the_open_gets_nothing_after_it() {
    let v = Vault::corpus("zk-min");
    let (gate, cache) = (Gate::new(), tempfile::tempdir().unwrap());
    let mut c = gated_session(&gate, cache.path(), json!({}));
    let uri = v.uri("broken.md");
    c.open(&uri, &v.read("broken.md"));
    assert_eq!(c.diagnostics(&uri).len(), 1);
    c.notify(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert_eq!(c.diagnostics(&uri), Vec::<Value>::new());
    gate.release();
    // The root opens and stays cached: reopening serves it at once, with
    // one publish (no second, post-open one).
    let other = v.uri("a.md");
    loop {
        let r = ok(c.request(
            "workspace/executeCommand",
            command("mdroots.info", json!([other])),
        ));
        if r.as_str().is_some_and(|t| t.contains("mode: marker")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    c.open(&uri, &v.read("broken.md"));
    assert_eq!(codes(&c.diagnostics(&uri)), ["broken-link"]);
    let seen = shutdown_seeing(c);
    let pubs = seen.iter().filter(
        |m| matches!(m, Message::Notification(n) if n.method == "textDocument/publishDiagnostics"),
    );
    assert_eq!(pubs.count(), 0, "{seen:?}");
}
