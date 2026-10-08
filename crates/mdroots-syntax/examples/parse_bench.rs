//! Parse every markdown and org file under a directory and report throughput.
//!
//! cargo run --release -p mdroots-syntax --example parse_bench -- <dir>
//!
//! Skips hidden entries, `node_modules` and `target`; does not follow
//! symlinks. Only reads files.

use std::path::{Path, PathBuf};
use std::time::Instant;

use mdroots_syntax::{Dialect, ParseOptions, parse_bytes};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            walk(&path, out);
        } else if meta.is_file()
            && matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("md" | "org")
            )
        {
            out.push(path);
        }
    }
}

fn main() {
    let Some(dir) = std::env::args_os().nth(1) else {
        eprintln!("usage: parse_bench <dir>");
        std::process::exit(2);
    };
    let mut files = Vec::new();
    walk(Path::new(&dir), &mut files);

    let start = Instant::now();
    let contents: Vec<(PathBuf, Vec<u8>)> = files
        .into_iter()
        .filter_map(|p| std::fs::read(&p).ok().map(|b| (p, b)))
        .collect();
    let read_ms = start.elapsed().as_secs_f64() * 1e3;

    let bytes: usize = contents.iter().map(|(_, b)| b.len()).sum();
    let start = Instant::now();
    let mut links = 0usize;
    let mut binary = 0usize;
    for (path, b) in &contents {
        match parse_bytes(b, &ParseOptions::new(Dialect::detect_from_path(path))) {
            Some(doc) => links += doc.links().count(),
            None => binary += 1,
        }
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "files {}  bytes {}  read {:.1} ms  parse {:.1} ms  {:.1} MB/s  links {}  binary {}",
        contents.len(),
        bytes,
        read_ms,
        secs * 1e3,
        bytes as f64 / 1e6 / secs.max(1e-9),
        links,
        binary
    );
}
