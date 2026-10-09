//! mdroots-lsp: a synchronous language server over [`mdroots::Workspaces`]
//! (docs/specs/library.md §3.6, §4). One thread runs the message loop:
//! requests are answered in arrival order, so each sees the edits before
//! it; no async runtime. Roots open on one background thread: until a
//! document's root is open it is served alone (single-file), and its
//! diagnostics are published again when the open finishes.
//!
//! The binary entry point is `mdroots lsp` in mdroots-cli:
//!
//! ```no_run
//! let (conn, io) = lsp_server::Connection::stdio();
//! mdroots_lsp::serve(conn).unwrap();
//! io.join().unwrap();
//! ```
#![forbid(unsafe_code)]

mod diagnostics;
mod features;
mod position;
mod server;
mod uri;

use lsp_server::Connection;

/// Serve one client on `conn` with default [`mdroots::Options`] until it
/// sends `exit` or disconnects. `Err` on a protocol error, or on `exit`
/// without a preceding `shutdown`.
pub fn serve(conn: Connection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    serve_with(conn, mdroots::Options::default())
}

/// [`serve`] with explicit options for every workspace the server opens
/// (tests pass an in-memory index or a temp cache dir). Workspace folders
/// from `initialize` are added to them.
pub fn serve_with(
    conn: Connection,
    opts: mdroots::Options,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    server::Server::start(conn, opts)?.run()
}
