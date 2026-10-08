//! End-to-end snapshots of `parse_bytes` over the synthetic vaults in
//! `tests/corpus/` (repo root). One named snapshot per non-hidden `.md` /
//! `.org` file, walked in sorted path order.

use std::fmt::Debug;
use std::path::{Path, PathBuf};

use mdroots_syntax::{
    Anchor, Confidence, Context, Dialect, Document, FrontmatterFormat, LinkKind, ParseOptions,
    TagSyntax, Value, parse_bytes,
};

#[allow(dead_code)] // fields are read through Debug only
#[derive(Debug)]
struct Summary {
    lossy: bool,
    frontmatter: Option<FmSummary>,
    headings: Vec<HeadingSummary>,
    links: Vec<LinkSummary>,
    tags: Vec<TagSummary>,
    link_defs: Vec<(String, String)>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct FmSummary {
    format: FrontmatterFormat,
    error: Option<String>,
    entries: Vec<(String, Value)>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct HeadingSummary {
    level: u8,
    text: String,
    slug: String,
    id: Option<String>,
    custom_id: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug)]
struct LinkSummary {
    kind: LinkKind,
    context: Context,
    confidence: Confidence,
    raw: String,
    path: String,
    anchor: Option<Anchor>,
    label: Option<String>,
    line: Option<u32>,
    group: Option<u32>,
    range: String,
    text_range: String,
    src_line: u32,
}

#[allow(dead_code)]
#[derive(Debug)]
struct TagSummary {
    name: String,
    syntax: TagSyntax,
    range: String,
}

fn summarize(doc: &Document) -> Summary {
    let src = doc.source();
    let slice = |r: &std::ops::Range<usize>| src[r.clone()].to_owned();
    let line_of = |off: usize| src[..off].matches('\n').count() as u32 + 1;
    Summary {
        lossy: doc.is_lossy(),
        frontmatter: doc.frontmatter().map(|f| FmSummary {
            format: f.format,
            error: f.error.clone(),
            entries: f.entries().to_vec(),
        }),
        headings: doc
            .headings()
            .map(|h| HeadingSummary {
                level: h.level,
                text: h.text.clone(),
                slug: h.slug.clone(),
                id: h.id.clone(),
                custom_id: h.custom_id.clone(),
            })
            .collect(),
        links: doc
            .links()
            .map(|l| LinkSummary {
                kind: l.kind,
                context: l.context,
                confidence: l.confidence,
                raw: l.target.raw.clone(),
                path: l.target.path.clone(),
                anchor: l.target.anchor.clone(),
                label: l.label.clone(),
                line: l.target.line,
                group: l.group,
                range: slice(&l.range),
                text_range: slice(&l.text_range),
                src_line: line_of(l.range.start),
            })
            .collect(),
        tags: doc
            .tags()
            .map(|t| TagSummary {
                name: t.name.clone(),
                syntax: t.syntax,
                range: slice(&t.range),
            })
            .collect(),
        link_defs: doc
            .link_defs()
            .map(|d| (d.label.clone(), d.dest.clone()))
            .collect(),
    }
}

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().unwrap().to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let meta = std::fs::symlink_metadata(&p).unwrap();
        if meta.is_dir() {
            walk(&p, out);
        } else if meta.is_file()
            && matches!(p.extension().and_then(|e| e.to_str()), Some("md" | "org"))
        {
            out.push(p);
        }
    }
}

/// Per-snapshot notes for values that deviate from the spec on purpose.
fn description(name: &str) -> &'static str {
    match name {
        "mixed__obsidian__canvas.md" => {
            "%%comment%% stays prose for now (spec 3.1 lists it as comment context for \
             Obsidian roots; that needs the root vote), so [[hidden-link]] is Prose"
        }
        _ => "",
    }
}

fn snapshot<T: Debug>(name: &str, value: &T) {
    insta::with_settings!({ prepend_module_to_snapshot => false, description => description(name) }, {
        insta::assert_debug_snapshot!(name, value);
    });
}

#[test]
fn corpus_files() {
    let root = corpus_root();
    let mut files = Vec::new();
    walk(&root, &mut files);
    assert!(files.len() >= 30, "corpus too small: {}", files.len());
    for path in files {
        let rel = path.strip_prefix(&root).unwrap().to_string_lossy();
        let name = rel.replace('/', "__");
        let bytes = std::fs::read(&path).unwrap();
        let opts = ParseOptions::new(Dialect::detect_from_path(&path));
        let doc = parse_bytes(&bytes, &opts).expect("corpus text file parsed as binary");
        snapshot(&name, &summarize(&doc));
    }
}

#[test]
fn crlf_fixture_is_byte_exact() {
    let bytes = std::fs::read(corpus_root().join("mixed/misc/crlf.md")).unwrap();
    assert!(bytes.windows(2).any(|w| w == b"\r\n"));
}

#[test]
fn gitignored_fixture_exists() {
    assert!(corpus_root().join("zkvault/cache/x.html").is_file());
}

#[test]
fn invalid_utf8_is_lossy() {
    let bytes: &[u8] = b"# Bad \xff\xfe bytes\n\nSee [[note-a]] and caf\xc3 [md](b.md).\n";
    let doc = parse_bytes(bytes, &ParseOptions::new(Dialect::Markdown)).unwrap();
    snapshot("bytes__invalid_utf8", &summarize(&doc));
}

#[test]
fn binary_with_nul_is_none() {
    let mut bytes = b"# Looks like markdown [[a]]\n".to_vec();
    bytes.resize(4096, b'x');
    bytes[2048] = 0;
    let doc = parse_bytes(&bytes, &ParseOptions::new(Dialect::Markdown));
    snapshot("bytes__binary_nul", &doc.map(|d| summarize(&d)));
}
