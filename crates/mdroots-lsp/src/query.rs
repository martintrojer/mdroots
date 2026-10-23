//! `mdroots.query`: the `notes` flags as a JSON object (docs/specs/library.md
//! §3.6), run as a [`NoteQuery`].

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use lsp_types::Uri;
use mdroots::query::{NoteQuery, TagExpr, day_range, parse_date, parse_sort};
use serde_json::{Map, Value};

use crate::features::{Ctx, Fail, graph_fail, note_locations};
use crate::uri;

/// A query as sent, checked for shape but not yet against the notebook.
pub(crate) struct Request {
    q: NoteQuery,
}

fn bad(field: &str, why: impl std::fmt::Display) -> Fail {
    Fail::Params(format!("mdroots.query: {field}: {why}"))
}

fn array<'a>(field: &str, v: &'a Value) -> Result<&'a Vec<Value>, Fail> {
    v.as_array().ok_or_else(|| bad(field, "expected an array"))
}

fn string<'a>(field: &str, v: &'a Value) -> Result<&'a str, Fail> {
    v.as_str().ok_or_else(|| bad(field, "expected a string"))
}

fn boolean(field: &str, v: &Value) -> Result<bool, Fail> {
    v.as_bool().ok_or_else(|| bad(field, "expected a boolean"))
}

/// The paths of an array of `file:` URIs.
fn uris(field: &str, v: &Value) -> Result<Vec<PathBuf>, Fail> {
    array(field, v)?
        .iter()
        .map(|e| {
            let s = string(field, e)?;
            s.parse::<Uri>()
                .ok()
                .and_then(|u| uri::to_path(&u))
                .ok_or_else(|| bad(field, format!("not a file: URI: {s}")))
        })
        .collect()
}

/// `p` with `.` dropped and each `..` removing the component before it
/// (a `..` at the root stays there). Purely lexical.
fn fold_dots(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// The date bounds of one timestamp. The bounds of all its fields hold
/// together, whatever the order of the keys: the latest lower bound and
/// the earliest upper bound win.
#[derive(Default)]
struct Span {
    after: Option<SystemTime>,
    before: Option<SystemTime>,
}

impl Span {
    fn after(&mut self, t: SystemTime) {
        self.after = Some(self.after.map_or(t, |a| a.max(t)));
    }

    fn before(&mut self, t: SystemTime) {
        self.before = Some(self.before.map_or(t, |b| b.min(t)));
    }
}

impl Request {
    /// `None` (or `null`) is the empty query.
    pub(crate) fn parse(arg: Option<&Value>) -> Result<Request, Fail> {
        let empty = Map::new();
        let obj = match arg {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(o)) => o,
            Some(_) => return Err(bad("argument", "expected an object")),
        };
        let now = SystemTime::now();
        let date = |f: &str, v: &Value| -> Result<SystemTime, Fail> {
            parse_date(string(f, v)?, now).map_err(|e| bad(f, e))
        };
        let mut q = NoteQuery::default();
        let (mut created, mut modified) = (Span::default(), Span::default());
        for (field, v) in obj {
            let f = field.as_str();
            match f {
                "paths" => q.paths = uris(f, v)?,
                "exclude" => q.exclude = uris(f, v)?,
                "linkTo" => q.link_to = uris(f, v)?,
                "linkedBy" => q.linked_by = uris(f, v)?,
                "related" => q.related = uris(f, v)?,
                "tag" => {
                    for e in array(f, v)? {
                        q.tag
                            .push(TagExpr::parse(string(f, e)?).map_err(|e| bad(f, e))?);
                    }
                }
                "match" => q.matching = Some(string(f, v)?.to_owned()),
                "tagless" => q.tagless = boolean(f, v)?,
                "orphan" => q.orphan = boolean(f, v)?,
                "missingBacklink" => q.missing_backlink = boolean(f, v)?,
                "created" | "modified" => {
                    let (start, end) = day_range(string(f, v)?, now).map_err(|e| bad(f, e))?;
                    let span = if f == "created" {
                        &mut created
                    } else {
                        &mut modified
                    };
                    span.after(start);
                    span.before(end);
                }
                "createdAfter" => created.after(date(f, v)?),
                "createdBefore" => created.before(date(f, v)?),
                "modifiedAfter" => modified.after(date(f, v)?),
                "modifiedBefore" => modified.before(date(f, v)?),
                "sort" => q.sort = Some(parse_sort(string(f, v)?).map_err(|e| bad(f, e))?),
                "limit" => {
                    let n = v
                        .as_u64()
                        .ok_or_else(|| bad(f, "expected a non-negative integer"))?;
                    q.limit = Some(usize::try_from(n).unwrap_or(usize::MAX));
                }
                _ => return Err(bad(f, "unknown field")),
            }
        }
        (q.created_after, q.created_before) = (created.after, created.before);
        (q.modified_after, q.modified_before) = (modified.after, modified.before);
        Ok(Request { q })
    }
}

/// Runs the query in `c`'s workspace: each note at line 0, its title as
/// message, in the query's sort.
pub(crate) fn run(c: &Ctx, req: Request) -> Result<Value, Fail> {
    let mut q = req.q;
    let ws = c.ws;
    let root = ws.root().path;
    // A path that does not exist has `.` and `..` folded lexically, so
    // `root/../x` does not pass for a path under the root.
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| fold_dots(p));
    for (field, paths) in [("paths", &mut q.paths), ("exclude", &mut q.exclude)] {
        for p in paths.iter_mut() {
            *p = canon(p);
            if !p.starts_with(&root) {
                return Err(bad(
                    field,
                    format!("not under the root: {}", p.to_string_lossy()),
                ));
            }
        }
    }
    for (field, notes) in [
        ("linkTo", &mut q.link_to),
        ("linkedBy", &mut q.linked_by),
        ("related", &mut q.related),
    ] {
        for n in notes.iter_mut() {
            *n = canon(n);
            if ws.text(n).is_err() {
                return Err(bad(field, format!("not a note: {}", n.to_string_lossy())));
            }
        }
    }
    let found = ws.query(&q).map_err(graph_fail)?;
    Ok(note_locations(
        found.into_iter().map(|n| (n.path, 0, Some(n.title))),
    ))
}
