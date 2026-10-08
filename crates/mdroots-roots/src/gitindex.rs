//! In-process reader for the [git](https://git-scm.com) index
//! (`.git/index`), so index-driven and tracked-only roots can list tracked
//! markdown without `read_dir` (docs/specs/roots.md §1 stage 3).
//!
//! A small hand parser of the documented format:
//! <https://git-scm.com/docs/index-format>. It handles versions 2, 3 and 4
//! with SHA-1 object names only; a SHA-256 repository is rejected with
//! `InvalidData`. The trailing checksum is not verified: every read is
//! bounds-checked instead, so a corrupt index yields `InvalidData` or a
//! partial result, never a panic.
//!
//! The index's stat data is a snapshot taken by the last git command.
//! Callers must `stat` each listed file before trusting that it exists or
//! is unchanged.

use std::io;
use std::path::Path;

use crate::probe::Probe;

/// The 12-byte index header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexHeader {
    pub version: u32,
    /// Entry count as written by git. Unreliable for sparse or split
    /// indexes (see [`IndexScan`]).
    pub entries: u32,
}

/// Result of [`scan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexScan {
    pub version: u32,
    /// Entry count from the header.
    pub entries: u32,
    /// Kept paths among the first `cap` entries, in index order (sorted by
    /// git). Sparse directory entries and empty names are never included.
    pub paths: Vec<String>,
    /// The `sdir` extension is present: some directories are collapsed into
    /// single entries, so `entries` and `paths` do not cover the whole tree.
    pub sparse: bool,
    /// The `link` extension is present (split index): most entries live in
    /// a shared index file that is not read, so `entries` and `paths` are
    /// incomplete.
    pub split: bool,
    /// More than `cap` entries exist; `paths` covers only the first `cap`.
    pub truncated: bool,
}

/// Largest index [`scan`] reads; a bigger one means the repository is
/// huge (spec: "> 32 MB → lazy").
pub const MAX_INDEX_BYTES: usize = 32 << 20;

const HEADER_LEN: usize = 12;
/// ctime, mtime (8 each), dev, ino, mode, uid, gid, size (4 each), SHA-1.
const OID_LEN: usize = 20;
const FIXED_LEN: usize = 40 + OID_LEN + 2;
const MODE_OFFSET: usize = 24;
const FLAG_EXTENDED: u16 = 0x4000;
const NAME_MASK: u16 = 0x0fff;
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("git index: {msg}"))
}

fn parse_header(b: &[u8]) -> io::Result<IndexHeader> {
    if b.len() < HEADER_LEN || &b[..4] != b"DIRC" {
        return Err(invalid("missing DIRC header"));
    }
    let version = be32(b, 4)?;
    if !(2..=4).contains(&version) {
        return Err(invalid("unsupported version"));
    }
    Ok(IndexHeader {
        version,
        entries: be32(b, 8)?,
    })
}

/// Read the header only (the first 12 bytes, via [`Probe::read_prefix`]).
pub fn read_header(probe: &dyn Probe, index_path: &Path) -> io::Result<IndexHeader> {
    parse_header(&probe.read_prefix(index_path, HEADER_LEN)?)
}

/// Parse the whole index (at most [`MAX_INDEX_BYTES`], else `InvalidData`)
/// and collect the paths among the first `cap` entries for which `keep`
/// returns true.
pub fn scan(
    probe: &dyn Probe,
    index_path: &Path,
    keep: &dyn Fn(&str) -> bool,
    cap: usize,
) -> io::Result<IndexScan> {
    if let Some(dir) = index_path.parent()
        && is_sha256_repo(probe, &dir.join("config"))
    {
        return Err(invalid("sha256 index not supported"));
    }
    let data = probe.read_small(index_path, MAX_INDEX_BYTES)?;
    parse(&data, keep, cap)
}

/// Whether the git config at `config` sets `extensions.objectformat` to
/// sha256. A missing or unreadable config means SHA-1.
fn is_sha256_repo(probe: &dyn Probe, config: &Path) -> bool {
    let Ok(text) = probe.read_small(config, 1 << 20) else {
        return false;
    };
    let mut in_extensions = false;
    for line in String::from_utf8_lossy(&text).lines() {
        let line = line.trim();
        if line.starts_with('[') {
            let name = line.trim_start_matches('[').trim_end_matches(']').trim();
            in_extensions = name.eq_ignore_ascii_case("extensions");
            continue;
        }
        if !in_extensions {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case("objectformat")
            && v.trim().trim_matches('"').eq_ignore_ascii_case("sha256")
        {
            return true;
        }
    }
    false
}

fn be32(b: &[u8], at: usize) -> io::Result<u32> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| invalid("truncated"))
}

fn be16(b: &[u8], at: usize) -> io::Result<u16> {
    b.get(at..at + 2)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
        .ok_or_else(|| invalid("truncated"))
}

/// Index of the first NUL at or after `from`.
fn nul_from(b: &[u8], from: usize) -> io::Result<usize> {
    b.get(from..)
        .and_then(|s| s.iter().position(|&c| c == 0))
        .map(|i| from + i)
        .ok_or_else(|| invalid("unterminated path"))
}

/// The offset varint used by index v4 path compression.
fn varint(b: &[u8], mut at: usize) -> io::Result<(usize, usize)> {
    let mut byte = *b.get(at).ok_or_else(|| invalid("truncated varint"))?;
    at += 1;
    let mut val = usize::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        byte = *b.get(at).ok_or_else(|| invalid("truncated varint"))?;
        at += 1;
        val = val
            .checked_add(1)
            .and_then(|v| v.checked_mul(128))
            .ok_or_else(|| invalid("varint overflow"))?
            | usize::from(byte & 0x7f);
    }
    Ok((val, at))
}

fn parse(b: &[u8], keep: &dyn Fn(&str) -> bool, cap: usize) -> io::Result<IndexScan> {
    let h = parse_header(b)?;
    // Extensions and entries both end before the trailing checksum.
    let end = b
        .len()
        .checked_sub(OID_LEN)
        .filter(|&e| e >= HEADER_LEN)
        .ok_or_else(|| invalid("truncated"))?;
    let body = &b[..end];
    let mut out = IndexScan {
        version: h.version,
        entries: h.entries,
        paths: Vec::new(),
        sparse: false,
        split: false,
        truncated: false,
    };
    let mut at = HEADER_LEN;
    // Previous path, for v4 prefix compression.
    let mut prev: Vec<u8> = Vec::new();
    for i in 0..h.entries as usize {
        let mode = be32(body, at + MODE_OFFSET)?;
        let flags = be16(body, at + FIXED_LEN - 2)?;
        let mut name_at = at + FIXED_LEN;
        if flags & FLAG_EXTENDED != 0 {
            if h.version < 3 {
                return Err(invalid("extended flag in a version 2 index"));
            }
            name_at += 2;
        }
        let name_len = usize::from(flags & NAME_MASK);
        let path: &[u8];
        if h.version == 4 {
            let (strip, suffix_at) = varint(body, name_at)?;
            let nul = nul_from(body, suffix_at)?;
            let kept = prev
                .len()
                .checked_sub(strip)
                .ok_or_else(|| invalid("bad prefix length"))?;
            prev.truncate(kept);
            prev.extend_from_slice(&body[suffix_at..nul]);
            at = nul + 1;
            path = &prev;
        } else {
            let nul = if name_len == usize::from(NAME_MASK) {
                nul_from(body, name_at)?
            } else {
                name_at + name_len
            };
            if body.get(nul) != Some(&0) {
                return Err(invalid("unterminated path"));
            }
            path = &body[name_at..nul];
            // 1-8 NULs pad the entry to a multiple of 8 bytes.
            at += (nul - at + 8) & !7;
        }
        if at > body.len() {
            return Err(invalid("truncated entry"));
        }
        if i >= cap {
            // Keep parsing (cheap, already in memory) to reach extensions.
            out.truncated = true;
            continue;
        }
        if path.is_empty() || mode & S_IFMT == S_IFDIR {
            continue;
        }
        // Non-UTF-8 paths cannot be represented in the String API; skip.
        if let Ok(p) = std::str::from_utf8(path)
            && keep(p)
        {
            out.paths.push(p.to_owned());
        }
    }
    while at < body.len() {
        let sig = body.get(at..at + 4).ok_or_else(|| invalid("truncated"))?;
        let size = be32(body, at + 4)? as usize;
        match sig {
            b"sdir" => out.sparse = true,
            b"link" => out.split = true,
            _ => {}
        }
        at = at
            .checked_add(8)
            .and_then(|a| a.checked_add(size))
            .filter(|&a| a <= body.len())
            .ok_or_else(|| invalid("truncated extension"))?;
    }
    Ok(out)
}
