//! Browsing snapshots via `proxmox-backup-client catalog dump`.
//!
//! The dump lists every entry of every archive, one per line:
//! `f "./home.pxar.didx/alice/notes.txt" 17 2026-11-08T19:17:36Z`
//! (type, quoted path, and for files size and modification time). The dump
//! of a snapshot is cached in the root-only runtime directory and scanned
//! for the children of the requested directory.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::restore::UserScope;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DirEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub size_bytes: Option<u64>,
    pub modified: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub kind: char,
    pub path: String,
    pub size: Option<u64>,
    pub modified: Option<String>,
}

/// Parse a quoted string in Rust debug notation (`\"`, `\\`, `\n`, `\t`,
/// `\r`, `\0`, `\xNN`, `\u{NNNN}`); returns (value, rest). Invalid UTF-8
/// byte sequences become U+FFFD.
fn parse_quoted(s: &str) -> Option<(String, &str)> {
    let s = s.strip_prefix('"')?;
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), &s[i + 1..])),
            b'\\' if i + 1 < bytes.len() => match bytes[i + 1] {
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                }
                b'0' => {
                    out.push(0);
                    i += 2;
                }
                b'x' => {
                    let hex = bytes.get(i + 2..i + 4)?;
                    let v = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                    out.push(v);
                    i += 4;
                }
                b'u' => {
                    // \u{XXXX}
                    let rest = bytes.get(i + 2..)?;
                    if rest.first() != Some(&b'{') {
                        return None;
                    }
                    let end = rest.iter().position(|b| *b == b'}')?;
                    let code =
                        u32::from_str_radix(std::str::from_utf8(&rest[1..end]).ok()?, 16).ok()?;
                    let c = char::from_u32(code)?;
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    i += 2 + end + 1;
                }
                other => {
                    out.push(other);
                    i += 2;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    None
}

pub fn parse_line(line: &str) -> Option<Line> {
    let line = line.trim_end();
    let mut chars = line.chars();
    let kind = chars.next()?;
    let rest = chars.as_str().trim_start();
    let (path, rest) = parse_quoted(rest)?;
    let mut fields = rest.split_whitespace();
    let size = fields.next().and_then(|v| v.parse().ok());
    let modified = fields.next().map(String::from);
    Some(Line {
        kind,
        path,
        size,
        modified,
    })
}

fn kind_name(c: char) -> &'static str {
    match c {
        'f' => "file",
        'd' => "directory",
        'l' => "symlink",
        _ => "other",
    }
}

/// Catalog prefixes of an archive (`home.pxar` → `./home.pxar.didx`, and the
/// metadata part of split archives).
fn archive_prefixes(archive: &str) -> Vec<String> {
    let base = archive.strip_suffix(".pxar").unwrap_or(archive);
    vec![
        format!("./{base}.pxar.didx"),
        format!("./{base}.mpxar.didx"),
    ]
}

/// Direct children of `dir` (normalised `/a/b`) in `archive`. For a user
/// scope, ancestors of the home directory only show the branch towards it.
pub fn list_children(
    catalog: &Path,
    archive: &str,
    dir: &str,
    scope: Option<&UserScope>,
) -> Result<Vec<DirEntry>> {
    let file =
        fs::File::open(catalog).with_context(|| format!("cannot read {}", catalog.display()))?;
    let bases: Vec<String> = archive_prefixes(archive)
        .into_iter()
        .map(|p| if dir == "/" { p } else { format!("{p}{dir}") })
        .collect();
    let only_branch = scope
        .filter(|s| !s.contains(dir) && s.is_ancestor(dir))
        .map(|s| {
            // Name of the child of `dir` on the way to the home directory.
            let rest = s
                .prefix
                .strip_prefix(dir)
                .unwrap_or(&s.prefix)
                .trim_start_matches('/');
            rest.split('/').next().unwrap_or("").to_string()
        });
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let Some(entry) = parse_line(&line) else {
            continue;
        };
        let Some(name) = bases.iter().find_map(|b| {
            entry
                .path
                .strip_prefix(b.as_str())
                .and_then(|r| r.strip_prefix('/'))
                .filter(|n| !n.is_empty() && !n.contains('/'))
        }) else {
            continue;
        };
        if let Some(branch) = &only_branch
            && name != branch
        {
            continue;
        }
        out.push(DirEntry {
            name: name.to_string(),
            kind: kind_name(entry.kind),
            size_bytes: if entry.kind == 'f' { entry.size } else { None },
            modified: entry.modified.clone(),
        });
    }
    out.sort_by(|a, b| {
        (a.kind != "directory")
            .cmp(&(b.kind != "directory"))
            .then_with(|| a.name.cmp(&b.name))
    });
    out.dedup_by(|a, b| a.name == b.name);
    Ok(out)
}

/// Cache file for the catalog of `snapshot` inside `cache_dir`.
pub fn cache_path(cache_dir: &Path, snapshot: &str) -> PathBuf {
    let digest = crate::secrets::digest(&crate::secrets::Secret::new(snapshot.as_bytes().to_vec()));
    cache_dir.join(format!("{}.txt", &digest[..32]))
}

/// Keep the newest `keep` files in `dir`, never removing `keep_this`.
pub fn prune_dir(dir: &Path, keep: usize, keep_this: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path() != keep_this && e.path().extension().is_some_and(|x| x == "txt"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort();
    let excess = (files.len() + 1).saturating_sub(keep);
    for (_, path) in files.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"d "./home.pxar.didx"
d "./home.pxar.didx/alice"
f "./home.pxar.didx/alice/.bashrc" 3771 2024-03-31T08:41:03Z
d "./home.pxar.didx/alice/Documents"
f "./home.pxar.didx/alice/Documents/report.txt" 17 2026-11-08T19:17:36Z
f "./home.pxar.didx/alice/Documents/with \"quote\" and\x20space" 5 2026-11-08T19:17:36Z
l "./home.pxar.didx/alice/link"
d "./home.pxar.didx/bob"
f "./home.pxar.didx/bob/secret.txt" 3 2026-11-08T19:17:36Z
d "./etc.pxar.didx"
f "./etc.pxar.didx/hostname" 5 2026-11-08T19:17:36Z
"#;

    fn catalog() -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, DUMP.as_bytes()).unwrap();
        f
    }

    #[test]
    fn parses_lines_with_escapes() {
        let l = parse_line(r#"f "./a/with \"q\"\x20x" 5 2026-11-08T19:17:36Z   "#).unwrap();
        assert_eq!(l.kind, 'f');
        assert_eq!(l.path, "./a/with \"q\" x");
        assert_eq!(l.size, Some(5));
        let d = parse_line(r#"d "./home.pxar.didx"    "#).unwrap();
        assert_eq!(d.size, None);
    }

    #[test]
    fn unicode_escapes_and_bad_input() {
        let l = parse_line(r#"f "./h/Cafe\u{301}" 1 2026-11-08T19:17:36Z"#).unwrap();
        assert_eq!(l.path, "./h/Cafe\u{301}");
        // Non-ASCII after \x must not panic.
        assert!(parse_line("f \"./h/\\xä\" 1").is_none());
    }

    #[test]
    fn lists_children() {
        let f = catalog();
        let root = list_children(f.path(), "home.pxar", "/", None).unwrap();
        let names: Vec<_> = root.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alice", "bob"]);
        let docs = list_children(f.path(), "home.pxar", "/alice/Documents", None).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].size_bytes, Some(17));
        assert!(docs.iter().any(|e| e.name == "with \"quote\" and space"));
        let alice = list_children(f.path(), "home.pxar", "/alice", None).unwrap();
        assert_eq!(alice[0].name, "Documents");
        assert_eq!(alice[0].kind, "directory");
        assert!(alice.iter().any(|e| e.kind == "symlink"));
        let etc = list_children(f.path(), "etc.pxar", "/", None).unwrap();
        assert_eq!(etc.len(), 1);
    }

    #[test]
    fn user_scope_hides_other_homes() {
        let f = catalog();
        let scope = UserScope {
            prefix: "/alice".into(),
        };
        let root = list_children(f.path(), "home.pxar", "/", Some(&scope)).unwrap();
        let names: Vec<_> = root.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alice"]);
        let inside = list_children(f.path(), "home.pxar", "/alice", Some(&scope)).unwrap();
        assert!(inside.len() >= 3);
    }
}
