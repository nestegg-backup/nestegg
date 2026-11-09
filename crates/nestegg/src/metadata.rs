//! Backup metadata stored inside every snapshot as a blob archive
//! (decisions 0015 and 0016).
//!
//! The run writes a small JSON file into the root-only runtime directory
//! and passes it to `proxmox-backup-client backup` as `nestegg-meta.conf`;
//! the server stores it as `nestegg-meta.conf.blob` next to the data
//! archives. Unlike the snapshot notes it is part of the snapshot's
//! manifest: encrypted and signed with the encryption key when encryption is
//! on, and immutable once the snapshot is finished. The daemon uses it to
//! decide what an unprivileged user may browse and restore.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::client::{Client, RestoreKey, RestoreRequest};

/// Archive name in the backup spec and for `restore`.
pub const ARCHIVE: &str = "nestegg-meta.conf";
/// File name of the archive on the server.
pub const BLOB_FILE: &str = "nestegg-meta.conf.blob";
/// Current format version of [`BackupMetadata`].
pub const FORMAT: u32 = 1;
/// Larger blobs are not read (the real one is a few hundred bytes).
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupMetadata {
    pub format: u32,
    pub nestegg_version: String,
    pub backup_id: String,
    pub hostname: String,
    /// Backup time in epoch seconds (the snapshot's time).
    pub backup_time: i64,
    /// `host/<backup id>/<RFC 3339 time>`.
    pub snapshot: String,
    /// `home.pxar` → canonical source path, only for archives of this
    /// snapshot. Basis for the scope of unprivileged users.
    pub archive_sources: BTreeMap<String, String>,
    /// Configured source paths.
    pub sources: Vec<String>,
    /// The configuration asks for a full system backup.
    pub system_mode: bool,
    /// Why the snapshot does not contain everything the configuration asks
    /// for (same as `Run::incomplete`).
    #[serde(default)]
    pub incomplete: Vec<String>,
}

impl BackupMetadata {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = serde_json::to_vec_pretty(self).unwrap_or_default();
        bytes.push(b'\n');
        bytes
    }

    pub fn parse(bytes: &[u8]) -> Result<BackupMetadata> {
        let meta: BackupMetadata =
            serde_json::from_slice(bytes).context("the backup metadata is not valid JSON")?;
        if meta.format == 0 {
            bail!("the backup metadata has no format version");
        }
        Ok(meta)
    }

    /// The metadata describes exactly `snapshot` (`host/<id>/<time>`).
    pub fn belongs_to(&self, snapshot: &str) -> bool {
        self.snapshot == snapshot
            && crate::client::snapshot_path(&self.backup_id, self.backup_time) == snapshot
    }

    pub fn source_of(&self, archive: &str) -> Option<&str> {
        self.archive_sources.get(archive).map(String::as_str)
    }
}

/// The metadata blob as read back from the server.
pub struct Fetched {
    pub bytes: Vec<u8>,
    /// `none`, `sign-only` or `encrypt`.
    pub crypt_mode: String,
}

/// Read the metadata blob of `snapshot` with the configured key. `None` if
/// the snapshot has none (made by an older version). `work` must be a
/// private, root-only directory; the blob is restored into it.
pub fn fetch(client: &Client, snapshot: &str, work: &Path) -> Result<Option<Fetched>> {
    let files = client.snapshot_files(snapshot)?;
    let Some(crypt_mode) = crate::client::crypt_mode_for_archive(&files, ARCHIVE) else {
        return Ok(None);
    };
    if let Some(size) = crate::client::size_for_archive(&files, ARCHIVE)
        && size > MAX_BYTES
    {
        bail!("the backup metadata of {snapshot} is too large ({size} bytes)");
    }
    let target = work.join(format!("meta-{:016x}", rand::random::<u64>()));
    let mut log = Vec::new();
    let restored = client.restore(
        &RestoreRequest {
            snapshot,
            archive: ARCHIVE,
            target: &target,
            patterns: &[],
            key: RestoreKey::Auto,
        },
        &mut log,
    );
    let result = restored.and_then(|()| {
        let len = std::fs::symlink_metadata(&target)
            .with_context(|| format!("the client did not write {}", target.display()))?
            .len();
        if len > MAX_BYTES {
            bail!("the backup metadata of {snapshot} is too large ({len} bytes)");
        }
        std::fs::read(&target).with_context(|| format!("cannot read {}", target.display()))
    });
    let _ = std::fs::remove_file(&target);
    Ok(Some(Fetched {
        bytes: result?,
        crypt_mode,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> BackupMetadata {
        BackupMetadata {
            format: FORMAT,
            nestegg_version: "0.1.0".into(),
            backup_id: "desktop".into(),
            hostname: "desktop".into(),
            backup_time: 1_794_000_000,
            snapshot: crate::client::snapshot_path("desktop", 1_794_000_000),
            archive_sources: [("home.pxar".to_string(), "/home".to_string())].into(),
            sources: vec!["/home".into()],
            system_mode: false,
            incomplete: vec![],
        }
    }

    #[test]
    fn roundtrip_and_snapshot_binding() {
        let m = meta();
        let back = BackupMetadata::parse(&m.to_bytes()).unwrap();
        assert_eq!(back, m);
        assert!(m.belongs_to("host/desktop/2026-11-06T21:20:00Z"));
        assert!(!m.belongs_to("host/desktop/2026-11-06T21:20:01Z"));
        assert!(!m.belongs_to("host/other/2026-11-06T21:20:00Z"));
        // A changed snapshot field does not help: id and time must match it.
        let mut moved = m.clone();
        moved.snapshot = "host/desktop/2026-11-07T00:00:00Z".into();
        assert!(!moved.belongs_to("host/desktop/2026-11-07T00:00:00Z"));
        assert_eq!(m.source_of("home.pxar"), Some("/home"));
        assert_eq!(m.source_of("etc.pxar"), None);
    }

    #[test]
    fn invalid_metadata_is_rejected() {
        assert!(BackupMetadata::parse(b"not json").is_err());
        assert!(BackupMetadata::parse(b"{}").is_err());
        let mut m = serde_json::to_value(meta()).unwrap();
        m["format"] = 0.into();
        assert!(BackupMetadata::parse(m.to_string().as_bytes()).is_err());
        // Unknown fields of later versions are ignored.
        let mut m = serde_json::to_value(meta()).unwrap();
        m["future"] = true.into();
        assert!(BackupMetadata::parse(m.to_string().as_bytes()).is_ok());
    }
}
