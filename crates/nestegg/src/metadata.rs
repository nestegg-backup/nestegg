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

use crate::client::{Client, RestoreRequest};

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
}

/// What a `snapshot files` listing says about the metadata blob, for
/// [`fetch`].
#[derive(Debug, PartialEq, Eq)]
pub enum BlobListing {
    /// No blob at all (an older nestegg version made the snapshot).
    Absent,
    /// The blob is present and small enough to read.
    Present,
    /// The blob is listed with an implausible size; it is not read.
    TooLarge(u64),
}

/// Decide from a `snapshot files` listing whether the metadata blob is there.
/// The blob counts as present when the listing shows a crypt mode for it or,
/// on an older server that lists no crypt modes, at least a size — the same
/// reading as the read-back's listing check, so an old listing is not taken
/// for "no blob".
pub fn blob_listing(files_json: &str) -> BlobListing {
    let size = crate::client::size_for_archive(files_json, ARCHIVE);
    let present =
        crate::client::crypt_mode_for_archive(files_json, ARCHIVE).is_some() || size.is_some();
    if !present {
        return BlobListing::Absent;
    }
    match size {
        Some(s) if s > MAX_BYTES => BlobListing::TooLarge(s),
        _ => BlobListing::Present,
    }
}

/// Read the metadata blob of `snapshot`. `None` if the snapshot has none
/// (made by an older version). `work` must be a private, root-only
/// directory; the blob is restored into it.
///
/// With encryption configured, the client is always handed the key
/// ([`Client::configured_key`]), so it verifies the manifest signature
/// itself: the key is never withheld because the server lists the blob as
/// `none` (decision 0016). The listing is used only to see whether the blob
/// is present at all. The calls are time-bounded and cancellable so a server
/// that fell asleep after the upload cannot make the read-back hang
/// (decision 0015).
pub fn fetch(client: &Client, snapshot: &str, work: &Path) -> Result<Option<Fetched>> {
    let files = client.snapshot_files_bounded(snapshot, crate::client::readback_list_timeout())?;
    match blob_listing(&files) {
        BlobListing::Absent => return Ok(None),
        BlobListing::TooLarge(size) => {
            bail!("the backup metadata of {snapshot} is too large ({size} bytes)")
        }
        BlobListing::Present => {}
    }
    let target = work.join(format!("meta-{:016x}", rand::random::<u64>()));
    let mut log = Vec::new();
    let restored = client.restore_bounded(
        &RestoreRequest {
            snapshot,
            archive: ARCHIVE,
            target: &target,
            patterns: &[],
            key: client.configured_key()?,
        },
        crate::client::readback_list_timeout(),
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
    Ok(Some(Fetched { bytes: result? }))
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
    fn blob_listing_detects_presence_old_server_and_size() {
        // Modern server: crypt mode present.
        assert_eq!(
            blob_listing(
                r#"[{"filename":"nestegg-meta.conf.blob","crypt-mode":"encrypt","size":300}]"#
            ),
            BlobListing::Present
        );
        // Older server: the blob is listed with a size but no crypt mode. It
        // must count as present, not as "no blob" (item of commit 28d0133).
        assert_eq!(
            blob_listing(r#"[{"filename":"nestegg-meta.conf.blob","size":300}]"#),
            BlobListing::Present
        );
        // No blob at all.
        assert_eq!(
            blob_listing(r#"[{"filename":"home.pxar.didx","crypt-mode":"encrypt","size":10}]"#),
            BlobListing::Absent
        );
        assert_eq!(blob_listing("[]"), BlobListing::Absent);
        // Implausibly large blobs are not read.
        let huge = format!(
            r#"[{{"filename":"nestegg-meta.conf.blob","crypt-mode":"encrypt","size":{}}}]"#,
            MAX_BYTES + 1
        );
        assert_eq!(blob_listing(&huge), BlobListing::TooLarge(MAX_BYTES + 1));
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
