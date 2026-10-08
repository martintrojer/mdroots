//! `mdroots lsp`: the language server of mdroots-lsp on stdin/stdout.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use lsp_server::{Connection, Message};
use mdroots::{Error, ErrorKind};

use crate::Outcome;

/// Serves until `exit` or stdin EOF. With `log`, one line per message is
/// appended to that file: time, direction (`<-` from the client, `->` to
/// it), method or `response`, and id.
pub fn run(log: Option<&str>) -> Result<Outcome, Error> {
    let (conn, io) = Connection::stdio();
    let served = match log {
        None => mdroots_lsp::serve(conn),
        Some(path) => {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| Error::new(ErrorKind::Io, format!("{path}: {e}")))?;
            logged(conn, Arc::new(Mutex::new(file)))
        }
    };
    served.map_err(|e| Error::new(ErrorKind::Io, format!("lsp: {e}")))?;
    io.join()
        .map_err(|e| Error::new(ErrorKind::Io, format!("lsp: {e}")))?;
    Ok(Outcome::Ok)
}

/// Serves on an in-memory connection, forwarding (and logging) each
/// message between it and `stdio`.
fn logged(
    stdio: Connection,
    log: Arc<Mutex<File>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (server, proxy) = Connection::memory();
    let Connection { sender, receiver } = stdio;
    let inbound = {
        let log = log.clone();
        let to_server = proxy.sender;
        std::thread::spawn(move || {
            for m in receiver {
                write_line(&log, "<-", &m);
                if to_server.send(m).is_err() {
                    break;
                }
            }
        })
    };
    let outbound = {
        let from_server = proxy.receiver;
        std::thread::spawn(move || {
            for m in from_server {
                write_line(&log, "->", &m);
                if sender.send(m).is_err() {
                    break;
                }
            }
        })
    };
    let served = mdroots_lsp::serve(server);
    // The server's end is dropped: outbound drains and drops the stdio
    // sender, which lets the writer thread finish.
    let _ = outbound.join();
    // Inbound ends at stdin EOF or after `exit`; only wait when the
    // server ended normally, which is after `exit` or EOF.
    if served.is_ok() {
        let _ = inbound.join();
    }
    served
}

fn write_line(log: &Mutex<File>, dir: &str, m: &Message) {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let what = match m {
        Message::Request(r) => format!("{} id={}", r.method, r.id),
        Message::Response(r) => format!("response id={}", r.id),
        Message::Notification(n) => n.method.clone(),
    };
    let mut f = log.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(f, "{ms} {dir} {what}");
}
