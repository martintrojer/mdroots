//! `zkdiff <vault> [--json out.json] [--examples N]`: compare mdroots link
//! resolution with zk's `.zk/notebook.db` (opened read-only, immutable).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use mdroots_core::{Cancel, MemStore, StdFs};

const USAGE: &str = "usage: zkdiff <vault> [--json out.json] [--examples N]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zkdiff: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut vault: Option<PathBuf> = None;
    let mut json: Option<PathBuf> = None;
    let mut examples = 5usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--json" => json = Some(args.next().ok_or(USAGE)?.into()),
            "--examples" => {
                examples = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .ok_or("--examples needs a number")?
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ if vault.is_none() && !a.starts_with('-') => vault = Some(a.into()),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let vault = vault.ok_or(USAGE)?;
    let zk = zkdiff::load_zk(&vault).map_err(|e| format!("zk db: {e}"))?;
    let store = MemStore::open(Arc::new(StdFs), vault.clone(), &Cancel::new())
        .map_err(|e| format!("mdroots: {e}"))?;
    let md = zkdiff::md_records(&store);
    let report = zkdiff::diff(&zk, &md);
    let title = format!(
        "{} (zk: {} notes, {} links + {} orphan rows; mdroots: {} files, {} compared links)",
        vault.display(),
        zk.notes.len(),
        zk.rows.len(),
        zk.orphans,
        store.files().count(),
        md.len()
    );
    print!("{}", report.table(&title, examples));
    if let Some(out) = json {
        let meta = serde_json::json!({
            "vault": vault.display().to_string(),
            "zk_notes": zk.notes.len(),
            "zk_link_rows": zk.rows.len(),
            "mdroots_files": store.files().count(),
            "mdroots_compared_links": md.len(),
        });
        let text =
            serde_json::to_string_pretty(&report.to_json(&zk, meta)).map_err(|e| e.to_string())?;
        std::fs::write(&out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    Ok(())
}
