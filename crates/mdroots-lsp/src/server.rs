//! The message loop: lifecycle, document sync, diagnostics, cancellation.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvError, Sender, TryRecvError, select};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::{
    CancelParams, CodeActionOptions, CodeActionProviderCapability, CodeLensOptions,
    CompletionOptions, DidChangeConfigurationParams, DidChangeTextDocumentParams,
    DidChangeWatchedFilesParams, DidChangeWatchedFilesRegistrationOptions,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
    ExecuteCommandOptions, ExecuteCommandParams, FileSystemWatcher, FoldingRangeProviderCapability,
    GlobPattern, MessageType, NumberOrString, OneOf, Position, PositionEncodingKind,
    PublishDiagnosticsParams, Registration, RegistrationParams, RenameOptions, SaveOptions,
    ServerCapabilities, ShowMessageParams, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Uri, WorkDoneProgressOptions,
};
use mdroots::syntax::{LineIndex, PositionEncoding};
use mdroots::{Cancel, ErrorKind, Options, Workspace, Workspaces, names};
use serde_json::{Value, json};

use crate::diagnostics::{self, Setting};
use crate::features::{self, Ctx, Fail};
use crate::{position, uri};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Quiet time after the last `didChange` of a document before its
/// diagnostics are published (library.md §4, phase 2).
const DEBOUNCE: Duration = Duration::from_millis(500);

/// The commands `workspace/executeCommand` accepts.
const COMMANDS: [&str; 4] = [
    "mdroots.anchorLinks",
    "mdroots.backlinks",
    "mdroots.info",
    "mdroots.renameFile",
];

/// An open document. `ws` is `None` when the file is not on disk (an
/// unsaved new buffer): such a document gets no diagnostics.
struct Doc {
    uri: Uri,
    path: PathBuf,
    version: i32,
    text: String,
    ws: Option<Workspace>,
}

pub(crate) struct Server {
    conn: Connection,
    workspaces: Workspaces,
    enc: PositionEncoding,
    setting: Setting,
    /// Open documents by URI string (`lsp_types::Uri` has interior
    /// mutability, so it is not a map key).
    docs: HashMap<String, Doc>,
    /// Messages read ahead of handling, so a `$/cancelRequest` behind a
    /// request is seen before the request runs.
    queue: VecDeque<Message>,
    /// Ids of queued requests the client cancelled.
    cancelled: HashSet<RequestId>,
    /// The request being handled and its token.
    current: Option<(RequestId, Cancel)>,
    /// URI string -> when its debounced diagnostics are due.
    due: HashMap<String, Instant>,
    shutdown: bool,
    /// Ids of the `workspace/applyEdit` requests sent to the client.
    apply_seq: u64,
    /// The client registers file watchers on request
    /// (`workspace.didChangeWatchedFiles.dynamicRegistration`).
    watch_dynamic: bool,
    /// The client re-requests code lenses on `workspace/codeLens/refresh`
    /// (`workspace.codeLens.refreshSupport`).
    lens_refresh: bool,
    /// Ids of the `workspace/codeLens/refresh` requests sent.
    lens_seq: u64,
    /// Roots of the watching workspaces subscribed to (one forwarding
    /// thread each).
    watched: HashSet<PathBuf>,
    /// The forwarding threads send a watching workspace's root here when
    /// its watcher changed notes; the message loop selects on it.
    changes: (Sender<PathBuf>, Receiver<PathBuf>),
}

/// What the message loop waits for.
enum Event {
    Message(Message),
    /// A watching workspace (by root) changed on disk.
    Changed(PathBuf),
}

impl Server {
    /// Runs the `initialize` handshake. A client that disconnects first
    /// gets a server that exits at once.
    pub(crate) fn start(conn: Connection, opts: Options) -> Result<Server, BoxError> {
        let (id, params) = match conn.initialize_start() {
            Ok(v) => v,
            Err(e) if e.channel_is_disconnected() => return Ok(Server::closed(conn, opts)),
            Err(e) => return Err(e.into()),
        };
        let offered: Option<Vec<PositionEncodingKind>> = params
            .pointer("/capabilities/general/positionEncodings")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let enc = position::negotiate(offered.as_deref());
        let folders: Vec<PathBuf> = params
            .get("workspaceFolders")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|f| f.get("uri")?.as_str()?.parse::<Uri>().ok())
            .filter_map(|u| uri::to_path(&u))
            .collect();
        let opts = if folders.is_empty() {
            opts
        } else {
            opts.workspace_folders(folders)
        };
        let result = json!({
            "capabilities": capabilities(enc),
            "serverInfo": { "name": "mdroots", "version": env!("CARGO_PKG_VERSION") },
        });
        let watch_dynamic = params
            .pointer("/capabilities/workspace/didChangeWatchedFiles/dynamicRegistration")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let lens_refresh = params
            .pointer("/capabilities/workspace/codeLens/refreshSupport")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut server = Server::closed(conn, opts);
        server.enc = enc;
        server.watch_dynamic = watch_dynamic;
        server.lens_refresh = lens_refresh;
        // `initialize_finish` waits for `initialized`, so the watcher is
        // registered here rather than in the message loop.
        match server.conn.initialize_finish(id, result) {
            Ok(()) => {
                server.register_watcher();
                Ok(server)
            }
            Err(e) if e.channel_is_disconnected() => Ok(server),
            Err(e) => Err(e.into()),
        }
    }

    fn closed(conn: Connection, opts: Options) -> Server {
        Server {
            conn,
            workspaces: Workspaces::new(opts),
            enc: PositionEncoding::Utf16,
            setting: Setting::default(),
            docs: HashMap::new(),
            queue: VecDeque::new(),
            cancelled: HashSet::new(),
            current: None,
            due: HashMap::new(),
            shutdown: false,
            apply_seq: 0,
            watch_dynamic: false,
            lens_refresh: false,
            lens_seq: 0,
            watched: HashSet::new(),
            changes: crossbeam_channel::unbounded(),
        }
    }

    /// Until `exit` (Ok after `shutdown`, else Err) or disconnect (Ok).
    pub(crate) fn run(mut self) -> Result<(), BoxError> {
        loop {
            if self.queue.is_empty() {
                match self.recv() {
                    Some(m) => self.queue.push_back(m),
                    None => return Ok(()),
                }
            }
            // Drain before popping, so a cancel or a didChange behind the
            // next request is seen before it runs.
            self.drain();
            let Some(msg) = self.queue.pop_front() else {
                continue;
            };
            match msg {
                Message::Request(req) => self.request(req),
                Message::Notification(n) if n.method == "exit" => {
                    return match self.shutdown {
                        true => Ok(()),
                        false => Err("exit without shutdown".into()),
                    };
                }
                // After shutdown only exit counts (LSP lifecycle).
                Message::Notification(_) if self.shutdown => {}
                Message::Notification(n) => self.notification(n),
                Message::Response(_) => {}
            }
        }
    }

    /// The next message, publishing debounced diagnostics as they fall
    /// due and republishing after watcher changes while waiting. `None`
    /// once the client is gone.
    fn recv(&mut self) -> Option<Message> {
        loop {
            let timeout = self
                .due
                .values()
                .min()
                .map(|at| at.saturating_duration_since(Instant::now()));
            let got: Option<Result<Event, RecvError>> = {
                let (conn, changes) = (&self.conn.receiver, &self.changes.1);
                let never = crossbeam_channel::never();
                let deadline = match timeout {
                    Some(t) => crossbeam_channel::after(t),
                    None => never.clone(),
                };
                select! {
                    recv(conn) -> m => Some(m.map(Event::Message)),
                    recv(changes) -> r => Some(r.map(Event::Changed)),
                    recv(deadline) -> _ => None,
                }
            };
            match got {
                Some(Ok(Event::Message(m))) => return Some(m),
                Some(Ok(Event::Changed(root))) => {
                    if !self.shutdown {
                        self.republish(&[root]);
                    }
                }
                // The changes channel never closes (the server holds a
                // sender), so this is the client going away.
                Some(Err(RecvError)) => return None,
                None => self.publish_due(),
            }
        }
    }

    /// Reads every message already sent into the queue, applying cancels:
    /// queued requests are marked, the running one has its token set. A
    /// `didChange` also cancels the queued requests on that document,
    /// which would otherwise answer on stale text. Long handlers may call
    /// this between steps.
    fn drain(&mut self) {
        loop {
            match self.conn.receiver.try_recv() {
                Ok(Message::Notification(n)) if n.method == "$/cancelRequest" => {
                    let Ok(p) = serde_json::from_value::<CancelParams>(n.params) else {
                        continue;
                    };
                    let id: RequestId = match p.id {
                        NumberOrString::Number(n) => n.into(),
                        NumberOrString::String(s) => s.into(),
                    };
                    match &self.current {
                        Some((cur, cancel)) if *cur == id => cancel.cancel(),
                        _ => {
                            self.cancelled.insert(id);
                        }
                    }
                }
                Ok(Message::Notification(n)) if n.method == "textDocument/didChange" => {
                    if let Some(uri) = n
                        .params
                        .pointer("/textDocument/uri")
                        .and_then(Value::as_str)
                    {
                        let stale: Vec<RequestId> = self
                            .queue
                            .iter()
                            .filter_map(|m| match m {
                                Message::Request(r) if doc_uri(r) == Some(uri) => {
                                    Some(r.id.clone())
                                }
                                _ => None,
                            })
                            .collect();
                        self.cancelled.extend(stale);
                    }
                    self.queue.push_back(Message::Notification(n));
                }
                Ok(m) => self.queue.push_back(m),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    fn publish_due(&mut self) {
        let now = Instant::now();
        let ready: Vec<String> = self
            .due
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for key in ready {
            self.due.remove(&key);
            self.publish(&key);
        }
    }

    fn request(&mut self, req: Request) {
        let cancelled = self.cancelled.remove(&req.id);
        if self.shutdown {
            return self.reply_err(req.id, ErrorCode::InvalidRequest, "shutdown requested");
        }
        if cancelled {
            return self.reply_err(req.id, ErrorCode::RequestCanceled, "cancelled");
        }
        let cancel = Cancel::new();
        self.current = Some((req.id.clone(), cancel.clone()));
        let result = self.dispatch(&req, &cancel);
        self.current = None;
        match result {
            Some(Ok(v)) => self.send(Response::new_ok(req.id, v).into()),
            Some(Err(Fail::Lib(e))) if e.kind() == ErrorKind::Cancelled => {
                self.reply_err(req.id, ErrorCode::RequestCanceled, e.message())
            }
            Some(Err(Fail::Lib(e))) => {
                self.reply_err(req.id, ErrorCode::InternalError, e.message())
            }
            Some(Err(Fail::Params(m))) => self.reply_err(req.id, ErrorCode::InvalidParams, &m),
            None => self.reply_err(
                req.id,
                ErrorCode::MethodNotFound,
                &format!("unknown method: {}", req.method),
            ),
        }
    }

    /// `None` for an unknown method.
    fn dispatch(&mut self, req: &Request, cancel: &Cancel) -> Option<Result<Value, Fail>> {
        let p = &req.params;
        Some(match req.method.as_str() {
            "shutdown" => {
                self.shutdown = true;
                // No diagnostics after shutdown.
                self.due.clear();
                Ok(Value::Null)
            }
            "textDocument/definition" => self.at(p, cancel, |c, pos| {
                features::definition(c, pos).map(|v| match v.is_empty() {
                    true => Value::Null,
                    false => json!(v),
                })
            }),
            "textDocument/references" => self.at(p, cancel, |c, pos| {
                features::references(c, pos).map(|v| json!(v))
            }),
            "textDocument/hover" => self.at(p, cancel, |c, pos| {
                features::hover(c, pos).map(|v| json!(v))
            }),
            "textDocument/completion" => self.at(p, cancel, |c, pos| {
                features::completion(c, pos).map(|v| json!(v))
            }),
            "textDocument/prepareRename" => self.at(p, cancel, |c, pos| {
                features::prepare_rename(c, pos).map(|v| json!(v))
            }),
            "textDocument/rename" => {
                let name = p.get("newName").and_then(Value::as_str).unwrap_or_default();
                let name = name.to_owned();
                self.at(p, cancel, |c, pos| {
                    features::rename(c, pos, &name, cancel).map(|v| json!(v))
                })
            }
            "textDocument/documentSymbol" => {
                let uri = p.pointer("/textDocument/uri").and_then(Value::as_str);
                self.with_doc(uri, cancel, |c| {
                    features::document_symbols(c).map(|v| json!(v))
                })
            }
            "textDocument/foldingRange" => {
                let uri = p.pointer("/textDocument/uri").and_then(Value::as_str);
                self.with_doc(uri, cancel, |c| {
                    features::folding_ranges(c).map(|v| json!(v))
                })
            }
            "textDocument/codeAction" => {
                let uri = p.pointer("/textDocument/uri").and_then(Value::as_str);
                // `context.only`: kinds the client asked for; ours matches a
                // prefix of it ("refactor", "refactor.extract", ...).
                let wanted = p
                    .pointer("/context/only")
                    .and_then(Value::as_array)
                    .is_none_or(|only| {
                        only.iter().filter_map(Value::as_str).any(|k| {
                            let ours = features::EXTRACT_KIND;
                            let ours = ours.as_str();
                            ours == k || ours.starts_with(&format!("{k}."))
                        })
                    });
                match p.get("range").cloned().map(serde_json::from_value) {
                    _ if !wanted => Ok(json!([])),
                    Some(Ok(range)) => self.with_doc(uri, cancel, |c| {
                        features::code_actions(c, range, cancel).map(|v| json!(v))
                    }),
                    _ => Ok(json!([])),
                }
                .map(|v| match v {
                    Value::Null => json!([]),
                    v => v,
                })
            }
            "textDocument/codeLens" => {
                let uri = p.pointer("/textDocument/uri").and_then(Value::as_str);
                self.with_doc(uri, cancel, |c| features::code_lenses(c).map(|v| json!(v)))
            }
            "workspace/symbol" => {
                let q = p.get("query").and_then(Value::as_str).unwrap_or_default();
                cancel
                    .check()
                    .map_err(Fail::Lib)
                    .map(|()| json!(features::workspace_symbols(&self.workspaces.all(), q)))
            }
            "workspace/executeCommand" => self.command(p, cancel),
            _ => return None,
        })
    }

    /// Runs `f` on the document and position of a
    /// `TextDocumentPositionParams` request.
    fn at(
        &self,
        p: &Value,
        cancel: &Cancel,
        f: impl FnOnce(&Ctx, Position) -> Result<Value, Fail>,
    ) -> Result<Value, Fail> {
        let uri = p.pointer("/textDocument/uri").and_then(Value::as_str);
        let pos = p
            .get("position")
            .cloned()
            .map(serde_json::from_value::<Position>);
        match pos {
            Some(Ok(pos)) => self.with_doc(uri, cancel, |c| f(c, pos)),
            _ => Ok(Value::Null),
        }
    }

    /// Runs `f` on the note `uri` names: its workspace is the open
    /// document's, else the one serving the file. `null` when the URI is
    /// not a note mdroots indexes.
    fn with_doc(
        &self,
        uri: Option<&str>,
        cancel: &Cancel,
        f: impl FnOnce(&Ctx) -> Result<Value, Fail>,
    ) -> Result<Value, Fail> {
        cancel.check()?;
        let Some(uri) = uri else {
            return Ok(Value::Null);
        };
        let Some((ws, path)) = self.workspace_of(uri) else {
            return Ok(Value::Null);
        };
        match Ctx::new(&ws, path, self.enc) {
            Some(c) => f(&c),
            None => Ok(Value::Null),
        }
    }

    fn workspace_of(&self, uri: &str) -> Option<(Workspace, PathBuf)> {
        if let Some(d) = self.docs.get(uri) {
            return Some((d.ws.clone()?, d.path.clone()));
        }
        let path = uri::to_path(&uri.parse().ok()?)?;
        let ws = self.workspaces.for_path(&path).ok()?;
        Some((ws, path))
    }

    /// `workspace/executeCommand`: the commands in [`COMMANDS`].
    fn command(&mut self, p: &Value, cancel: &Cancel) -> Result<Value, Fail> {
        let Ok(p) = serde_json::from_value::<ExecuteCommandParams>(p.clone()) else {
            return Err(Fail::Params("bad executeCommand params".to_owned()));
        };
        let arg = |i: usize| p.arguments.get(i).and_then(Value::as_str);
        match p.command.as_str() {
            "mdroots.anchorLinks" => {
                let slug = arg(1).unwrap_or_default().to_owned();
                self.with_doc(arg(0), cancel, |c| {
                    features::anchor_links(c, &slug).map(|v| json!(v))
                })
            }
            "mdroots.backlinks" => {
                self.with_doc(arg(0), cancel, |c| features::backlinks(c).map(|v| json!(v)))
            }
            "mdroots.info" => {
                let info = self.with_doc(arg(0), cancel, |c| Ok(json!(info(c.ws))))?;
                if let Some(msg) = info.as_str() {
                    let params = ShowMessageParams {
                        typ: MessageType::INFO,
                        message: msg.to_owned(),
                    };
                    self.send(Notification::new("window/showMessage".to_owned(), params).into());
                }
                Ok(info)
            }
            "mdroots.renameFile" => {
                let to = arg(1)
                    .and_then(|u| u.parse::<Uri>().ok())
                    .and_then(|u| uri::to_path(&u))
                    .ok_or_else(|| Fail::Params("mdroots.renameFile: bad target URI".to_owned()))?;
                let enc = self.enc;
                let edit = self.with_doc(arg(0), cancel, |c| {
                    features::rename_file(c.ws, &c.path, &to, enc, cancel).map(|v| json!(v))
                })?;
                if !edit.is_null() {
                    self.apply_seq += 1;
                    let id: RequestId = format!("mdroots/applyEdit/{}", self.apply_seq).into();
                    let params = json!({ "label": "Rename note", "edit": edit });
                    self.send(Request::new(id, "workspace/applyEdit".to_owned(), params).into());
                }
                Ok(Value::Null)
            }
            c => Err(Fail::Params(format!("unknown command: {c}"))),
        }
    }

    fn notification(&mut self, n: Notification) {
        match n.method.as_str() {
            "textDocument/didOpen" => {
                if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(n.params) {
                    let d = p.text_document;
                    let key = d.uri.as_str().to_owned();
                    self.due.remove(&key);
                    if self.update(d.uri, d.version, d.text) {
                        self.publish(&key);
                    }
                }
            }
            "textDocument/didChange" => {
                if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(n.params) {
                    // Full sync: the last change holds the whole text.
                    let Some(text) = p.content_changes.into_iter().last().map(|c| c.text) else {
                        return;
                    };
                    let key = p.text_document.uri.as_str().to_owned();
                    if self.update(p.text_document.uri, p.text_document.version, text) {
                        self.due.insert(key, Instant::now() + DEBOUNCE);
                    }
                }
            }
            "textDocument/didSave" => {
                if let Ok(p) = serde_json::from_value::<DidSaveTextDocumentParams>(n.params) {
                    let key = p.text_document.uri.as_str().to_owned();
                    self.due.remove(&key);
                    match self.docs.get(&key).and_then(|d| d.ws.clone()) {
                        Some(ws) => self.refresh(&[ws]),
                        None => self.publish(&key),
                    }
                }
            }
            "workspace/didChangeWatchedFiles" => {
                if let Ok(p) = serde_json::from_value::<DidChangeWatchedFilesParams>(n.params) {
                    let paths: Vec<PathBuf> = p
                        .changes
                        .iter()
                        .filter_map(|c| uri::to_path(&c.uri))
                        .collect();
                    // A watching workspace already follows the disk itself.
                    let affected: Vec<Workspace> = self
                        .workspaces
                        .all()
                        .into_iter()
                        .filter(|ws| !ws.watching())
                        .filter(|ws| {
                            let root = ws.root().path;
                            paths.iter().any(|p| p.starts_with(&root))
                        })
                        .collect();
                    self.refresh(&affected);
                }
            }
            "textDocument/didClose" => {
                if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(n.params) {
                    let key = p.text_document.uri.as_str().to_owned();
                    self.due.remove(&key);
                    if let Some(doc) = self.docs.remove(&key) {
                        if let Some(ws) = &doc.ws {
                            log_err("didClose", ws.clear_overlay(&doc.path));
                        }
                        self.send_diagnostics(doc.uri, Vec::new(), None);
                    }
                }
            }
            "workspace/didChangeConfiguration" => {
                if let Ok(p) = serde_json::from_value::<DidChangeConfigurationParams>(n.params) {
                    self.setting = Setting::from_settings(&p.settings);
                    self.due.clear();
                    let mut keys: Vec<String> = self.docs.keys().cloned().collect();
                    keys.sort();
                    for k in keys {
                        self.publish(&k);
                    }
                }
            }
            _ => {}
        }
    }

    /// Asks the client to watch the notes, if it registers watchers on
    /// request; [Neovim](https://neovim.io) offers this on some platforms
    /// only.
    fn register_watcher(&mut self) {
        if !self.watch_dynamic {
            return;
        }
        let opts = DidChangeWatchedFilesRegistrationOptions {
            watchers: vec![FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/*.{md,markdown,org}".to_owned()),
                kind: None,
            }],
        };
        let params = RegistrationParams {
            registrations: vec![Registration {
                id: "mdroots/watch".to_owned(),
                method: "workspace/didChangeWatchedFiles".to_owned(),
                register_options: serde_json::to_value(opts).ok(),
            }],
        };
        let id: RequestId = "mdroots/registerCapability".to_owned().into();
        self.send(Request::new(id, "client/registerCapability".to_owned(), params).into());
    }

    /// Re-reads each workspace from disk ([`Workspace::refresh`]), then
    /// republishes the diagnostics of the open documents in them.
    fn refresh(&mut self, wss: &[Workspace]) {
        let mut roots = Vec::new();
        for ws in wss {
            log_err("refresh", ws.refresh(&Cancel::new()));
            roots.push(ws.root().path);
        }
        self.republish(&roots);
    }

    /// Republishes the diagnostics of the open documents in the
    /// workspaces with these roots, then asks the client to re-request
    /// code lenses (backlink counts may have changed).
    fn republish(&mut self, roots: &[PathBuf]) {
        let mut keys: Vec<String> = self
            .docs
            .iter()
            .filter(|(_, d)| {
                d.ws.as_ref()
                    .is_some_and(|w| roots.contains(&w.root().path))
            })
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        for k in keys {
            self.due.remove(&k);
            self.publish(&k);
        }
        if !roots.is_empty() {
            self.refresh_lenses();
        }
    }

    /// `workspace/codeLens/refresh`, if the client supports it; its answer
    /// is ignored.
    fn refresh_lenses(&mut self) {
        if !self.lens_refresh {
            return;
        }
        self.lens_seq += 1;
        let id: RequestId = format!("mdroots/codeLensRefresh/{}", self.lens_seq).into();
        self.send(Request::new(id, "workspace/codeLens/refresh".to_owned(), Value::Null).into());
    }

    /// Records the document's new text and sets it as the workspace
    /// overlay. Returns whether the document is tracked (a `file:` URI).
    fn update(&mut self, uri: Uri, version: i32, text: String) -> bool {
        let Some(path) = uri::to_path(&uri) else {
            return false;
        };
        let key = uri.as_str().to_owned();
        let ws = match self.docs.get(&key).and_then(|d| d.ws.clone()) {
            Some(ws) => Some(ws),
            None => match self.workspaces.for_path(&path) {
                Ok(ws) => Some(ws),
                Err(e) => {
                    eprintln!(
                        "mdroots-lsp: {}: no workspace: {}",
                        path.display(),
                        e.message()
                    );
                    None
                }
            },
        };
        if let Some(ws) = &ws {
            log_err("overlay", ws.set_overlay(&path, &text));
            self.follow(ws);
        }
        let doc = Doc {
            uri,
            path,
            version,
            text,
            ws,
        };
        self.docs.insert(key, doc);
        true
    }

    /// Subscribes to a watching workspace once: a thread forwards its
    /// change notifications into the message loop as the root path. The
    /// thread ends when the workspace (its sender) or the server is gone.
    fn follow(&mut self, ws: &Workspace) {
        let root = ws.root().path;
        if !ws.watching() || self.watched.contains(&root) {
            return;
        }
        let rx = ws.subscribe();
        let tx = self.changes.0.clone();
        let fwd_root = root.clone();
        let spawned = std::thread::Builder::new()
            .name("mdroots-lsp-watch".to_owned())
            .spawn(move || {
                for _ in rx {
                    if tx.send(fwd_root.clone()).is_err() {
                        return;
                    }
                }
            });
        match spawned {
            Ok(_) => {
                self.watched.insert(root);
            }
            Err(e) => eprintln!("mdroots-lsp: watch: {e}"),
        }
    }

    /// Publishes the open document's diagnostics now.
    fn publish(&mut self, key: &str) {
        let Some(doc) = self.docs.get(key) else {
            return;
        };
        let Some(ws) = &doc.ws else {
            return;
        };
        let cancel = Cancel::new();
        let diags = match ws.diagnostics(&doc.path, &cancel) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("mdroots-lsp: {}: {}", doc.path.display(), e.message());
                return;
            }
        };
        let index = LineIndex::new(&doc.text);
        let root = ws.root().path;
        let out = self
            .setting
            .apply(diags)
            .iter()
            .map(|d| diagnostics::to_lsp(d, &index, &doc.text, self.enc, &root))
            .collect();
        let (uri, version) = (doc.uri.clone(), doc.version);
        self.send_diagnostics(uri, out, Some(version));
    }

    fn send_diagnostics(
        &self,
        uri: Uri,
        diagnostics: Vec<lsp_types::Diagnostic>,
        version: Option<i32>,
    ) {
        let params = PublishDiagnosticsParams {
            uri,
            diagnostics,
            version,
        };
        self.send(Notification::new("textDocument/publishDiagnostics".to_owned(), params).into());
    }

    fn reply_err(&self, id: RequestId, code: ErrorCode, msg: &str) {
        self.send(Response::new_err(id, code as i32, msg.to_owned()).into());
    }

    /// A failed send means the client is gone; the next receive ends the
    /// loop.
    fn send(&self, m: Message) {
        let _ = self.conn.sender.send(m);
    }
}

/// The request's `textDocument.uri`, if it has one.
fn doc_uri(r: &Request) -> Option<&str> {
    r.params.pointer("/textDocument/uri")?.as_str()
}

/// What `mdroots.info` reports.
fn info(ws: &Workspace) -> String {
    let r = ws.root();
    format!(
        "root: {}\nmode: {}\nwhy: {}\nfiles: {}",
        r.path.display(),
        names::mode(r.mode),
        r.reason,
        ws.files().len()
    )
}

fn log_err(what: &str, r: Result<(), mdroots::Error>) {
    if let Err(e) = r {
        eprintln!("mdroots-lsp: {what}: {}", e.message());
    }
}

fn capabilities(enc: PositionEncoding) -> ServerCapabilities {
    let trigger = ["[", "(", "#", ":"].map(str::to_owned).to_vec();
    ServerCapabilities {
        position_encoding: Some(position::kind(enc)),
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
                ..Default::default()
            },
        )),
        definition_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        hover_provider: Some(true.into()),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(trigger),
            ..Default::default()
        }),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![features::EXTRACT_KIND]),
            resolve_provider: Some(false),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        code_lens_provider: Some(CodeLensOptions {
            resolve_provider: Some(false),
        }),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: COMMANDS.map(str::to_owned).to_vec(),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        ..Default::default()
    }
}
