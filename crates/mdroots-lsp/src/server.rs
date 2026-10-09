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

/// How long a background open runs before the client is told about it.
const PROGRESS_AFTER: Duration = Duration::from_secs(1);

/// An open document. `ws` is `None` when the file is not on disk (an
/// unsaved new buffer): such a document gets no diagnostics. Until its
/// root's workspace is open (`ready`), `ws` is a single-file workspace
/// ([`Workspace::open_single`]) that only this document holds.
struct Doc {
    uri: Uri,
    path: PathBuf,
    version: i32,
    text: String,
    ws: Option<Workspace>,
    ready: bool,
}

/// What the background opener thread reports.
enum Opening {
    /// It started opening the workspace of this file.
    Started(PathBuf),
    /// It finished; the workspace is now cached in [`Workspaces`].
    Done(PathBuf, Result<Workspace, mdroots::Error>),
}

/// The background open in progress.
struct Running {
    file: PathBuf,
    since: Instant,
    /// The client was told (progress begun or a message shown).
    announced: bool,
    /// The work-done progress token, if a progress was begun.
    token: Option<String>,
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
    /// Files to open on the background opener thread, started on first
    /// use; one open at a time, in order.
    jobs: Option<Sender<PathBuf>>,
    /// Files submitted to the opener and not done yet (de-duplicates).
    queued: HashSet<PathBuf>,
    /// The opener thread reports here; the message loop selects on it.
    opened: (Sender<Opening>, Receiver<Opening>),
    running: Option<Running>,
    /// The client shows work-done progress (`window.workDoneProgress`).
    progress: bool,
    /// Ids of the progress tokens created.
    progress_seq: u64,
    /// The "indexing" message was shown (clients without progress).
    indexing_shown: bool,
}

/// What the message loop waits for.
enum Event {
    Message(Message),
    /// A watching workspace (by root) changed on disk.
    Changed(PathBuf),
    Opened(Opening),
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
        let progress = params
            .pointer("/capabilities/window/workDoneProgress")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut server = Server::closed(conn, opts);
        server.progress = progress;
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
            jobs: None,
            queued: HashSet::new(),
            opened: crossbeam_channel::unbounded(),
            running: None,
            progress: false,
            progress_seq: 0,
            indexing_shown: false,
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
            // A busy queue must not hold back finished opens or progress.
            while let Ok(ev) = self.opened.1.try_recv() {
                self.on_opening(ev);
            }
            self.announce_due();
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
            let announce = self
                .announce_at()
                .map(|at| at.saturating_duration_since(Instant::now()));
            let got: Option<Result<Event, RecvError>> = {
                let (conn, changes) = (&self.conn.receiver, &self.changes.1);
                let opened = &self.opened.1;
                let after = |t: Option<Duration>| match t {
                    Some(t) => crossbeam_channel::after(t),
                    None => crossbeam_channel::never(),
                };
                let (deadline, announce) = (after(timeout), after(announce));
                select! {
                    recv(conn) -> m => Some(m.map(Event::Message)),
                    recv(changes) -> r => Some(r.map(Event::Changed)),
                    recv(opened) -> r => Some(r.map(Event::Opened)),
                    recv(deadline) -> _ => None,
                    recv(announce) -> _ => None,
                }
            };
            match got {
                Some(Ok(Event::Message(m))) => return Some(m),
                Some(Ok(Event::Changed(root))) => {
                    if !self.shutdown {
                        self.republish(&[root]);
                    }
                }
                Some(Ok(Event::Opened(ev))) => self.on_opening(ev),
                // The changes and opened channels never close (the server
                // holds a sender), so this is the client going away.
                Some(Err(RecvError)) => return None,
                None => {
                    self.publish_due();
                    self.announce_due();
                }
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
    /// document's, else the open workspace serving the file, else the file
    /// alone. `null` when the URI is not a note mdroots indexes.
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
        // Never opens a root here: that would block the loop.
        let ws = match self.workspaces.get(&path) {
            Some(ws) => ws,
            None => Workspace::open_single(&path, self.workspaces.options()).ok()?,
        };
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
                    match self.docs.get(&key).map(|d| (d.ws.clone(), d.ready)) {
                        Some((Some(ws), true)) => self.refresh(&[ws]),
                        Some((Some(ws), false)) => {
                            log_err("refresh", ws.refresh(&Cancel::new()));
                            self.publish(&key);
                        }
                        _ => self.publish(&key),
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
    /// workspaces with these roots (not those still served single-file),
    /// then asks the client to re-request code lenses (backlink counts may
    /// have changed).
    fn republish(&mut self, roots: &[PathBuf]) {
        let mut keys: Vec<String> = self
            .docs
            .iter()
            .filter(|(_, d)| {
                d.ready
                    && d.ws
                        .as_ref()
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
    /// overlay. A new document whose root is not open yet gets a
    /// single-file workspace at once and its root is opened in the
    /// background. Returns whether the document is tracked (a `file:` URI).
    fn update(&mut self, uri: Uri, version: i32, text: String) -> bool {
        let Some(path) = uri::to_path(&uri) else {
            return false;
        };
        let key = uri.as_str().to_owned();
        let (ws, ready) = match self.docs.get(&key) {
            Some(d) => (d.ws.clone(), d.ready),
            None => match self.workspaces.get(&path) {
                Some(ws) => (Some(ws), true),
                None => match Workspace::open_single(&path, self.workspaces.options()) {
                    Ok(ws) => {
                        self.submit(&path);
                        (Some(ws), false)
                    }
                    Err(e) => {
                        eprintln!(
                            "mdroots-lsp: {}: no workspace: {}",
                            path.display(),
                            e.message()
                        );
                        (None, false)
                    }
                },
            },
        };
        if let Some(ws) = &ws {
            log_err("overlay", ws.set_overlay(&path, &text));
            if ready {
                self.follow(ws);
            }
        }
        let doc = Doc {
            uri,
            path,
            version,
            text,
            ws,
            ready,
        };
        self.docs.insert(key, doc);
        true
    }

    /// Queues the opening of `file`'s workspace on the opener thread
    /// (started on first use), once per file until it is done.
    fn submit(&mut self, file: &std::path::Path) {
        if !self.queued.insert(file.to_path_buf()) {
            return;
        }
        if self.jobs.is_none() {
            let (tx, rx) = crossbeam_channel::unbounded::<PathBuf>();
            let (wss, out) = (self.workspaces.clone(), self.opened.0.clone());
            let spawned = std::thread::Builder::new()
                .name("mdroots-lsp-open".to_owned())
                .spawn(move || {
                    for file in rx {
                        // Opened meanwhile (another file of the root): no
                        // discovery, no progress.
                        let res = match wss.get(&file) {
                            Some(ws) => Ok(ws),
                            None => {
                                if out.send(Opening::Started(file.clone())).is_err() {
                                    return;
                                }
                                wss.for_path(&file)
                            }
                        };
                        if out.send(Opening::Done(file, res)).is_err() {
                            return;
                        }
                    }
                });
            match spawned {
                Ok(_) => self.jobs = Some(tx),
                Err(e) => {
                    // The document stays single-file.
                    eprintln!("mdroots-lsp: opener: {e}");
                    self.queued.remove(file);
                    return;
                }
            }
        }
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(file.to_path_buf());
        }
    }

    fn on_opening(&mut self, ev: Opening) {
        match ev {
            Opening::Started(file) => {
                self.running = Some(Running {
                    file,
                    since: Instant::now(),
                    announced: false,
                    token: None,
                });
            }
            Opening::Done(file, res) => {
                self.queued.remove(&file);
                if let Some(token) = self.running.take().and_then(|r| r.token) {
                    self.send_progress(&token, json!({ "kind": "end" }));
                }
                match res {
                    Ok(_) => self.adopt(),
                    // The document stays on its single-file workspace.
                    Err(e) => eprintln!(
                        "mdroots-lsp: {}: open failed: {}",
                        file.display(),
                        e.message()
                    ),
                }
            }
        }
    }

    /// Moves every document still served single-file whose workspace is
    /// now open onto it, then republishes the diagnostics of the open
    /// documents of those roots.
    fn adopt(&mut self) {
        let mut keys: Vec<String> = self
            .docs
            .iter()
            .filter(|(_, d)| !d.ready && d.ws.is_some())
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        let mut roots = Vec::new();
        for k in keys {
            let Some(d) = self.docs.get_mut(&k) else {
                continue;
            };
            let Some(real) = self.workspaces.get(&d.path) else {
                continue;
            };
            if let Some(single) = d.ws.replace(real.clone()) {
                log_err("overlay", single.clear_overlay(&d.path));
            }
            d.ready = true;
            log_err("overlay", real.set_overlay(&d.path, &d.text));
            let root = real.root().path;
            if !roots.contains(&root) {
                roots.push(root);
            }
            self.follow(&real);
        }
        if !self.shutdown {
            self.republish(&roots);
        }
    }

    /// When the running open should be announced, if it has not been.
    fn announce_at(&self) -> Option<Instant> {
        let r = self.running.as_ref().filter(|r| !r.announced)?;
        Some(r.since + PROGRESS_AFTER)
    }

    /// Tells the client about an open that has run for [`PROGRESS_AFTER`]:
    /// a work-done progress if it supports one, else one message per
    /// session.
    fn announce_due(&mut self) {
        if self.shutdown || self.announce_at().is_none_or(|at| at > Instant::now()) {
            return;
        }
        let Some(r) = self.running.as_mut() else {
            return;
        };
        r.announced = true;
        let dir = r.file.parent().unwrap_or(&r.file).display().to_string();
        let title = format!("mdroots: indexing {dir}");
        if self.progress {
            self.progress_seq += 1;
            let token = format!("mdroots/index/{}", self.progress_seq);
            r.token = Some(token.clone());
            let id: RequestId = format!("mdroots/progressCreate/{}", self.progress_seq).into();
            let params = json!({ "token": token });
            self.send(Request::new(id, "window/workDoneProgress/create".to_owned(), params).into());
            let begin = json!({ "kind": "begin", "title": title, "cancellable": false });
            self.send_progress(&token, begin);
        } else if !self.indexing_shown {
            self.indexing_shown = true;
            let params = ShowMessageParams {
                typ: MessageType::INFO,
                message: format!("{title}\u{2026}"),
            };
            self.send(Notification::new("window/showMessage".to_owned(), params).into());
        }
    }

    fn send_progress(&self, token: &str, value: Value) {
        let params = json!({ "token": token, "value": value });
        self.send(Notification::new("$/progress".to_owned(), params).into());
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
