//! `mdroots`: the command line over the `mdroots` facade
//! (docs/specs/library.md §6). Every command is formatting over
//! [`Workspace`]; the CLI holds no logic of its own and reads no notes
//! itself.
#![forbid(unsafe_code)]

mod lsp;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use mdroots::syntax::PositionEncoding;
use mdroots::{Cancel, Diagnostic, Error, ErrorKind, Options, Role, Severity, Workspace, names};

const USAGE: &str = "\
usage: mdroots <command> [args]

commands:
  check [--quiet] [PATH...]   report diagnostics of notes under each PATH
                              (a file or a directory; default .); exit 1 on
                              any error or warning
  roots PATH                  show the root chosen for PATH and why
  resolve FROM LINK           resolve LINK as written in the note FROM
  backlinks NOTE              list the notes linking to NOTE
  search [--] QUERY [PATH]    notes containing every word of QUERY (the
                              last also as a prefix): a file searches its
                              root, a directory (default .) is scanned in
                              memory; exit 1 without a hit
  lsp [--stdio] [--log FILE]  the language server on stdin/stdout (--stdio
                              is accepted and ignored); --log appends one
                              line per message to FILE

environment:
  MDROOTS_CACHE_DIR           cache dir to use instead of the user's (for
                              tests)
";

/// How a command ended, before it becomes an exit code.
enum Outcome {
    Ok,
    /// Ran, but found problems (exit 1).
    Fail,
    /// Bad arguments: usage to stderr (exit 2).
    Usage,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(Outcome::Ok) => ExitCode::SUCCESS,
        Ok(Outcome::Fail) => ExitCode::from(1),
        Ok(Outcome::Usage) => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("mdroots: {}", e.message());
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<Outcome, Error> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(Outcome::Usage);
    }
    let Some((cmd, rest)) = args.split_first() else {
        return Ok(Outcome::Usage);
    };
    let cwd = canonical(Path::new("."))?;
    let out = Out { cwd };
    match (cmd.as_str(), rest) {
        ("check", rest) => check(&out, rest),
        ("roots", [path]) if !is_flag(path) => roots(path),
        ("resolve", [from, link]) if !is_flag(from) => resolve(&out, from, link),
        ("backlinks", [note]) if !is_flag(note) => backlinks(&out, note),
        ("search", rest) => match search_args(rest) {
            Some((query, path)) => search(&out, query, path),
            None => Ok(Outcome::Usage),
        },
        ("__open", [path, flag, ms, rest @ ..]) if flag == "--hold-ms" && !is_flag(path) => {
            let every = match rest {
                [] => Some(None),
                [f, ms] if f == "--refresh-every" => ms.parse().ok().map(Some),
                _ => None,
            };
            match (ms.parse(), every) {
                (Ok(ms), Some(every)) => open_and_hold(path, ms, every),
                _ => Ok(Outcome::Usage),
            }
        }
        ("__gc", [flag, ms, force]) if flag == "--now-ms" && force == "--force" => {
            match ms.parse() {
                Ok(ms) => run_gc(ms),
                Err(_) => Ok(Outcome::Usage),
            }
        }
        ("lsp", rest) => match lsp_args(rest) {
            Some(log) => lsp::run(log.map(String::as_str)),
            None => Ok(Outcome::Usage),
        },
        _ => Ok(Outcome::Usage),
    }
}

/// `lsp` arguments: `--log FILE` and `--stdio` (stdio is the only
/// transport; accepted because many editor configs pass it), each at most
/// once, in any order. `None` on anything else.
fn lsp_args(args: &[String]) -> Option<Option<&String>> {
    let (mut log, mut stdio) = (None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--stdio" if !stdio => stdio = true,
            "--log" if log.is_none() => log = Some(it.next().filter(|f| !is_flag(f))?),
            _ => return None,
        }
    }
    Some(log)
}

/// Options for every workspace the CLI opens: the cache dir comes from
/// `MDROOTS_CACHE_DIR` when set (tests point it at a temp dir).
pub(crate) fn options() -> Options {
    match std::env::var_os("MDROOTS_CACHE_DIR") {
        Some(d) if !d.is_empty() => Options::default().cache_dir(PathBuf::from(d)),
        _ => Options::default(),
    }
}

fn role_name(role: Option<Role>) -> &'static str {
    match role {
        Some(Role::Reconciler) => "reconciler",
        Some(Role::Peer) => "peer",
        None => "memory",
    }
}

/// Hidden test command (`__open PATH --hold-ms N [--refresh-every MS]`):
/// open the workspace of PATH, print its role, file count and DB file, then
/// keep it (and its locks) open for N ms, refreshing every MS ms and
/// printing `files:` and `db:` again after each refresh. The many-process
/// tests run several at once.
fn open_and_hold(path: &str, hold_ms: u64, every: Option<u64>) -> Result<Outcome, Error> {
    let ws = Workspace::open_for(Path::new(path), options())?;
    println!("role: {}", role_name(ws.role()));
    print_state(&ws);
    let end = Instant::now() + Duration::from_millis(hold_ms);
    let Some(every) = every.filter(|e| *e > 0) else {
        std::thread::sleep(end.saturating_duration_since(Instant::now()));
        return Ok(Outcome::Ok);
    };
    let cancel = Cancel::new();
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(every)));
        ws.refresh(&cancel)?;
        print_state(&ws);
    }
    Ok(Outcome::Ok)
}

fn print_state(ws: &Workspace) {
    println!("files: {}", ws.files().len());
    match ws.cache() {
        Some(db) => println!("db: {}", db.display()),
        None => println!("db: memory"),
    }
    let _ = std::io::stdout().flush();
}

/// Hidden test command (`__gc --now-ms N --force`): run cache GC on
/// `MDROOTS_CACHE_DIR` (required, so it never touches the user's cache) as
/// of N ms, ignoring the daily gate, and print `deleted: PATH` and
/// `skipped: PATH` lines.
fn run_gc(now_ms: u64) -> Result<Outcome, Error> {
    let Some(dir) = std::env::var_os("MDROOTS_CACHE_DIR").filter(|d| !d.is_empty()) else {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "MDROOTS_CACHE_DIR is not set",
        ));
    };
    let opts = mdroots::index::GcOptions {
        force: true,
        ..Default::default()
    };
    let report = mdroots::index::gc(Path::new(&dir), now_ms, opts)?.unwrap_or_default();
    for p in &report.deleted {
        println!("deleted: {}", p.display());
    }
    for p in &report.skipped_busy {
        println!("skipped: {}", p.display());
    }
    Ok(Outcome::Ok)
}

fn is_flag(a: &str) -> bool {
    a.starts_with('-') && a != "-"
}

/// Prints paths relative to the canonical cwd when under it.
struct Out {
    cwd: PathBuf,
}

impl Out {
    fn show(&self, p: &Path) -> String {
        match p.strip_prefix(&self.cwd) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_owned(),
            Ok(rel) => rel.display().to_string(),
            Err(_) => p.display().to_string(),
        }
    }
}

fn canonical(p: &Path) -> Result<PathBuf, Error> {
    std::fs::canonicalize(p).map_err(|e| Error::new(ErrorKind::Io, format!("{}: {e}", p.display())))
}

fn check(out: &Out, args: &[String]) -> Result<Outcome, Error> {
    let mut quiet = false;
    let mut paths = Vec::new();
    for a in args {
        match a.as_str() {
            "--quiet" => quiet = true,
            a if is_flag(a) => return Ok(Outcome::Usage),
            a => paths.push(PathBuf::from(a)),
        }
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }
    let cancel = Cancel::new();
    // Canonical file path -> its diagnostics and line/col lookups; the
    // first PATH that covers a file wins.
    let mut files: BTreeMap<PathBuf, Vec<(u32, u32, Diagnostic)>> = BTreeMap::new();
    for p in &paths {
        let p = canonical(p)?;
        let is_dir = std::fs::metadata(&p)
            .map_err(|e| Error::new(ErrorKind::Io, format!("{}: {e}", p.display())))?
            .is_dir();
        let (ws, notes) = if is_dir {
            let ws = Workspace::open_at(&p, options())?;
            let notes = ws.files();
            (ws, notes)
        } else {
            (Workspace::open_for(&p, options())?, vec![p])
        };
        for f in notes {
            if files.contains_key(&f) {
                continue;
            }
            let mut v = Vec::new();
            for d in ws.diagnostics(&f, &cancel)? {
                let (line, col) = ws.line_col(&f, d.range.start, PositionEncoding::Utf32)?;
                v.push((line, col, d));
            }
            files.insert(f, v);
        }
    }
    let mut counts = [0usize; 4];
    let mut stdout = std::io::stdout().lock();
    for (f, diags) in &files {
        let mut diags: Vec<_> = diags.iter().collect();
        diags.sort_by_key(|(l, c, _)| (*l, *c));
        for (line, col, d) in diags {
            let i = match d.severity {
                Severity::Error => 0,
                Severity::Warning => 1,
                Severity::Info => 2,
                Severity::Hint => 3,
            };
            counts[i] += 1;
            let _ = writeln!(
                stdout,
                "{}:{}:{}: {}: {}",
                out.show(f),
                line + 1,
                col + 1,
                names::severity(d.severity),
                d.message
            );
        }
    }
    if !quiet {
        let [e, w, i, h] = counts;
        eprintln!(
            "{} files, {e} errors, {w} warnings, {i} info, {h} hints",
            files.len()
        );
    }
    Ok(if counts[0] + counts[1] > 0 {
        Outcome::Fail
    } else {
        Outcome::Ok
    })
}

fn roots(path: &str) -> Result<Outcome, Error> {
    let ws = Workspace::open_for(Path::new(path), options())?;
    let r = ws.root();
    // Absolute: the root is usually an ancestor of the cwd.
    println!("root: {}", r.path.display());
    println!("mode: {}", names::mode(r.mode));
    println!("why: {}", r.reason);
    println!("files: {}", ws.files().len());
    match ws.cache() {
        Some(db) => println!("cache: {}", db.display()),
        None => println!("cache: memory"),
    }
    let role = match ws.role() {
        None => "none",
        r => role_name(r),
    };
    println!("role: {role}");
    for n in &r.nested_roots {
        println!("nested: {}", n.display());
    }
    Ok(Outcome::Ok)
}

fn resolve(out: &Out, from: &str, link: &str) -> Result<Outcome, Error> {
    let from = Path::new(from);
    let ws = Workspace::open_for(from, options())?;
    let r = ws.resolve(from, link)?;
    for t in &r.targets {
        println!("{}", out.show(t));
    }
    println!("step: {}", names::step(r.step));
    println!("status: {}", names::status(r.status));
    Ok(if r.targets.is_empty() {
        Outcome::Fail
    } else {
        Outcome::Ok
    })
}

fn backlinks(out: &Out, note: &str) -> Result<Outcome, Error> {
    let note = Path::new(note);
    let ws = Workspace::open_for(note, options())?;
    for b in ws.backlinks(note)? {
        println!("{}:{}: {}", out.show(&b.from), b.line + 1, b.from_title);
    }
    Ok(Outcome::Ok)
}

/// `search` arguments: `[--] QUERY [PATH]`; `None` on anything else. After
/// `--` the query may start with `-`.
fn search_args(args: &[String]) -> Option<(&str, &str)> {
    let args = match args.split_first() {
        Some((dd, rest)) if dd == "--" => rest,
        _ if args.first().is_some_and(|a| is_flag(a)) => return None,
        _ => args,
    };
    match args {
        [query] => Some((query, ".")),
        [query, path] if !is_flag(path) => Some((query, path)),
        _ => None,
    }
}

/// Most hits `search` prints.
const SEARCH_LIMIT: usize = 1_000;

fn search(out: &Out, query: &str, path: &str) -> Result<Outcome, Error> {
    let p = canonical(Path::new(path))?;
    let is_dir = std::fs::metadata(&p)
        .map_err(|e| Error::new(ErrorKind::Io, format!("{}: {e}", p.display())))?
        .is_dir();
    // A directory is opened in memory (naive scan); a file through its
    // discovered root and, as reconciler, the DB's full-text index.
    let ws = match is_dir {
        true => Workspace::open_at(&p, options())?,
        false => Workspace::open_for(&p, options())?,
    };
    let hits = ws.full_text(query, SEARCH_LIMIT, &Cancel::new())?;
    let mut stdout = std::io::stdout().lock();
    for h in &hits {
        let _ = writeln!(
            stdout,
            "{}:{}: {}",
            out.show(&h.path),
            h.line + 1,
            h.snippet
        );
    }
    Ok(match hits.is_empty() {
        true => Outcome::Fail,
        false => Outcome::Ok,
    })
}
