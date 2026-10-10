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

use std::time::SystemTime;

use mdroots::query::{NoteQuery, TagExpr, day_range, format_rfc3339, parse_date, parse_sort};
use mdroots::syntax::PositionEncoding;
use mdroots::{
    Cancel, Diagnostic, DialectMarker, Error, ErrorKind, NoteSummary, Options, Role, Severity,
    Source, Workspace, names,
};

const USAGE: &str = "\
usage: mdroots <command> [args]

options:
  -V, --version               print the version and exit
  -h, --help                  print this usage

commands:
  notes [FLAG...] [PATH...]   list the notes matching every FLAG, as zk
                              list does, in the notebook of the first PATH
                              (default .); PATHs keep the notes under them
    -t, --tag EXPR            tags: `a, b` / `a AND b` and, `a OR b` /
                              `a|b` or, `NOT a` / `-a` not, `( )`
                              groups, `*` and `?` globs (repeatable: and)
    --tagless                 notes without tags
    -m, --match QUERY         notes containing every word of QUERY
    -x, --exclude PATH        not under PATH (repeatable)
    --created DATE, --modified DATE
                              on that day (UTC)
    --created-after DATE, --created-before DATE,
    --modified-after DATE, --modified-before DATE
                              after (inclusive) / before (exclusive) DATE:
                              YYYY-MM-DD, RFC 3339, today, yesterday,
                              `2 weeks ago`, `last monday`, 7d, ...
    --orphan                  notes no other note links to
    --missing-backlink        notes linked from a note they don't link to
    -l, --link-to NOTE        notes linking to NOTE (repeatable: and)
    -L, --linked-by NOTE      notes NOTE links to (repeatable: and)
    --related NOTE            notes sharing a linked note with NOTE but
                              not linked with it (repeatable: and)
    -s, --sort KEY[+|-]       title, path, created or modified (t, p, c,
                              m); + ascending, - descending (default
                              title+)
    -n, --limit N             at most N notes
    -f, --format FORMAT       path (default), tsv (path, title, tags,
                              modified, created), json or jsonl
    -0, --delimiter0          end records with NUL, not a line break
  tags [--sort name|count] [--format tsv|json] [PATH]
                              each tag with its note count, in the
                              notebook of PATH (default .), under PATH
                              when given
  check [--quiet] [--fail-on error|warning|never] [PATH...]
                              report diagnostics of notes under each PATH
                              (a file or a directory; default .); exit 1
                              on any error or warning (--fail-on error:
                              errors only; never: exit 0)
  roots PATH                  show the root chosen for PATH, why, and its
                              settings with where each came from
  resolve FROM LINK           resolve LINK as written in the note FROM
  backlinks NOTE              list the notes linking to NOTE
  search [--paths] [--] QUERY [PATH]
                              notes containing every word of QUERY (the
                              last also as a prefix): a file searches its
                              root, a directory (default .) is scanned in
                              memory; --paths prints each note's path
                              only; exit 1 without a hit
  lsp [--stdio] [--log FILE]  the language server on stdin/stdout (--stdio
                              is accepted and ignored); --log appends one
                              line per message to FILE

notes and tags exit 0 with or without results; 2 on a usage error.

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
    if matches!(args.first().map(String::as_str), Some("--version" | "-V")) {
        println!("mdroots {}", env!("CARGO_PKG_VERSION"));
        return Ok(Outcome::Ok);
    }
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
        ("notes", rest) => notes(&out, rest),
        ("tags", rest) => tags(rest),
        ("roots", [path]) if !is_flag(path) => roots(path),
        ("resolve", [from, link]) if !is_flag(from) => resolve(&out, from, link),
        ("backlinks", [note]) if !is_flag(note) => backlinks(&out, note),
        ("search", rest) => match search_args(rest) {
            Some((query, path, paths)) => search(&out, query, path, paths),
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

/// Every flag `mdroots notes` takes.
const NOTES_FLAGS: &[&str] = &[
    "--tagless",
    "--orphan",
    "--missing-backlink",
    "-0",
    "--delimiter0",
    "-t",
    "--tag",
    "-m",
    "--match",
    "-x",
    "--exclude",
    "--created",
    "--modified",
    "--created-after",
    "--created-before",
    "--modified-after",
    "--modified-before",
    "-l",
    "--link-to",
    "-L",
    "--linked-by",
    "--related",
    "-s",
    "--sort",
    "-n",
    "--limit",
    "-f",
    "--format",
];

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
    let mut fail_on = FailOn::Warning;
    let mut paths = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--quiet" => quiet = true,
            "--fail-on" => {
                fail_on = match it.next().map(String::as_str) {
                    Some("error") => FailOn::Error,
                    Some("warning") => FailOn::Warning,
                    Some("never") => FailOn::Never,
                    _ => return Ok(Outcome::Usage),
                }
            }
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
    let failing = match fail_on {
        FailOn::Error => counts[0],
        FailOn::Warning => counts[0] + counts[1],
        FailOn::Never => 0,
    };
    Ok(if failing > 0 {
        Outcome::Fail
    } else {
        Outcome::Ok
    })
}

/// `check --fail-on`: the least severe diagnostic that makes it exit 1.
enum FailOn {
    Error,
    Warning,
    Never,
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
    for s in ws.settings() {
        println!("{}: {} ({})", s.name, s.value, source_name(&s.source));
    }
    Ok(Outcome::Ok)
}

/// Where a setting came from, as `roots` prints it: `zk .zk/config.toml
/// link-format`, `zk default`, `vote` or `default`.
fn source_name(s: &Source) -> String {
    match s {
        Source::Config { tool, file, key } => format!("{tool} {file} {key}"),
        Source::Marker(m) => format!("{} default", marker_name(*m)),
        Source::Vote => "vote".to_owned(),
        Source::Default => "default".to_owned(),
        _ => "unknown".to_owned(),
    }
}

fn marker_name(m: DialectMarker) -> &'static str {
    match m {
        DialectMarker::Zk => "zk",
        DialectMarker::Obsidian => "obsidian",
        DialectMarker::Marksman => "marksman",
        DialectMarker::Foam => "foam",
        DialectMarker::Dendron => "dendron",
        DialectMarker::Logseq => "logseq",
        DialectMarker::OrgRoam => "org-roam",
        DialectMarker::Gollum => "gollum",
        DialectMarker::Mkdocs => "mkdocs",
        DialectMarker::Docusaurus => "docusaurus",
        DialectMarker::Hugo => "hugo",
        DialectMarker::Jekyll => "jekyll",
        DialectMarker::MdBook => "mdbook",
        DialectMarker::Zettlr => "zettlr",
        _ => "unknown",
    }
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

/// `search` arguments: `[--paths] [--] QUERY [PATH]` as (query, path,
/// paths only); `None` on anything else. After `--` the query may start
/// with `-`.
fn search_args(args: &[String]) -> Option<(&str, &str, bool)> {
    let (paths, args) = match args.split_first() {
        Some((p, rest)) if p == "--paths" => (true, rest),
        _ => (false, args),
    };
    let args = match args.split_first() {
        Some((dd, rest)) if dd == "--" => rest,
        _ if args.first().is_some_and(|a| is_flag(a)) => return None,
        _ => args,
    };
    match args {
        [query] => Some((query, ".", paths)),
        [query, path] if !is_flag(path) => Some((query, path, paths)),
        _ => None,
    }
}

/// Most hits `search` prints.
const SEARCH_LIMIT: usize = 1_000;

fn search(out: &Out, query: &str, path: &str, paths_only: bool) -> Result<Outcome, Error> {
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
        if paths_only {
            let _ = writeln!(stdout, "{}", out.show(&h.path));
            continue;
        }
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

/// The notebook of `path` (a note or a directory, canonical): a directory
/// is opened with `open_dir`, so a subdirectory finds its notebook's root
/// and the whole notebook is indexed (orphans and related notes need it).
fn open_notebook(path: &Path) -> Result<Workspace, Error> {
    let is_dir = std::fs::metadata(path)
        .map_err(|e| Error::new(ErrorKind::Io, format!("{}: {e}", path.display())))?
        .is_dir();
    match is_dir {
        true => Workspace::open_dir(path, options()),
        false => Workspace::open_for(path, options()),
    }
}

/// The workspace for the PATH arguments of `notes` and `tags`: the
/// notebook found from the first PATH (or the cwd), and the PATHs as
/// canonical filters. Without PATHs the whole notebook counts, as in zk.
fn notebook_for(paths: &[PathBuf]) -> Result<(Workspace, Vec<PathBuf>), Error> {
    let canon = paths
        .iter()
        .map(|p| canonical(p))
        .collect::<Result<Vec<_>, _>>()?;
    let ws = open_notebook(&canon.first().cloned().unwrap_or(canonical(Path::new("."))?))?;
    let root = ws.root().path;
    if let Some(p) = canon.iter().find(|p| !p.starts_with(&root)) {
        return Err(Error::new(
            ErrorKind::Unsupported,
            format!("{} is outside the notebook {}", p.display(), root.display()),
        ));
    }
    Ok((ws, canon))
}

/// Output of `notes --format`.
#[derive(Clone, Copy, PartialEq)]
enum Format {
    Path,
    Tsv,
    Json,
    Jsonl,
}

/// A bad flag value: `mdroots: FLAG: why`, exit 2.
fn bad(flag: &str, why: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Unsupported, format!("{flag}: {why}"))
}

fn notes(out: &Out, args: &[String]) -> Result<Outcome, Error> {
    let now = SystemTime::now();
    let mut q = NoteQuery::default();
    let mut format = Format::Path;
    let mut nul = false;
    let mut paths = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let flag = a.as_str();
        // Every flag but the booleans takes the next argument.
        // One of this command's flags where the value belongs is a missing
        // value (`-t --orphan`); other dashes are values (`-t -draft`).
        let mut value = || {
            it.next()
                .filter(|v| !NOTES_FLAGS.contains(&v.as_str()))
                .ok_or(())
        };
        let date = |v: &str| parse_date(v, now).map_err(|e| bad(flag, e));
        let day = |v: &str| day_range(v, now).map_err(|e| bad(flag, e));
        match flag {
            "--tagless" => q.tagless = true,
            "--orphan" => q.orphan = true,
            "--missing-backlink" => q.missing_backlink = true,
            "-0" | "--delimiter0" => nul = true,
            "-t" | "--tag" | "-m" | "--match" | "-x" | "--exclude" | "--created" | "--modified"
            | "--created-after" | "--created-before" | "--modified-after" | "--modified-before"
            | "-l" | "--link-to" | "-L" | "--linked-by" | "--related" | "-s" | "--sort" | "-n"
            | "--limit" | "-f" | "--format" => {
                let Ok(v) = value() else {
                    return Ok(Outcome::Usage);
                };
                match flag {
                    "-t" | "--tag" => q.tag.push(TagExpr::parse(v).map_err(|e| bad(flag, e))?),
                    "-m" | "--match" => q.matching = Some(v.clone()),
                    "-x" | "--exclude" => q.exclude.push(PathBuf::from(v)),
                    "--created" => (q.created_after, q.created_before) = day(v).map(split)?,
                    "--modified" => (q.modified_after, q.modified_before) = day(v).map(split)?,
                    "--created-after" => q.created_after = Some(date(v)?),
                    "--created-before" => q.created_before = Some(date(v)?),
                    "--modified-after" => q.modified_after = Some(date(v)?),
                    "--modified-before" => q.modified_before = Some(date(v)?),
                    "-l" | "--link-to" => q.link_to.push(PathBuf::from(v)),
                    "-L" | "--linked-by" => q.linked_by.push(PathBuf::from(v)),
                    "--related" => q.related.push(PathBuf::from(v)),
                    "-s" | "--sort" => q.sort = Some(parse_sort(v).map_err(|e| bad(flag, e))?),
                    "-n" | "--limit" => {
                        q.limit = Some(v.parse().map_err(|_| bad(flag, "not a number"))?)
                    }
                    _ => {
                        format = match v.as_str() {
                            "path" => Format::Path,
                            "tsv" => Format::Tsv,
                            "json" => Format::Json,
                            "jsonl" => Format::Jsonl,
                            _ => return Ok(Outcome::Usage),
                        }
                    }
                }
            }
            a if is_flag(a) => return Ok(Outcome::Usage),
            a => paths.push(PathBuf::from(a)),
        }
    }
    let (ws, paths) = notebook_for(&paths)?;
    q.paths = paths;
    let found = ws.query(&q)?;

    let end = if nul { '\0' } else { '\n' };
    let mut s = String::new();
    match format {
        Format::Path => {
            for n in &found {
                s.push_str(&out.show(&n.path));
                s.push(end);
            }
        }
        Format::Tsv => {
            for n in &found {
                let fields = [
                    out.show(&n.path),
                    n.title.clone(),
                    n.tags.join(","),
                    n.modified.map(format_rfc3339).unwrap_or_default(),
                    n.created.map(format_rfc3339).unwrap_or_default(),
                ];
                let fields: Vec<String> = fields.iter().map(|f| tsv_escape(f)).collect();
                s.push_str(&fields.join("\t"));
                s.push(end);
            }
        }
        Format::Json => {
            let rows: Vec<String> = found.iter().map(note_json).collect();
            s.push('[');
            s.push_str(&rows.join(","));
            s.push(']');
            s.push(end);
        }
        Format::Jsonl => {
            for n in &found {
                s.push_str(&note_json(n));
                s.push(end);
            }
        }
    }
    let _ = std::io::stdout().lock().write_all(s.as_bytes());
    Ok(Outcome::Ok)
}

fn split((a, b): (SystemTime, SystemTime)) -> (Option<SystemTime>, Option<SystemTime>) {
    (Some(a), Some(b))
}

fn tags(args: &[String]) -> Result<Outcome, Error> {
    let (mut by_count, mut json) = (false, false);
    let mut paths = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match (a.as_str(), it.as_slice().first().map(String::as_str)) {
            ("--sort", Some(v @ ("name" | "count"))) => {
                by_count = v == "count";
                it.next();
            }
            ("--format", Some(v @ ("tsv" | "json"))) => {
                json = v == "json";
                it.next();
            }
            (a, _) if is_flag(a) => return Ok(Outcome::Usage),
            (a, _) if paths.is_empty() => paths.push(PathBuf::from(a)),
            _ => return Ok(Outcome::Usage),
        }
    }
    let (ws, paths) = notebook_for(&paths)?;
    let mut tags = ws.tags_under(&paths);
    if by_count {
        // Stable: equal counts keep the name order.
        tags.sort_by_key(|t| std::cmp::Reverse(t.1));
    }
    let mut s = String::new();
    if json {
        let rows: Vec<String> = tags
            .iter()
            .map(|(t, n)| format!("{{\"name\":{},\"count\":{n}}}", json_str(t)))
            .collect();
        s.push('[');
        s.push_str(&rows.join(","));
        s.push_str("]\n");
    } else {
        for (t, n) in &tags {
            s.push_str(&format!("{}\t{n}\n", tsv_escape(t)));
        }
    }
    let _ = std::io::stdout().lock().write_all(s.as_bytes());
    Ok(Outcome::Ok)
}

/// A TSV field: backslash, tab, line feed and carriage return escaped as
/// `\\`, `\t`, `\n` and `\r`.
fn tsv_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// One note as a JSON object: absolute path, title, tags, and the times as
/// RFC 3339 UTC strings or null.
fn note_json(n: &NoteSummary) -> String {
    let time =
        |t: Option<SystemTime>| t.map_or("null".to_owned(), |t| json_str(&format_rfc3339(t)));
    let tags: Vec<String> = n.tags.iter().map(|t| json_str(t)).collect();
    format!(
        "{{\"path\":{},\"title\":{},\"tags\":[{}],\"modified\":{},\"created\":{}}}",
        json_str(&n.path.display().to_string()),
        json_str(&n.title),
        tags.join(","),
        time(n.modified),
        time(n.created),
    )
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
