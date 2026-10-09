//! Whole-root query timings on a synthetic notebook.
//!
//! Generates N notes (default 3,000; first argument overrides) into a temp
//! dir: frontmatter, a few paragraphs of words and five wiki links each
//! (about one in fifty broken). Opens the dir in memory and times
//! diagnostics for every file, backlinks for 100 files and one line/column
//! lookup per file.
//!
//! `cargo run --release -p mdroots --example bench_root [N]`

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use mdroots::syntax::PositionEncoding;
use mdroots::{Cancel, IndexMode, Options, Workspace};

const WORDS: &[&str] = &[
    "alpha", "branch", "cedar", "delta", "ember", "fjord", "garnet", "harbor", "indigo", "juniper",
    "kelp", "lumen", "meadow", "nectar", "orbit", "pebble", "quartz", "river", "summit", "thicket",
];

/// A small deterministic generator (no extra dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn note(i: usize, n: usize, rng: &mut Lcg) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "---\ntitle: Note {i}\ntags: [t{}, t{}]\n---\n",
        i % 17,
        i % 5
    );
    let _ = writeln!(s, "# Note {i}\n");
    for p in 0..4 {
        for _ in 0..60 {
            s.push_str(WORDS[rng.below(WORDS.len())]);
            s.push(' ');
        }
        if p < 3 {
            s.push_str("\n\n");
        }
    }
    s.push('\n');
    for _ in 0..5 {
        match rng.below(50) {
            0 => {
                let _ = writeln!(s, "See [[missing-{}]].", rng.below(n));
            }
            _ => {
                let _ = writeln!(s, "See [[note-{:05}]].", rng.below(n));
            }
        }
    }
    s
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(3_000);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut rng = Lcg(42);
    let t = Instant::now();
    for i in 0..n {
        let path = dir.path().join(format!("note-{i:05}.md"));
        std::fs::write(path, note(i, n, &mut rng)).expect("write note");
    }
    println!("generate {n} notes: {:?}", t.elapsed());

    let t = Instant::now();
    let ws =
        Workspace::open_at(dir.path(), Options::default().index(IndexMode::Memory)).expect("open");
    println!("open: {:?}", t.elapsed());

    let files: Vec<PathBuf> = ws.files();
    let cancel = Cancel::new();
    let t = Instant::now();
    let mut diags = 0usize;
    for f in &files {
        diags += ws.diagnostics(f, &cancel).expect("diagnostics").len();
    }
    println!(
        "diagnostics for {} files: {:?} ({diags} diagnostics)",
        files.len(),
        t.elapsed()
    );

    let t = Instant::now();
    let mut links = 0usize;
    for f in files.iter().take(100) {
        links += ws.backlinks(f).expect("backlinks").len();
    }
    println!(
        "backlinks for 100 files: {:?} ({links} backlinks)",
        t.elapsed()
    );

    // Builds every note's line index, as `mdroots check` and the language
    // server do for the notes they report on.
    let t = Instant::now();
    let mut lines = 0u64;
    for f in &files {
        let (line, _) = ws
            .line_col(f, usize::MAX, PositionEncoding::Utf16)
            .expect("line_col");
        lines += u64::from(line);
    }
    println!(
        "line_col for {} files: {:?} ({lines} lines)",
        files.len(),
        t.elapsed()
    );
}
