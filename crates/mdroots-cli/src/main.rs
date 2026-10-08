//! `mdroots`: the command line over the `mdroots` facade
//! (docs/specs/library.md §6). Every command is formatting over
//! [`Workspace`]; the CLI holds no logic of its own and reads no notes
//! itself.
#![forbid(unsafe_code)]

mod lsp;
mod names;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use mdroots::syntax::PositionEncoding;
use mdroots::{Cancel, Diagnostic, Error, ErrorKind, Options, Severity, Workspace};

const USAGE: &str = "\
usage: mdroots <command> [args]

commands:
  check [--quiet] [PATH...]   report diagnostics of notes under each PATH
                              (a file or a directory; default .); exit 1 on
                              any error or warning
  roots PATH                  show the root chosen for PATH and why
  resolve FROM LINK           resolve LINK as written in the note FROM
  backlinks NOTE              list the notes linking to NOTE
  lsp [--log FILE]            the language server on stdin/stdout; --log
                              appends one line per message to FILE
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
        ("lsp", []) => lsp::run(None),
        ("lsp", [flag, file]) if flag == "--log" && !is_flag(file) => lsp::run(Some(file)),
        _ => Ok(Outcome::Usage),
    }
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
            let ws = Workspace::open_at(&p, Options::default())?;
            let notes = ws.files();
            (ws, notes)
        } else {
            (Workspace::open_for(&p, Options::default())?, vec![p])
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
    let ws = Workspace::open_for(Path::new(path), Options::default())?;
    let r = ws.root();
    // Absolute: the root is usually an ancestor of the cwd.
    println!("root: {}", r.path.display());
    println!("mode: {}", names::mode(r.mode));
    println!("why: {}", r.reason);
    println!("files: {}", ws.files().len());
    for n in &r.nested_roots {
        println!("nested: {}", n.display());
    }
    Ok(Outcome::Ok)
}

fn resolve(out: &Out, from: &str, link: &str) -> Result<Outcome, Error> {
    let from = Path::new(from);
    let ws = Workspace::open_for(from, Options::default())?;
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
    let ws = Workspace::open_for(note, Options::default())?;
    for b in ws.backlinks(note)? {
        println!("{}:{}: {}", out.show(&b.from), b.line + 1, b.from_title);
    }
    Ok(Outcome::Ok)
}
