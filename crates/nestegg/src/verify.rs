//! Read a snapshot back after the upload (decision 0015).
//!
//! Success of `proxmox-backup-client backup` only means the client exited
//! with 0. Before a run counts as succeeded, the snapshot is checked on the
//! server: every uploaded archive is listed, the metadata blob restores
//! with the configured key and matches what was uploaded byte for byte,
//! and the inventory archive (if any) restores and matches the local
//! inventory. Data archives are not restored (too expensive); the server's
//! own verification jobs cover their chunks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::client::{Client, RestoreKey, RestoreRequest, SnapshotInfo};

/// What the run uploaded.
pub struct Expected<'a> {
    pub snapshot: &'a str,
    /// Archive names as in the backup spec (`home.pxar`, `inventory.pxar`).
    pub archives: Vec<String>,
    /// The archives were uploaded with `--crypt-mode encrypt`.
    pub encrypted: bool,
    /// Exact bytes of the uploaded metadata file.
    pub metadata: &'a [u8],
    /// Local inventory directory, if `inventory.pxar` was uploaded.
    pub inventory: Option<&'a Path>,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Verified,
    /// What did not match, or why reading it back failed.
    Failed(String),
    Cancelled,
}

/// Problems of a `snapshot files` listing: archives that are missing or
/// were not stored encrypted although the run asked for it.
pub fn listing_problems(files_json: &str, archives: &[String], encrypted: bool) -> Vec<String> {
    let mut problems = Vec::new();
    for archive in archives {
        match crate::client::crypt_mode_for_archive(files_json, archive) {
            None => {
                if crate::client::size_for_archive(files_json, archive).is_some() {
                    // Listed, but the listing has no crypt mode (older server).
                    continue;
                }
                problems.push(format!("{archive} is missing"));
            }
            Some(mode) if encrypted && mode != "encrypt" => {
                problems.push(format!("{archive} is not encrypted (crypt mode {mode})"));
            }
            Some(_) => {}
        }
    }
    problems
}

/// Regular files below `dir`, relative path → content.
fn read_tree(dir: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)
            .with_context(|| format!("cannot read {}", current.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let rel = path.strip_prefix(dir).unwrap_or(&path).to_path_buf();
                let bytes = std::fs::read(&path)
                    .with_context(|| format!("cannot read {}", path.display()))?;
                files.insert(rel, bytes);
            }
        }
    }
    Ok(files)
}

/// File that `proxmox-backup-client` adds to the root of every archive
/// made with `--exclude` (the exclude patterns of the run).
const CLIENT_EXCLUDE_FILE: &str = ".pxarexclude-cli";

/// Differences between the local inventory and its restored copy.
pub fn compare_trees(local: &Path, restored: &Path) -> Result<Vec<String>> {
    let local = read_tree(local)?;
    let mut restored = read_tree(restored)?;
    if !local.contains_key(Path::new(CLIENT_EXCLUDE_FILE)) {
        restored.remove(Path::new(CLIENT_EXCLUDE_FILE));
    }
    let mut problems = Vec::new();
    for (path, bytes) in &local {
        match restored.get(path) {
            None => problems.push(format!("{} is missing", path.display())),
            Some(other) if other != bytes => {
                problems.push(format!("{} has different content", path.display()))
            }
            Some(_) => {}
        }
    }
    for path in restored.keys().filter(|p| !local.contains_key(*p)) {
        problems.push(format!("{} was not uploaded", path.display()));
    }
    Ok(problems)
}

/// Snapshots the server's own verification marked as failed.
pub fn failed_snapshots(list: &[SnapshotInfo]) -> Vec<String> {
    list.iter()
        .filter(|s| s.verified.as_deref() == Some("failed"))
        .map(|s| s.snapshot.clone())
        .collect()
}

/// Read `expected.snapshot` back from the server. `work` is a private,
/// root-only directory that the caller removes afterwards. `cancelled`
/// is polled between the steps.
pub fn read_back(
    client: &Client,
    expected: &Expected<'_>,
    work: &Path,
    log: &mut dyn FnMut(&str),
    cancelled: &dyn Fn() -> bool,
) -> Outcome {
    match read_back_steps(client, expected, work, log, cancelled) {
        Ok(outcome) => outcome,
        // A client stopped by a cancel request fails; report the cancel.
        Err(_) if cancelled() => Outcome::Cancelled,
        Err(e) => Outcome::Failed(format!(
            "the snapshot {} could not be read back from the server: {e:#}",
            expected.snapshot
        )),
    }
}

fn read_back_steps(
    client: &Client,
    expected: &Expected<'_>,
    work: &Path,
    log: &mut dyn FnMut(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<Outcome> {
    let mismatch = |what: String| {
        Outcome::Failed(format!(
            "the snapshot {} on the server does not match what was uploaded: {what}",
            expected.snapshot
        ))
    };
    // 1. Every archive is listed.
    let files = client.snapshot_files(expected.snapshot)?;
    let mut archives = expected.archives.clone();
    archives.push(crate::metadata::ARCHIVE.to_string());
    let problems = listing_problems(&files, &archives, expected.encrypted);
    if !problems.is_empty() {
        return Ok(mismatch(problems.join("; ")));
    }
    log(&format!("read-back: {} archives listed", archives.len()));
    if cancelled() {
        return Ok(Outcome::Cancelled);
    }

    // 2. The metadata blob restores with the configured key, byte for byte.
    let Some(fetched) = crate::metadata::fetch(client, expected.snapshot, work)? else {
        return Ok(mismatch(format!("{} is missing", crate::metadata::ARCHIVE)));
    };
    if fetched.bytes != expected.metadata {
        return Ok(mismatch(format!(
            "{} has different content ({} bytes instead of {})",
            crate::metadata::ARCHIVE,
            fetched.bytes.len(),
            expected.metadata.len()
        )));
    }
    log(&format!(
        "read-back: {} restored and identical",
        crate::metadata::ARCHIVE
    ));
    if cancelled() {
        return Ok(Outcome::Cancelled);
    }

    // 3. The inventory restores and matches the local copy.
    if let Some(local) = expected.inventory {
        let target = work.join("inventory");
        let mut client_log = Vec::new();
        client
            .restore(
                &RestoreRequest {
                    snapshot: expected.snapshot,
                    archive: "inventory.pxar",
                    target: &target,
                    patterns: &[],
                    key: RestoreKey::Auto,
                },
                &mut client_log,
            )
            .context("restoring inventory.pxar failed")?;
        let problems = compare_trees(local, &target)?;
        if !problems.is_empty() {
            return Ok(mismatch(format!("inventory.pxar: {}", problems.join("; "))));
        }
        log("read-back: inventory.pxar restored and identical");
    }
    if cancelled() {
        return Ok(Outcome::Cancelled);
    }
    Ok(Outcome::Verified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_detects_missing_and_unencrypted_archives() {
        let json = r#"[
          {"filename":"home.pxar.didx","crypt-mode":"encrypt","size":10},
          {"filename":"data.mpxar.didx","crypt-mode":"encrypt","size":1},
          {"filename":"data.ppxar.didx","crypt-mode":"encrypt","size":9},
          {"filename":"etc.pxar.didx","crypt-mode":"none","size":10},
          {"filename":"nestegg-meta.conf.blob","crypt-mode":"encrypt","size":300},
          {"filename":"index.json.blob","crypt-mode":"sign-only","size":1}
        ]"#;
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(listing_problems(json, &names(&["home.pxar", "data.pxar"]), true).is_empty());
        assert!(listing_problems(json, &names(&["nestegg-meta.conf"]), true).is_empty());
        assert_eq!(
            listing_problems(json, &names(&["home.pxar", "inventory.pxar"]), true),
            vec!["inventory.pxar is missing".to_string()]
        );
        assert_eq!(
            listing_problems(json, &names(&["etc.pxar"]), true),
            vec!["etc.pxar is not encrypted (crypt mode none)".to_string()]
        );
        assert!(listing_problems(json, &names(&["etc.pxar"]), false).is_empty());
        // Listing without crypt modes: presence is enough.
        assert!(
            listing_problems(
                r#"[{"filename":"home.pxar.didx","size":1}]"#,
                &names(&["home.pxar"]),
                true
            )
            .is_empty()
        );
    }

    #[test]
    fn trees_are_compared_file_by_file() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for dir in [a.path(), b.path()] {
            std::fs::create_dir(dir.join("sub")).unwrap();
            std::fs::write(dir.join("layout.json"), "{}").unwrap();
            std::fs::write(dir.join("sub/x.txt"), "x").unwrap();
        }
        assert!(compare_trees(a.path(), b.path()).unwrap().is_empty());
        // Added by the client to every archive made with excludes.
        std::fs::write(b.path().join(".pxarexclude-cli"), "**/.cache").unwrap();
        assert!(compare_trees(a.path(), b.path()).unwrap().is_empty());
        std::fs::write(b.path().join("sub/.pxarexclude-cli"), "x").unwrap();
        assert_eq!(
            compare_trees(a.path(), b.path()).unwrap(),
            vec!["sub/.pxarexclude-cli was not uploaded".to_string()]
        );
        std::fs::remove_file(b.path().join("sub/.pxarexclude-cli")).unwrap();
        std::fs::write(b.path().join("sub/x.txt"), "y").unwrap();
        std::fs::remove_file(b.path().join("layout.json")).unwrap();
        std::fs::write(b.path().join("extra.txt"), "e").unwrap();
        let problems = compare_trees(a.path(), b.path()).unwrap();
        assert_eq!(
            problems,
            vec![
                "layout.json is missing".to_string(),
                "sub/x.txt has different content".to_string(),
                "extra.txt was not uploaded".to_string(),
            ]
        );
        assert!(compare_trees(a.path(), &a.path().join("nope")).is_err());
    }

    #[test]
    fn failed_server_verification_is_collected() {
        let json = r#"[
          {"backup-type":"host","backup-id":"desktop","backup-time":1794000000,"files":[],
           "verification":{"state":"failed","upid":"x"}},
          {"backup-type":"host","backup-id":"desktop","backup-time":1793000000,"files":[],
           "verification":{"state":"ok","upid":"y"}},
          {"backup-type":"host","backup-id":"desktop","backup-time":1792000000,"files":[]}
        ]"#;
        let list = crate::client::parse_snapshot_list(json).unwrap();
        assert_eq!(
            failed_snapshots(&list),
            vec!["host/desktop/2026-11-06T21:20:00Z".to_string()]
        );
        assert!(failed_snapshots(&list[1..]).is_empty());
    }
}
