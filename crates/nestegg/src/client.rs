//! Wrapper around the official `proxmox-backup-client`.
//!
//! All PBS operations go through this module. Argument building is kept in
//! pure functions so it can be unit-tested without a server.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::config::{ChangeDetectionMode, Config};
use crate::secrets::{self, FdPassing, Secret, SecretStore};
use crate::state::{ErrorKind, RunError};

const PASSWORD_FD: i32 = 10;
const KEY_FD: i32 = 11;

/// Time limit for the small listing calls of the read-back (`snapshot
/// files`, metadata blob restore). They move a few hundred bytes; if the
/// server fell asleep right after the upload they must not hang and hold the
/// run lock for hours (decision 0015). Minutes, not hours.
pub fn readback_list_timeout() -> std::time::Duration {
    readback_timeout(std::time::Duration::from_secs(120))
}

/// Time limit for restoring the inventory archive during the read-back. It
/// is larger than the blob (KiB, not hundreds of bytes) but still small.
pub fn readback_restore_timeout() -> std::time::Duration {
    readback_timeout(std::time::Duration::from_secs(300))
}

/// `NESTEGG_READBACK_TIMEOUT_SECS` overrides the read-back time limits (used
/// by the tests to exercise a timeout without waiting minutes).
fn readback_timeout(default: std::time::Duration) -> std::time::Duration {
    std::env::var("NESTEGG_READBACK_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(default)
}

pub fn client_binary() -> PathBuf {
    std::env::var_os("NESTEGG_PBS_CLIENT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("proxmox-backup-client"))
}

/// One archive of a backup run.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchiveSpec {
    pub name: String,
    pub path: PathBuf,
}

impl ArchiveSpec {
    pub fn file_name(&self) -> String {
        format!("{}.pxar", self.name)
    }
}

#[derive(Debug, Clone)]
pub struct BackupSpec {
    pub backup_id: String,
    pub namespace: Option<String>,
    pub backup_time: i64,
    pub archives: Vec<ArchiveSpec>,
    pub excludes: Vec<String>,
    pub include_dev: Vec<PathBuf>,
    pub encrypt: bool,
    pub change_detection_mode: Option<ChangeDetectionMode>,
    pub rate_limit: Option<String>,
    /// File uploaded as the metadata blob (`nestegg-meta.conf`).
    pub metadata: Option<PathBuf>,
}

/// Arguments after `proxmox-backup-client` for a backup (no secrets).
pub fn backup_args(spec: &BackupSpec) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["backup".into()];
    for archive in &spec.archives {
        let mut a = OsString::from(format!("{}:", archive.file_name()));
        a.push(archive.path.as_os_str());
        args.push(a);
    }
    if let Some(meta) = &spec.metadata {
        let mut a = OsString::from(format!("{}:", crate::metadata::ARCHIVE));
        a.push(meta.as_os_str());
        args.push(a);
    }
    args.extend(["--backup-type".into(), "host".into()]);
    args.extend(["--backup-id".into(), spec.backup_id.clone().into()]);
    args.extend(["--backup-time".into(), spec.backup_time.to_string().into()]);
    if let Some(ns) = &spec.namespace {
        args.extend(["--ns".into(), ns.into()]);
    }
    if spec.encrypt {
        args.extend(["--keyfd".into(), KEY_FD.to_string().into()]);
        args.extend(["--crypt-mode".into(), "encrypt".into()]);
    } else {
        args.extend(["--crypt-mode".into(), "none".into()]);
    }
    for pattern in &spec.excludes {
        args.extend(["--exclude".into(), pattern.into()]);
    }
    for dev in &spec.include_dev {
        args.push("--include-dev".into());
        args.push(dev.as_os_str().to_owned());
    }
    if let Some(mode) = spec.change_detection_mode {
        args.extend(["--change-detection-mode".into(), mode.as_str().into()]);
    }
    if let Some(rate) = &spec.rate_limit {
        args.extend(["--rate".into(), rate.into()]);
    }
    args.extend(["--skip-lost-and-found".into(), "true".into()]);
    args
}

/// Backup time (epoch seconds) from a snapshot path `type/id/<RFC 3339>`.
pub fn snapshot_epoch(snapshot: &str) -> Option<i64> {
    let time = snapshot.rsplit('/').next()?;
    DateTime::parse_from_rfc3339(time)
        .ok()
        .map(|t| t.timestamp())
}

/// Snapshot path as used by the PBS: `host/<id>/<RFC 3339 UTC>`.
pub fn snapshot_path(backup_id: &str, backup_time: i64) -> String {
    let t = Utc
        .timestamp_opt(backup_time, 0)
        .single()
        .unwrap_or_else(Utc::now);
    format!("host/{backup_id}/{}", crate::state::format_time(t))
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ArchiveStats {
    pub name: String,
    pub uploaded_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
}

/// Parse the client's per-archive summary lines, e.g.
/// `home.pxar: had to backup 1.23 GiB of 10.5 GiB (compressed 900 MiB) in 30.12 s`.
pub fn parse_archive_stats(line: &str) -> Option<ArchiveStats> {
    let (name, rest) = line.split_once(": had to backup ")?;
    let name = name.trim().rsplit(' ').next()?.to_string();
    let (uploaded, rest) = rest.split_once(" of ")?;
    let total = rest.split(" (").next().unwrap_or(rest);
    let total = total.split(" in ").next().unwrap_or(total);
    Some(ArchiveStats {
        name,
        uploaded_bytes: parse_size(uploaded),
        total_bytes: parse_size(total),
    })
}

pub fn parse_size(s: &str) -> Option<u64> {
    let mut parts = s.split_whitespace();
    let number: f64 = parts.next()?.parse().ok()?;
    let unit = parts.next().unwrap_or("B");
    let factor: f64 = match unit {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0_f64.powi(2),
        "GiB" => 1024.0_f64.powi(3),
        "TiB" => 1024.0_f64.powi(4),
        "PiB" => 1024.0_f64.powi(5),
        "KB" | "kB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return None,
    };
    Some((number * factor).round() as u64)
}

/// A time limit that nestegg itself imposed on a client call (the `*_bounded`
/// calls) was hit. Carried in the error chain so callers decide by type, not
/// by the words of a message (decision 0015).
#[derive(Debug)]
pub struct TimedOut(pub String);

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TimedOut {}

/// A bounded client call failed because the server could not be reached (the
/// client's own transport errors, see [`is_transport_failure`]). Carried in
/// the error chain like [`TimedOut`].
#[derive(Debug)]
pub struct Unreachable(pub String);

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreachable {}

/// Whether an error chain carries [`TimedOut`] or [`Unreachable`]: the server
/// stopped answering, not a fault of the snapshot.
pub fn is_unavailable_error(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<TimedOut>().is_some() || c.downcast_ref::<Unreachable>().is_some()
    })
}

/// Whether the error output of a failed client call is a transport failure:
/// the connection to the server could not be made or broke. Narrower than
/// [`classify_failure`]: a bare "timed out" is not enough (a server task or
/// an error message from the server may contain these words); only the
/// connection-level phrases of the client and the OS count. The client exits
/// with the same code for every error, so its words are all there is to go
/// by.
pub fn is_transport_failure(stderr: &str) -> bool {
    let text = stderr.to_lowercase();
    [
        "connection refused",
        "no route to host",
        "network is unreachable",
        "failed to lookup address",
        "error trying to connect",
        "connection timed out",
        "connection reset",
        "broken pipe",
    ]
    .iter()
    .any(|p| text.contains(p))
}

/// The error of a bounded client call that exited unsuccessfully: typed as
/// [`Unreachable`] for a transport failure, a plain error otherwise.
fn bounded_failure(stderr: &[u8]) -> anyhow::Error {
    let text = String::from_utf8_lossy(stderr).trim().to_string();
    // Judged by the client's own error line only, not by everything it
    // printed before: less room for words that merely pass through.
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let line = last_error_line(&lines).unwrap_or_default();
    if is_transport_failure(&line) {
        Unreachable(text).into()
    } else {
        anyhow::anyhow!("{text}")
    }
}

/// Distinguish an actual key/signature refusal from an unreadable manifest.
pub fn group_key_error_kind(error: &anyhow::Error) -> ErrorKind {
    let message = format!("{error:#}").to_ascii_lowercase();
    if message.contains("wrong key") || message.contains("manifest has no signature") {
        ErrorKind::KeyMismatch
    } else {
        ErrorKind::VerificationFailed
    }
}

/// Map client output to an error category.
pub fn classify_failure(output_tail: &[String]) -> ErrorKind {
    let text = output_tail.join("\n").to_lowercase();
    if text.contains("authentication failed")
        || text.contains("permission check failed")
        || text.contains("401 unauthorized")
        || text.contains("no password")
        || text.contains("invalid credentials")
    {
        ErrorKind::Authentication
    } else if text.contains("connection refused")
        || text.contains("no route to host")
        || text.contains("timed out")
        || text.contains("network is unreachable")
        || text.contains("failed to lookup address")
        || text.contains("error trying to connect")
    {
        ErrorKind::PbsUnreachable
    } else {
        ErrorKind::ClientFailed
    }
}

/// Whether a restore error means the snapshot's manifest is not signed with
/// the key we handed to the client, as opposed to a transport or file-system
/// error. The client prints these messages when a key is set but the
/// manifest has no signature at all (an unencrypted, possibly forged
/// snapshot). Used so the daemon can fall back to the local run record for
/// such a snapshot instead of treating it as a hard error (decision 0016).
pub fn manifest_unsigned_error(message: &str) -> bool {
    message
        .to_lowercase()
        .contains("manifest has no signature, but an encryption key is set")
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SnapshotInfo {
    pub time: String,
    pub snapshot: String,
    pub size_bytes: Option<u64>,
    pub archives: Vec<String>,
    pub encrypted: Option<bool>,
    pub verified: Option<String>,
    pub protected: bool,
    pub comment: Option<String>,
    /// Logical size of each archive (`system.pxar.didx` → bytes).
    #[serde(skip)]
    pub archive_bytes: Vec<(String, u64)>,
    /// Crypt mode of each file as the server lists it
    /// (`system.pxar.didx` → `encrypt`). Not verified: only the manifest
    /// read with the key is (see [`Client::read_manifest_with_key`]).
    #[serde(skip)]
    pub crypt_modes: Vec<(String, String)>,
}

fn parse_space_info(v: &Value) -> Result<(u64, Option<u64>)> {
    let available = v["avail"]
        .as_u64()
        .context("PBS did not report space available to its backup user")?;
    Ok((available, v["total"].as_u64().filter(|total| *total > 0)))
}

/// PBS itself defines a finished snapshot by the presence of its manifest.
/// A failed upload can leave a newer unfinished directory; it must not hide
/// the last completed snapshot's key, nor prevent the official client retry.
pub(crate) fn last_finished_snapshot(snapshots: &[SnapshotInfo]) -> Option<&SnapshotInfo> {
    snapshots
        .iter()
        .filter(|s| s.archives.iter().any(|name| name == "index.json.blob"))
        .max_by_key(|s| snapshot_epoch(&s.snapshot))
}

/// Convert the JSON of `snapshot list --output-format json`.
pub fn parse_snapshot_list(json: &str) -> Result<Vec<SnapshotInfo>> {
    let value: Value = serde_json::from_str(json).context("unexpected snapshot list output")?;
    let items = value.as_array().context("snapshot list is not an array")?;
    let mut out = Vec::new();
    for item in items {
        let backup_type = item["backup-type"].as_str().unwrap_or("host");
        let backup_id = item["backup-id"].as_str().unwrap_or_default();
        let Some(epoch) = item["backup-time"].as_i64() else {
            continue;
        };
        let Some(time) = Utc.timestamp_opt(epoch, 0).single() else {
            continue;
        };
        let time_str = crate::state::format_time(time);
        let files = item["files"].as_array().cloned().unwrap_or_default();
        let archives: Vec<String> = files
            .iter()
            .filter_map(|f| {
                f["filename"]
                    .as_str()
                    .or_else(|| f.as_str())
                    .map(String::from)
            })
            .collect();
        let archive_bytes: Vec<(String, u64)> = files
            .iter()
            .filter_map(|f| Some((f["filename"].as_str()?.to_string(), f["size"].as_u64()?)))
            .collect();
        let crypt_modes: Vec<(String, String)> = files
            .iter()
            .filter_map(|f| {
                Some((
                    f["filename"].as_str()?.to_string(),
                    f["crypt-mode"].as_str()?.to_string(),
                ))
            })
            .collect();
        let encrypted = if crypt_modes.is_empty() {
            None
        } else {
            Some(crypt_modes.iter().any(|(_, m)| m == "encrypt"))
        };
        out.push(SnapshotInfo {
            snapshot: format!("{backup_type}/{backup_id}/{time_str}"),
            time: time_str,
            size_bytes: item["size"].as_u64(),
            archives,
            encrypted,
            verified: item["verification"]["state"].as_str().map(String::from),
            protected: item["protected"].as_bool().unwrap_or(false),
            comment: item["comment"].as_str().map(String::from),
            archive_bytes,
            crypt_modes,
        });
    }
    out.sort_by(|a, b| b.time.cmp(&a.time));
    Ok(out)
}

pub enum RestoreKey {
    /// Use the stored key if the archive is signed or encrypted.
    Auto,
    /// Use this key (e.g. an exported key of an older key generation).
    File(Secret),
    /// Never pass a key.
    Without,
}

impl RestoreKey {
    /// An independent copy (the key bytes are copied).
    pub fn duplicate(&self) -> RestoreKey {
        match self {
            RestoreKey::Auto => RestoreKey::Auto,
            RestoreKey::File(s) => RestoreKey::File(Secret::new(s.as_bytes().to_vec())),
            RestoreKey::Without => RestoreKey::Without,
        }
    }
}

pub struct RestoreRequest<'a> {
    pub snapshot: &'a str,
    pub archive: &'a str,
    pub target: &'a Path,
    pub patterns: &'a [String],
    pub key: RestoreKey,
}

fn archive_candidates(archive: &str) -> Vec<String> {
    let base = archive
        .strip_suffix(".pxar")
        .or_else(|| archive.strip_suffix(".mpxar"))
        .or_else(|| archive.strip_suffix(".ppxar"))
        .unwrap_or(archive);
    ["pxar", "mpxar", "ppxar", "img"]
        .iter()
        .flat_map(|ext| [format!("{base}.{ext}.didx"), format!("{base}.{ext}.fidx")])
        .chain([format!("{archive}.blob"), archive.to_string()])
        .collect()
}

/// Sum of the sizes of the files belonging to `archive` in the JSON of
/// `snapshot files`.
pub fn size_for_archive(json: &str, archive: &str) -> Option<u64> {
    let value: Value = serde_json::from_str(json).ok()?;
    let candidates = archive_candidates(archive);
    let sizes: Vec<u64> = value
        .as_array()?
        .iter()
        .filter(|f| {
            f["filename"]
                .as_str()
                .is_some_and(|n| candidates.iter().any(|c| c == n))
        })
        .filter_map(|f| f["size"].as_u64())
        .collect();
    (!sizes.is_empty()).then(|| sizes.iter().sum())
}

/// Crypt modes of every file that belongs to `archive` (e.g. `home.pxar`;
/// a split archive has two: `home.mpxar` and `home.ppxar`) in the JSON of
/// `snapshot files` or the `files` of a manifest.
pub fn crypt_modes_for_archive(json: &str, archive: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let candidates = archive_candidates(archive);
    value
        .as_array()
        .map(|files| {
            files
                .iter()
                .filter(|f| {
                    f["filename"]
                        .as_str()
                        .is_some_and(|n| candidates.iter().any(|c| c == n))
                })
                .filter_map(|f| f["crypt-mode"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Find the crypt mode of `archive` (e.g. `home.pxar`) in the JSON of
/// `snapshot files`. Split archives (`home.mpxar`/`home.ppxar`) count too.
pub fn crypt_mode_for_archive(json: &str, archive: &str) -> Option<String> {
    let modes = crypt_modes_for_archive(json, archive);
    if modes.is_empty() {
        return None;
    }
    // Strongest mode wins.
    for m in ["encrypt", "sign-only", "none"] {
        if modes.iter().any(|x| x == m) {
            return Some(m.to_string());
        }
    }
    modes.into_iter().next()
}

/// Whether an archive of a snapshot is protected by the encryption key, as
/// the JSON of `snapshot files` lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protection {
    /// Encrypted or signed.
    Protected,
    /// Listed with crypt mode `none`.
    Unprotected,
    /// Listed, but without a crypt mode (an older server).
    ModeUnknown,
    /// Not in the snapshot at all.
    NotListed,
}

/// [`Protection`] of `archive` in the JSON of `snapshot files`.
pub fn archive_protection(json: &str, archive: &str) -> Protection {
    match crypt_mode_for_archive(json, archive).as_deref() {
        Some("encrypt") | Some("sign-only") => Protection::Protected,
        Some(_) => Protection::Unprotected,
        None => {
            let candidates = archive_candidates(archive);
            let listed = serde_json::from_str::<Value>(json)
                .ok()
                .and_then(|v| {
                    v.as_array().map(|files| {
                        files.iter().any(|f| {
                            f["filename"]
                                .as_str()
                                .is_some_and(|n| candidates.iter().any(|c| c == n))
                        })
                    })
                })
                .unwrap_or(false);
            if listed {
                Protection::ModeUnknown
            } else {
                Protection::NotListed
            }
        }
    }
}

/// Whether an [`RestoreKey::Auto`] restore of an archive must pass the key.
#[derive(Debug, PartialEq, Eq)]
pub enum AutoKey {
    /// Pass the stored encryption key (the archive is signed or encrypted).
    Key,
    /// No key: the archive is stored unencrypted, or an older server lists it
    /// without a crypt mode but with a size, so it is present and read
    /// without a key (the same reading as the read-back's listing check).
    None,
    /// The archive is not in the snapshot at all.
    Missing,
}

/// Decide from a single `snapshot files` listing whether an `Auto` restore of
/// `archive` needs the key. Taking both lookups from the same listing keeps
/// the answer consistent: an older server that lists the archive without a
/// crypt mode must not be read as "missing" by a crypt-mode-only lookup while
/// a size lookup sees it (the regression behind commit 28d0133).
pub fn auto_key_decision(files_json: &str, archive: &str) -> AutoKey {
    match crypt_mode_for_archive(files_json, archive).as_deref() {
        Some("none") => AutoKey::None,
        Some(_) => AutoKey::Key,
        None if size_for_archive(files_json, archive).is_some() => AutoKey::None,
        None => AutoKey::Missing,
    }
}

pub struct BackupOutcome {
    pub archives: Vec<ArchiveStats>,
}

pub struct Client<'a> {
    local: Option<&'a crate::stick::launcher::LocalPbs>,
    config: &'a Config,
    secrets: SecretStore,
    /// Overrides `pbs.auth_id` (read-only overview credentials).
    auth_id: Option<String>,
    password_secret: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupInfo {
    pub backup_type: String,
    pub backup_id: String,
    pub last_backup: Option<i64>,
    pub backup_count: Option<u64>,
}

/// Convert the JSON of `list --output-format json`.
pub fn parse_group_list(json: &str) -> Result<Vec<GroupInfo>> {
    let value: Value = serde_json::from_str(json).context("unexpected group list output")?;
    Ok(value
        .as_array()
        .context("group list is not an array")?
        .iter()
        .filter_map(|g| {
            Some(GroupInfo {
                backup_type: g["backup-type"].as_str()?.to_string(),
                backup_id: g["backup-id"].as_str()?.to_string(),
                last_backup: g["last-backup"].as_i64(),
                backup_count: g["backup-count"].as_u64(),
            })
        })
        .collect())
}

impl<'a> Client<'a> {
    pub fn new(config: &'a Config, secrets: SecretStore) -> Client<'a> {
        Client {
            config,
            secrets,
            auth_id: None,
            password_secret: secrets::PASSWORD,
            local: None,
        }
    }

    pub fn local(
        config: &'a Config,
        secrets: SecretStore,
        pbs: &'a crate::stick::launcher::LocalPbs,
    ) -> Self {
        let mut client = Self::new(config, secrets);
        client.local = Some(pbs);
        client
    }

    /// Client with other credentials (e.g. the read-only overview token).
    pub fn with_credentials(
        config: &'a Config,
        secrets: SecretStore,
        auth_id: &str,
        password_secret: &'static str,
    ) -> Client<'a> {
        Client {
            config,
            secrets,
            auth_id: Some(auth_id.to_string()),
            password_secret,
            local: None,
        }
    }

    fn repository(&self) -> String {
        match &self.auth_id {
            Some(id) => format!(
                "{id}@{}:{}:{}",
                self.config
                    .pbs
                    .as_ref()
                    .map(|p| p.server.as_str())
                    .unwrap_or_default(),
                self.config.pbs.as_ref().map(|p| p.port).unwrap_or(8007),
                self.config
                    .pbs
                    .as_ref()
                    .map(|p| p.datastore.as_str())
                    .unwrap_or_default()
            ),
            None => self.config.repository(),
        }
    }

    fn output(&self, args: &[&str], with_namespace: bool) -> Result<String> {
        let mut fds = FdPassing::new();
        let mut cmd = self.command(&mut fds)?;
        cmd.args(args);
        if with_namespace {
            self.add_namespace(&mut cmd);
        }
        fds.apply(&mut cmd);
        let output = run_capturing(&mut cmd, Some(std::time::Duration::from_secs(12 * 3600)))?;
        if output.stopped != Stopped::No {
            bail!("client query cancelled or timed out");
        }
        if !output.status.success() {
            return Err(bounded_failure(&output.stderr));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Backup groups in the configured namespace.
    pub fn list_groups(&self) -> Result<Vec<GroupInfo>> {
        parse_group_list(&self.output(&["list", "--output-format", "json"], true)?)
    }

    /// Snapshots of any group (`host/<id>`).
    pub fn list_group_snapshots(&self, group: &str) -> Result<Vec<SnapshotInfo>> {
        parse_snapshot_list(&self.output(
            &["snapshot", "list", group, "--output-format", "json"],
            true,
        )?)
    }

    pub fn notes(&self, snapshot: &str) -> Result<String> {
        self.output(&["snapshot", "notes", "show", snapshot], true)
    }

    pub fn available_bytes(&self) -> Result<u64> {
        Ok(self.space_info()?.0)
    }

    /// Space available to the backup user and the datastore's total size.
    pub fn space_info(&self) -> Result<(u64, Option<u64>)> {
        let v: Value =
            serde_json::from_str(&self.output(&["status", "--output-format", "json"], false)?)?;
        parse_space_info(&v)
    }

    /// Datastore usage `(used, total)`; `None` if the token may not see it.
    pub fn datastore_usage(&self) -> Result<Option<(u64, u64)>> {
        let v: Value =
            serde_json::from_str(&self.output(&["status", "--output-format", "json"], false)?)
                .context("unexpected status output")?;
        let (used, total) = (
            v["used"].as_u64().unwrap_or(0),
            v["total"].as_u64().unwrap_or(0),
        );
        Ok((total > 0).then_some((used, total)))
    }

    /// Write the catalog of `snapshot` to `out` (see [`crate::catalog`]).
    pub fn catalog_dump(&self, snapshot: &str, out: &Path) -> Result<()> {
        let files = self.snapshot_files(snapshot)?;
        let needs_key =
            crypt_mode_for_archive(&files, "catalog.pcat1.didx").is_some_and(|m| m != "none");
        let mut fds = FdPassing::new();
        let mut cmd = self.command(&mut fds)?;
        cmd.args(["catalog", "dump", snapshot]);
        if needs_key {
            let key = self.secrets.load(secrets::ENCRYPTION_KEY)?;
            fds.add(&key, KEY_FD)?;
            cmd.args(["--keyfd", &KEY_FD.to_string()]);
        } else {
            cmd.args(["--crypt-mode", "none"]);
        }
        self.add_namespace(&mut cmd);
        fds.apply(&mut cmd);
        // The client prints the catalog through its logger (stderr), so both
        // streams go into the file.
        let tmp = out.with_extension(format!("{:08x}.tmp", rand::random::<u32>()));
        let file = std::fs::File::create(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        let status = cmd
            .stdout(file.try_clone()?)
            .stderr(file)
            .status()
            .context("cannot run proxmox-backup-client")?;
        // Stream the check instead of loading a possibly huge dump.
        let valid = status.success()
            && std::fs::File::open(&tmp)
                .map(|f| {
                    BufReader::new(f)
                        .lines()
                        .map_while(Result::ok)
                        .any(|l| crate::catalog::parse_line(&l).is_some())
                })
                .unwrap_or(false);
        if !valid {
            let tail = tail_of_file(&tmp, 2048);
            let _ = std::fs::remove_file(&tmp);
            let last = tail
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string();
            bail!("catalog dump failed: {last}");
        }
        std::fs::rename(&tmp, out)?;
        Ok(())
    }

    /// Base command with repository, fingerprint and password descriptor.
    fn command(&self, fds: &mut FdPassing) -> Result<Command> {
        let mut cmd = match self.local {
            Some(local) => local.client_command(&client_binary())?,
            None => {
                self.config.remote_pbs()?;
                Command::new(client_binary())
            }
        };
        for var in [
            "PBS_PASSWORD",
            "PBS_PASSWORD_FILE",
            "PBS_PASSWORD_CMD",
            "PBS_ENCRYPTION_PASSWORD",
            "PBS_ENCRYPTION_PASSWORD_FD",
            "PBS_ENCRYPTION_PASSWORD_FILE",
            "PBS_ENCRYPTION_PASSWORD_CMD",
            "PBS_REPOSITORY",
            "PBS_NAMESPACE",
            "PBS_FINGERPRINT",
        ] {
            cmd.env_remove(var);
        }
        if let Some(local) = self.local {
            cmd.env("PBS_REPOSITORY", local.repository())
                .env("PBS_FINGERPRINT", local.fingerprint())
                .env("PBS_PASSWORD_FD", "9");
            cmd.stdin(Stdio::null());
            return Ok(cmd);
        }
        cmd.env("PBS_REPOSITORY", self.repository());
        if let Some(fp) = &self.config.remote_pbs()?.fingerprint {
            cmd.env("PBS_FINGERPRINT", fp);
        }
        let password = self.secrets.load(self.password_secret)?;
        let fd = fds.add(&trim_secret(password), PASSWORD_FD)?;
        cmd.env("PBS_PASSWORD_FD", fd.to_string());
        cmd.stdin(Stdio::null());
        Ok(cmd)
    }

    fn add_namespace(&self, cmd: &mut Command) {
        if self.local.is_none()
            && let Some(ns) = self.config.pbs.as_ref().and_then(|p| p.namespace.as_ref())
        {
            cmd.args(["--ns", ns]);
        }
    }

    pub fn check_group_key(&self, backup_id: &str) -> Result<()> {
        let snapshots = self.list_snapshots_bounded(backup_id, readback_list_timeout())?;
        if let Some(last) = last_finished_snapshot(&snapshots) {
            let key = self.secrets.load(secrets::ENCRYPTION_KEY)?;
            // The official client itself rejects unsigned/wrong-key manifests.
            self.read_manifest_with_key(&last.snapshot, &key)?;
        }
        Ok(())
    }

    pub fn prune_stick(
        &self,
        backup_id: &str,
        retention: &crate::config::StickRetention,
    ) -> Result<()> {
        let group = format!("host/{backup_id}");
        let mut args = vec!["prune".to_string(), group];
        for (flag, count) in [
            ("--keep-last", retention.keep_last),
            ("--keep-daily", retention.keep_daily),
            ("--keep-weekly", retention.keep_weekly),
            ("--keep-monthly", retention.keep_monthly),
        ] {
            if count > 0 {
                args.extend([flag.into(), count.to_string()]);
            }
        }
        self.output(&args.iter().map(String::as_str).collect::<Vec<_>>(), false)?;
        Ok(())
    }

    /// Run a backup, streaming all client output into `log`.
    pub fn backup(
        &self,
        spec: &BackupSpec,
        log: &mut dyn Write,
        on_archive: &mut dyn FnMut(&ArchiveStats),
    ) -> std::result::Result<BackupOutcome, RunError> {
        let internal = |e: anyhow::Error| RunError {
            kind: ErrorKind::Internal,
            message: format!("{e:#}"),
        };
        let mut fds = FdPassing::new();
        let mut cmd = self
            .command(&mut fds)
            .map_err(|e| RunError::from_error(&e, ErrorKind::Config))?;
        if spec.encrypt {
            let key = self
                .secrets
                .load(secrets::ENCRYPTION_KEY)
                .map_err(|e| RunError::from_error(&e, ErrorKind::Config))?;
            fds.add(&key, KEY_FD).map_err(internal)?;
        }
        cmd.args(backup_args(spec));
        fds.apply(&mut cmd);
        let mut stats = Vec::new();
        // No time limit on the upload itself (it may legitimately take hours);
        // only a cancel stops it (decision 0015).
        let (status, tail, stopped) = run_streaming_cancellable(
            &mut cmd,
            log,
            |line| {
                if let Some(s) = parse_archive_stats(line) {
                    on_archive(&s);
                    stats.push(s);
                }
            },
            None,
        )
        .map_err(|e| RunError {
            kind: ErrorKind::ClientMissing,
            message: format!("{e:#}"),
        })?;
        // Only a client that was actually stopped counts as cancelled; if it
        // finished successfully first, the snapshot exists and the run
        // completes normally.
        if stopped == Stopped::Cancelled && !status.success() {
            return Err(RunError {
                kind: ErrorKind::Cancelled,
                message: "the backup was cancelled".into(),
            });
        }
        if !status.success() {
            return Err(RunError {
                kind: classify_failure(&tail),
                message: last_error_line(&tail)
                    .unwrap_or_else(|| format!("proxmox-backup-client exited with {status}")),
            });
        }
        Ok(BackupOutcome { archives: stats })
    }

    pub fn list_snapshots(&self, backup_id: &str) -> Result<Vec<SnapshotInfo>> {
        let mut fds = FdPassing::new();
        let mut cmd = self.command(&mut fds)?;
        cmd.args([
            "snapshot",
            "list",
            &format!("host/{backup_id}"),
            "--output-format",
            "json",
        ]);
        self.add_namespace(&mut cmd);
        fds.apply(&mut cmd);
        let output = cmd.output().context("cannot run proxmox-backup-client")?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        parse_snapshot_list(&String::from_utf8_lossy(&output.stdout))
    }

    /// Like [`Client::list_snapshots`], but with a time limit. Used at the
    /// end of a backup run (the server's verification state), where a server
    /// that fell asleep must not keep the run and its lock hanging.
    pub fn list_snapshots_bounded(
        &self,
        backup_id: &str,
        timeout: std::time::Duration,
    ) -> Result<Vec<SnapshotInfo>> {
        self.list_snapshot_scope_bounded(Some(backup_id), timeout)
    }

    pub fn list_all_snapshots_bounded(
        &self,
        timeout: std::time::Duration,
    ) -> Result<Vec<SnapshotInfo>> {
        self.list_snapshot_scope_bounded(None, timeout)
    }

    fn list_snapshot_scope_bounded(
        &self,
        backup_id: Option<&str>,
        timeout: std::time::Duration,
    ) -> Result<Vec<SnapshotInfo>> {
        let mut fds = FdPassing::new();
        let mut cmd = self.command(&mut fds)?;
        cmd.args(["snapshot", "list"]);
        if let Some(id) = backup_id {
            cmd.arg(format!("host/{id}"));
        }
        cmd.args(["--output-format", "json"]);
        self.add_namespace(&mut cmd);
        fds.apply(&mut cmd);
        let cap = run_capturing(&mut cmd, Some(timeout))?;
        match cap.stopped {
            Stopped::TimedOut => {
                return Err(TimedOut(format!(
                    "listing the snapshots timed out after {}s",
                    timeout.as_secs()
                ))
                .into());
            }
            Stopped::Cancelled => bail!("listing the snapshots was cancelled"),
            Stopped::No => {}
        }
        if !cap.status.success() {
            return Err(bounded_failure(&cap.stderr));
        }
        parse_snapshot_list(&String::from_utf8_lossy(&cap.stdout))
    }

    /// Like [`Client::set_notes`], but with a time limit (end of a backup
    /// run, see [`Client::list_snapshots_bounded`]).
    pub fn set_notes_bounded(
        &self,
        snapshot: &str,
        notes: &str,
        timeout: std::time::Duration,
    ) -> Result<()> {
        let mut fds = FdPassing::new();
        let mut cmd = self.command(&mut fds)?;
        cmd.args(["snapshot", "notes", "update", snapshot, notes]);
        self.add_namespace(&mut cmd);
        fds.apply(&mut cmd);
        let cap = run_capturing(&mut cmd, Some(timeout))?;
        match cap.stopped {
            Stopped::TimedOut => {
                return Err(TimedOut(format!(
                    "writing the snapshot notes timed out after {}s",
                    timeout.as_secs()
                ))
                .into());
            }
            Stopped::Cancelled => bail!("writing the snapshot notes was cancelled"),
            Stopped::No => {}
        }
        if !cap.status.success() {
            return Err(bounded_failure(&cap.stderr));
        }
        Ok(())
    }

    pub fn set_notes(&self, snapshot: &str, notes: &str) -> Result<()> {
        let mut fds = FdPassing::new();
        let mut cmd = self.command(&mut fds)?;
        cmd.args(["snapshot", "notes", "update", snapshot, notes]);
        self.add_namespace(&mut cmd);
        fds.apply(&mut cmd);
        let output = cmd.output().context("cannot run proxmox-backup-client")?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(())
    }

    /// Size in bytes of an archive (sum of its index files), if listed.
    pub fn archive_size(&self, snapshot: &str, archive: &str) -> Result<Option<u64>> {
        Ok(size_for_archive(&self.snapshot_files(snapshot)?, archive))
    }

    /// JSON of `snapshot files`.
    pub fn snapshot_files(&self, snapshot: &str) -> Result<String> {
        let mut fds = FdPassing::new();
        let mut cmd = self.command(&mut fds)?;
        cmd.args(["snapshot", "files", snapshot, "--output-format", "json"]);
        self.add_namespace(&mut cmd);
        fds.apply(&mut cmd);
        let output = cmd.output().context("cannot run proxmox-backup-client")?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Like [`Client::snapshot_files`], but with a time limit and honouring a
    /// cancel request while the client runs. Used by the read-back so a
    /// server that fell asleep after the upload cannot make the run hang and
    /// hold the run lock (decision 0015).
    pub fn snapshot_files_bounded(
        &self,
        snapshot: &str,
        timeout: std::time::Duration,
    ) -> Result<String> {
        let mut fds = FdPassing::new();
        let mut cmd = self.command(&mut fds)?;
        cmd.args(["snapshot", "files", snapshot, "--output-format", "json"]);
        self.add_namespace(&mut cmd);
        fds.apply(&mut cmd);
        let cap = run_capturing(&mut cmd, Some(timeout))?;
        match cap.stopped {
            Stopped::TimedOut => {
                return Err(TimedOut(format!(
                    "listing the files of {snapshot} timed out after {}s",
                    timeout.as_secs()
                ))
                .into());
            }
            Stopped::Cancelled => bail!("listing the files of {snapshot} was cancelled"),
            Stopped::No => {}
        }
        if !cap.status.success() {
            return Err(bounded_failure(&cap.stderr));
        }
        Ok(String::from_utf8_lossy(&cap.stdout).into_owned())
    }

    /// Restore an archive (or the paths matching `patterns`) into `target`.
    ///
    /// The key is only passed when the archive is signed or encrypted (or
    /// an explicit key is given); passing a key for an unsigned archive makes
    /// the client reject it.
    pub fn restore(&self, request: &RestoreRequest<'_>, log: &mut dyn Write) -> Result<()> {
        self.restore_with_args(request, &[], None, log)
    }

    /// Like [`Client::restore`], but with a time limit (cancellation already
    /// works while the client runs). Used by the read-back for the inventory
    /// archive (decision 0015).
    pub fn restore_bounded(
        &self,
        request: &RestoreRequest<'_>,
        timeout: std::time::Duration,
        log: &mut dyn Write,
    ) -> Result<()> {
        self.restore_with_args(request, &[], Some(timeout), log)
    }

    /// Key handling for the trusted read-back and the permission lookup of
    /// the metadata blob and inventory (decisions 0015, 0016): with
    /// encryption on, always hand the configured key to the client so it
    /// verifies the manifest signature itself; the decision never depends on
    /// what the server lists. With encryption off, behaviour is unchanged
    /// (`Auto`, which reads an encrypted archive's key only when the listing
    /// says it is signed).
    pub fn configured_key(&self) -> Result<RestoreKey> {
        if self.config.backup.encryption {
            let key = self.secrets.load(secrets::ENCRYPTION_KEY).context(
                "this snapshot is encrypted; the configured encryption key is needed to read it back",
            )?;
            Ok(RestoreKey::File(key))
        } else {
            Ok(RestoreKey::Auto)
        }
    }

    /// Prove that `key` opens `snapshot`: restore its manifest (`index.json`)
    /// with that key. The client checks the manifest's signature with the
    /// key, so this only works with the key the snapshot was made with.
    /// `Ok(false)`: the snapshot is not signed or encrypted, so it proves
    /// nothing about a key.
    pub fn open_snapshot_with_key(&self, snapshot: &str, key: &Secret) -> Result<bool> {
        let files = self.snapshot_files(snapshot)?;
        if !listed_as_signed(&files) {
            return Ok(false);
        }
        self.read_manifest_with_key(snapshot, key)?;
        Ok(true)
    }

    /// The manifest (`index.json`) of `snapshot`, read with `key`. The
    /// client verifies the manifest's signature with the key before it
    /// prints it, so the result (including the crypt mode of every file)
    /// is what the holder of the key wrote. Fails for a wrong key ("wrong
    /// key - unable to verify signature ...") and for an unsigned manifest
    /// ("manifest has no signature, but an encryption key is set ...").
    pub fn read_manifest_with_key(&self, snapshot: &str, key: &Secret) -> Result<Value> {
        let mut fds = FdPassing::new();
        let mut cmd = self.command(&mut fds)?;
        // Target "-" writes the manifest to stdout.
        cmd.args(["restore", snapshot, "index.json", "-"]);
        fds.add(key, KEY_FD)?;
        cmd.args(["--keyfd", &KEY_FD.to_string()]);
        self.add_namespace(&mut cmd);
        fds.apply(&mut cmd);
        let output = run_capturing(&mut cmd, Some(readback_list_timeout()))?;
        match output.stopped {
            Stopped::TimedOut => {
                return Err(TimedOut("reading the signed manifest timed out".into()).into());
            }
            Stopped::Cancelled => bail!("reading the signed manifest was cancelled"),
            Stopped::No => {}
        }
        if !output.status.success() {
            return Err(bounded_failure(&output.stderr));
        }
        serde_json::from_slice::<Value>(&output.stdout)
            .context("the client did not return the manifest of the snapshot")
    }

    /// Restore into a directory that already exists and may contain
    /// (empty) mount points, as the rescue system does for a whole system.
    pub fn restore_into_existing(
        &self,
        request: &RestoreRequest<'_>,
        log: &mut dyn Write,
    ) -> Result<()> {
        self.restore_with_args(request, &["--allow-existing-dirs", "true"], None, log)
    }

    fn restore_with_args(
        &self,
        request: &RestoreRequest<'_>,
        extra: &[&str],
        timeout: Option<std::time::Duration>,
        log: &mut dyn Write,
    ) -> Result<()> {
        let key = match &request.key {
            RestoreKey::Without => None,
            RestoreKey::File(secret) => Some(Secret::new(secret.as_bytes().to_vec())),
            RestoreKey::Auto => {
                // One listing, both lookups (see [`auto_key_decision`]). On the
                // read-back path a time limit is set; the listing must then be
                // bounded and cancellable too, so a server that fell asleep
                // after the upload cannot hang here before the restore's own
                // time limit even applies (decision 0015).
                let files = match timeout {
                    Some(t) => self.snapshot_files_bounded(request.snapshot, t)?,
                    None => self.snapshot_files(request.snapshot)?,
                };
                match auto_key_decision(&files, request.archive) {
                    AutoKey::None => None,
                    AutoKey::Key => Some(self.secrets.load(secrets::ENCRYPTION_KEY).context(
                        "this archive is encrypted or signed; pass the exported key with --keyfile",
                    )?),
                    AutoKey::Missing => bail!(
                        "archive {} not found in snapshot {}",
                        request.archive,
                        request.snapshot
                    ),
                }
            }
        };
        let mut fds = FdPassing::new();
        let mut cmd = self.command(&mut fds)?;
        cmd.args(["restore", request.snapshot, request.archive])
            .arg(request.target);
        for p in request.patterns {
            cmd.args(["--pattern", p]);
        }
        cmd.args(extra);
        if let Some(key) = &key {
            fds.add(key, KEY_FD)?;
            cmd.args(["--keyfd", &KEY_FD.to_string()]);
        } else {
            // Without this the client falls back to its own default key.
            cmd.args(["--crypt-mode", "none"]);
        }
        self.add_namespace(&mut cmd);
        fds.apply(&mut cmd);
        let (status, tail, stopped) = run_streaming_cancellable(&mut cmd, log, |_| {}, timeout)?;
        if stopped == Stopped::TimedOut {
            return Err(TimedOut(format!(
                "restoring {} from {} timed out after {}s",
                request.archive,
                request.snapshot,
                timeout.map(|t| t.as_secs()).unwrap_or(0)
            ))
            .into());
        }
        if !status.success() {
            let line =
                last_error_line(&tail).unwrap_or_else(|| format!("restore failed with {status}"));
            // Only the bounded restores (read-back) are classified; the others
            // keep their plain error.
            if timeout.is_some() && is_transport_failure(&line) {
                return Err(Unreachable(line).into());
            }
            bail!("{line}");
        }
        Ok(())
    }
}

/// Whether any file in the JSON of `snapshot files` is signed or encrypted
/// (as the server lists it).
pub fn listed_as_signed(files_json: &str) -> bool {
    serde_json::from_str::<Value>(files_json)
        .ok()
        .and_then(|v| {
            v.as_array().map(|list| {
                list.iter()
                    .any(|f| f["crypt-mode"].as_str().is_some_and(|m| m != "none"))
            })
        })
        .unwrap_or(false)
}

/// Commands that do not talk to the server.
pub mod local {
    use super::*;

    pub fn version() -> Result<String> {
        let output = Command::new(client_binary())
            .args(["version", "--output-format", "json"])
            .stdin(Stdio::null())
            .output()
            .context("proxmox-backup-client is not installed or not in PATH")?;
        let text = String::from_utf8_lossy(&output.stdout);
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(v["client"]["version"]
            .as_str()
            .map(|ver| format!("{ver}-{}", v["client"]["release"].as_str().unwrap_or("?")))
            .unwrap_or_else(|| text.trim().to_string()))
    }

    /// Create an unprotected key file (`--kdf none`). The caller stores it
    /// as a credential and makes sure it is exported.
    pub fn create_key(path: &Path) -> Result<()> {
        let output = Command::new(client_binary())
            .args(["key", "create", "--kdf", "none"])
            .arg(path)
            .stdin(Stdio::null())
            .output()
            .context("cannot run proxmox-backup-client")?;
        if !output.status.success() {
            bail!(
                "key creation failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    pub fn key_fingerprint(path: &Path) -> Result<String> {
        Ok(key_info(path)?.fingerprint)
    }

    /// What `proxmox-backup-client key show` reports about a key file.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct KeyInfo {
        pub fingerprint: String,
        /// Key derivation of the key's password: `none` for keys without a
        /// password, `scrypt` or `pbkdf2` otherwise.
        pub kdf: Option<String>,
    }

    impl KeyInfo {
        /// nestegg passes the key through a file descriptor and has no key
        /// password, so it can only use keys created with `--kdf none`.
        pub fn require_unprotected(&self) -> Result<()> {
            match self.kdf.as_deref() {
                None | Some("none") => Ok(()),
                Some(kdf) => bail!(
                    "this key file is protected with a password (kdf {kdf}); nestegg needs a key \
                     without password. Remove the password from a copy first: \
                     proxmox-backup-client key change-passphrase --kdf none <copy of the file>"
                ),
            }
        }
    }

    /// Validate a key file with `key show` (it does not need the password).
    pub fn key_info(path: &Path) -> Result<KeyInfo> {
        let output = Command::new(client_binary())
            .args(["key", "show"])
            .arg(path)
            .args(["--output-format", "json"])
            .stdin(Stdio::null())
            .output()
            .context("cannot run proxmox-backup-client")?;
        if !output.status.success() {
            bail!(
                "cannot read key: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        parse_key_show(&output.stdout)
    }

    pub fn parse_key_show(stdout: &[u8]) -> Result<KeyInfo> {
        let v: Value = serde_json::from_slice(stdout).context("unexpected key show output")?;
        let fingerprint = v["fingerprint"]
            .as_str()
            .filter(|f| !f.is_empty())
            .map(String::from)
            .context("key show did not report a fingerprint")?;
        Ok(KeyInfo {
            fingerprint,
            kdf: v["kdf"].as_str().map(|k| k.to_lowercase()),
        })
    }

    /// Paper key (text with QR code) for printing.
    pub fn paperkey(path: &Path, format: &str) -> Result<Vec<u8>> {
        let output = Command::new(client_binary())
            .args(["key", "paperkey"])
            .arg(path)
            .args(["--output-format", format])
            .stdin(Stdio::null())
            .output()
            .context("cannot run proxmox-backup-client")?;
        if !output.status.success() {
            bail!(
                "paperkey failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }
}

fn trim_secret(secret: Secret) -> Secret {
    let bytes = secret.as_bytes();
    let end = bytes
        .iter()
        .rposition(|b| !matches!(b, b'\n' | b'\r'))
        .map_or(0, |i| i + 1);
    Secret::new(bytes[..end].to_vec())
}

fn last_error_line(tail: &[String]) -> Option<String> {
    tail.iter()
        .rev()
        .find(|l| l.to_lowercase().contains("error"))
        .or_else(|| tail.last())
        .map(|l| l.trim().to_string())
}

/// Why a streamed or captured client run stopped early (other than the
/// client exiting on its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
    No,
    /// A cancel was requested and the client was killed.
    Cancelled,
    /// The time limit was reached and the client was killed.
    TimedOut,
}

/// Run a command, forwarding stdout and stderr line by line into `log` and
/// to `on_line`. Returns the exit status, the last 40 lines and why it was
/// stopped (if it was). A cancel request always stops it; `timeout`, when
/// set, is an upper bound on how long the client may run.
fn run_streaming_cancellable(
    cmd: &mut Command,
    log: &mut dyn Write,
    mut on_line: impl FnMut(&str),
    timeout: Option<std::time::Duration>,
) -> Result<(std::process::ExitStatus, Vec<String>, Stopped)> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot start {}", client_binary().display()))?;
    let (tx, rx) = mpsc::channel::<String>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    if let Some(err) = child.stderr.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    drop(tx);
    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    let mut tail: Vec<String> = Vec::new();
    let mut stopped = Stopped::No;
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(300)) {
            Ok(line) => {
                let _ = writeln!(log, "{line}");
                on_line(&line);
                tail.push(line);
                if tail.len() > 40 {
                    tail.remove(0);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if stopped == Stopped::No && crate::cancel::requested() {
            let _ = writeln!(log, "cancel requested, stopping proxmox-backup-client");
            let _ = child.kill();
            stopped = Stopped::Cancelled;
        }
        if stopped == Stopped::No && deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            let _ = writeln!(log, "time limit reached, stopping proxmox-backup-client");
            let _ = child.kill();
            stopped = Stopped::TimedOut;
        }
    }
    for r in readers {
        let _ = r.join();
    }
    let status = child.wait()?;
    let _ = log.flush();
    Ok((status, tail, stopped))
}

/// Result of [`run_capturing`].
struct Captured {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stopped: Stopped,
}

/// Run a command to completion, capturing stdout and stderr in full, while
/// honouring a cancel request and an optional deadline. Used by the
/// read-back's `snapshot files` call, which needs the whole JSON output but
/// must not hang if the server fell asleep after the upload (decision 0015).
fn run_capturing(cmd: &mut Command, timeout: Option<std::time::Duration>) -> Result<Captured> {
    fn read_all<R: std::io::Read + Send + 'static>(
        r: Option<R>,
    ) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            buf
        })
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot start {}", client_binary().display()))?;
    let out_h = read_all(child.stdout.take());
    let err_h = read_all(child.stderr.take());
    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    let mut stopped = Stopped::No;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if stopped == Stopped::No && crate::cancel::requested() {
            let _ = child.kill();
            stopped = Stopped::Cancelled;
        }
        if stopped == Stopped::No && deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            let _ = child.kill();
            stopped = Stopped::TimedOut;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let stdout = out_h.join().unwrap_or_default();
    let stderr = err_h.join().unwrap_or_default();
    Ok(Captured {
        status,
        stdout,
        stderr,
        stopped,
    })
}

/// Last `max` bytes of a file as text.
fn tail_of_file(path: &Path, max: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(max)));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_uses_backup_user_availability_and_optional_total() {
        assert_eq!(
            parse_space_info(&serde_json::json!({"avail": 25, "used": 60, "total": 100})).unwrap(),
            (25, Some(100))
        );
        assert_eq!(
            parse_space_info(&serde_json::json!({"avail": 0})).unwrap(),
            (0, None)
        );
        assert!(parse_space_info(&serde_json::json!({"used": 10, "total": 100})).is_err());
    }

    #[test]
    fn key_gate_uses_last_completed_snapshot_after_an_interrupted_upload() {
        let snapshots = parse_snapshot_list(r#"[
          {"backup-type":"host","backup-id":"desktop","backup-time":30,"files":[{"filename":"files.pxar.didx"}]},
          {"backup-type":"host","backup-id":"desktop","backup-time":20,"files":[{"filename":"index.json.blob"}]},
          {"backup-type":"host","backup-id":"desktop","backup-time":10,"files":[{"filename":"index.json.blob"}]}
        ]"#).unwrap();
        assert_eq!(
            snapshot_epoch(&last_finished_snapshot(&snapshots).unwrap().snapshot),
            Some(20)
        );
        assert!(last_finished_snapshot(&snapshots[..1]).is_none());
    }

    fn spec() -> BackupSpec {
        BackupSpec {
            backup_id: "desktop".into(),
            namespace: Some("desktops".into()),
            backup_time: 1_794_000_000,
            archives: vec![
                ArchiveSpec {
                    name: "home".into(),
                    path: "/home".into(),
                },
                ArchiveSpec {
                    name: "etc".into(),
                    path: "/etc".into(),
                },
            ],
            excludes: vec!["**/.cache".into()],
            include_dev: vec!["/boot/efi".into()],
            encrypt: true,
            change_detection_mode: Some(ChangeDetectionMode::Metadata),
            rate_limit: Some("10MiB".into()),
            metadata: Some("/run/nestegg/tmp/run.meta.json".into()),
        }
    }

    fn strs(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn backup_arguments() {
        let args = strs(&backup_args(&spec()));
        assert_eq!(
            &args[..4],
            [
                "backup",
                "home.pxar:/home",
                "etc.pxar:/etc",
                "nestegg-meta.conf:/run/nestegg/tmp/run.meta.json"
            ]
        );
        let joined = args.join(" ");
        assert!(joined.contains("--backup-type host --backup-id desktop --backup-time 1794000000"));
        assert!(joined.contains("--ns desktops"));
        assert!(joined.contains("--keyfd 11 --crypt-mode encrypt"));
        assert!(joined.contains("--exclude **/.cache"));
        assert!(joined.contains("--include-dev /boot/efi"));
        assert!(joined.contains("--change-detection-mode metadata"));
        assert!(joined.contains("--rate 10MiB"));
    }

    #[test]
    fn unencrypted_backup_arguments() {
        let mut s = spec();
        s.encrypt = false;
        s.namespace = None;
        let joined = strs(&backup_args(&s)).join(" ");
        assert!(joined.contains("--crypt-mode none"));
        assert!(!joined.contains("--keyfd"));
        assert!(!joined.contains("--ns"));
    }

    #[test]
    fn snapshot_epoch_roundtrip() {
        assert_eq!(
            snapshot_epoch(&snapshot_path("desktop", 1_794_000_000)),
            Some(1_794_000_000)
        );
        assert_eq!(snapshot_epoch("garbage"), None);
    }

    #[test]
    fn snapshot_path_format() {
        assert_eq!(
            snapshot_path("desktop", 0),
            "host/desktop/1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn archive_stats() {
        let s = parse_archive_stats(
            "home.pxar: had to backup 1.5 GiB of 10 GiB (compressed 900 MiB) in 30.12 s (average 51 MiB/s)",
        )
        .unwrap();
        assert_eq!(s.name, "home.pxar");
        assert_eq!(s.uploaded_bytes, Some(1_610_612_736));
        assert_eq!(s.total_bytes, Some(10_737_418_240));
        assert!(parse_archive_stats("Starting backup").is_none());
        assert_eq!(parse_size("0 B"), Some(0));
        assert_eq!(parse_size("2 lightyears"), None);
    }

    #[test]
    fn failure_classification() {
        assert_eq!(
            classify_failure(&["Error: authentication failed - invalid credentials".into()]),
            ErrorKind::Authentication
        );
        assert_eq!(
            classify_failure(&[
                "Error: error trying to connect: tcp connect error: Connection refused".into()
            ]),
            ErrorKind::PbsUnreachable
        );
        assert_eq!(
            classify_failure(&["Error: something else".into()]),
            ErrorKind::ClientFailed
        );
    }

    #[test]
    fn snapshot_list_parsing() {
        let json = r#"[
          {"backup-type":"host","backup-id":"desktop","backup-time":1794000000,
           "files":[{"filename":"home.pxar.didx","crypt-mode":"encrypt","size":10},
                    {"filename":"index.json.blob","crypt-mode":"sign-only","size":1}],
           "size":11,"protected":false,"comment":"nestegg ok",
           "verification":{"state":"ok","upid":"x"}},
          {"backup-type":"host","backup-id":"desktop","backup-time":1793000000,"files":["a.pxar.didx"]}
        ]"#;
        let snaps = parse_snapshot_list(json).unwrap();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].snapshot, "host/desktop/2026-11-06T21:20:00Z");
        assert_eq!(snaps[0].encrypted, Some(true));
        assert_eq!(snaps[0].verified.as_deref(), Some("ok"));
        assert_eq!(snaps[0].comment.as_deref(), Some("nestegg ok"));
        assert_eq!(snaps[1].archives, vec!["a.pxar.didx".to_string()]);
        assert_eq!(snaps[1].encrypted, None);
    }

    #[test]
    fn group_list_parsing() {
        let json = r#"[{"backup-count":4,"backup-id":"e2e-desktop","backup-type":"host","files":[],"last-backup":1794165464,"owner":"x"}]"#;
        let g = parse_group_list(json).unwrap();
        assert_eq!(g[0].backup_id, "e2e-desktop");
        assert_eq!(g[0].last_backup, Some(1_794_165_464));
        assert_eq!(g[0].backup_count, Some(4));
    }

    #[test]
    fn archive_size_lookup() {
        let json = r#"[{"filename":"home.pxar.didx","crypt-mode":"encrypt","size":1000},
                       {"filename":"data.mpxar.didx","size":10},{"filename":"data.ppxar.didx","size":90}]"#;
        assert_eq!(size_for_archive(json, "home.pxar"), Some(1000));
        assert_eq!(size_for_archive(json, "data.pxar"), Some(100));
        assert_eq!(size_for_archive(json, "nope.pxar"), None);
    }

    #[test]
    fn crypt_mode_lookup() {
        let json = r#"[
          {"filename":"home.pxar.didx","crypt-mode":"encrypt","size":1},
          {"filename":"old.pxar.didx","crypt-mode":"none","size":1},
          {"filename":"data.mpxar.didx","crypt-mode":"encrypt","size":1},
          {"filename":"data.ppxar.didx","crypt-mode":"encrypt","size":1},
          {"filename":"index.json.blob","crypt-mode":"sign-only","size":1}
        ]"#;
        assert_eq!(
            crypt_mode_for_archive(json, "home.pxar").as_deref(),
            Some("encrypt")
        );
        assert_eq!(
            crypt_mode_for_archive(json, "old.pxar").as_deref(),
            Some("none")
        );
        assert_eq!(
            crypt_mode_for_archive(json, "data.pxar").as_deref(),
            Some("encrypt")
        );
        assert_eq!(crypt_mode_for_archive(json, "missing.pxar"), None);
        // "home" must not match "homework.pxar.didx".
        assert_eq!(
            crypt_mode_for_archive(
                r#"[{"filename":"homework.pxar.didx","crypt-mode":"none"}]"#,
                "home.pxar"
            ),
            None
        );
    }

    #[test]
    fn auto_key_decision_from_one_listing() {
        let enc = r#"[{"filename":"home.pxar.didx","crypt-mode":"encrypt","size":10}]"#;
        assert_eq!(auto_key_decision(enc, "home.pxar"), AutoKey::Key);
        let plain = r#"[{"filename":"home.pxar.didx","crypt-mode":"none","size":10}]"#;
        assert_eq!(auto_key_decision(plain, "home.pxar"), AutoKey::None);
        // An older server lists the archive with a size but no crypt mode:
        // present, read without a key (not "missing"). This is the case that
        // made a read-back/restore fail with "archive not found" (28d0133).
        let old = r#"[{"filename":"nestegg-meta.conf.blob","size":300}]"#;
        assert_eq!(auto_key_decision(old, "nestegg-meta.conf"), AutoKey::None);
        // Genuinely absent.
        assert_eq!(auto_key_decision(enc, "etc.pxar"), AutoKey::Missing);
        assert_eq!(auto_key_decision("[]", "home.pxar"), AutoKey::Missing);
    }

    #[test]
    fn archive_protection_separates_unlisted_from_unprotected() {
        let json = r#"[
          {"filename":"home.pxar.didx","crypt-mode":"encrypt","size":10},
          {"filename":"etc.pxar.didx","crypt-mode":"none","size":10},
          {"filename":"old.pxar.didx","size":10}
        ]"#;
        assert_eq!(archive_protection(json, "home.pxar"), Protection::Protected);
        assert_eq!(
            archive_protection(json, "etc.pxar"),
            Protection::Unprotected
        );
        assert_eq!(
            archive_protection(json, "old.pxar"),
            Protection::ModeUnknown
        );
        assert_eq!(archive_protection(json, "data.pxar"), Protection::NotListed);
    }

    #[test]
    fn transport_failures_are_recognised_narrowly() {
        assert!(is_transport_failure(
            "Error: error trying to connect: tcp connect error: Connection refused (os error 111)"
        ));
        assert!(is_transport_failure(
            "Error: client error (Connect): Connection timed out (os error 110)"
        ));
        // A bare "timed out" (a server task, a message from the server) is not.
        // Only the client's error line decides, not what it printed before.
        assert!(
            bounded_failure(b"connection reset by peer while reading x\nError: manifest mismatch")
                .downcast_ref::<Unreachable>()
                .is_none()
        );
        assert!(
            bounded_failure(b"some notice\nError: error trying to connect: tcp connect error")
                .downcast_ref::<Unreachable>()
                .is_some()
        );
        assert!(!is_transport_failure("Error: task timed out"));
        assert!(!is_transport_failure("Error: manifest mismatch"));
        assert!(
            bounded_failure(b"Error: no route to host")
                .downcast_ref::<Unreachable>()
                .is_some()
        );
        assert!(
            bounded_failure(b"Error: task timed out")
                .downcast_ref::<Unreachable>()
                .is_none()
        );
    }

    #[test]
    fn run_capturing_times_out_and_kills_the_client() {
        // A client that never returns is killed at the deadline, so a sleeping
        // server cannot make the read-back hang (decision 0015).
        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        let start = std::time::Instant::now();
        let cap = run_capturing(&mut cmd, Some(std::time::Duration::from_millis(200))).unwrap();
        assert_eq!(cap.stopped, Stopped::TimedOut);
        assert!(!cap.status.success());
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn run_streaming_times_out_and_kills_the_client() {
        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        let mut log = Vec::new();
        let start = std::time::Instant::now();
        let (status, _tail, stopped) = run_streaming_cancellable(
            &mut cmd,
            &mut log,
            |_| {},
            Some(std::time::Duration::from_millis(200)),
        )
        .unwrap();
        assert_eq!(stopped, Stopped::TimedOut);
        assert!(!status.success());
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn unsigned_manifest_error_is_recognised() {
        assert!(manifest_unsigned_error(
            "Error: manifest has no signature, but an encryption key is set - refusing to trust \
             a potentially tampered, unsigned backup"
        ));
        // A wrong-key failure is not "unsigned": that is a real error, not a
        // fall-back-to-the-local-record case.
        assert!(!manifest_unsigned_error(
            "Error: wrong key - unable to verify signature since manifest's key aa does not \
             match provided key bb"
        ));
        assert!(!manifest_unsigned_error("Error: connection refused"));
    }

    #[test]
    fn trailing_newline_is_trimmed_from_secrets() {
        let s = trim_secret(Secret::new(b"abc\n".to_vec()));
        assert_eq!(s.as_bytes(), b"abc");
    }

    #[test]
    fn key_show_output_is_validated_for_import() {
        let plain = local::parse_key_show(
            br#"{"path":"/tmp/k.json","kdf":"none","created":1,"modified":1,
                 "fingerprint":"aa:bb:cc","hint":null}"#,
        )
        .unwrap();
        assert_eq!(plain.fingerprint, "aa:bb:cc");
        plain.require_unprotected().unwrap();

        let protected =
            local::parse_key_show(br#"{"kdf":"Scrypt","fingerprint":"aa:bb:cc"}"#).unwrap();
        let err = protected.require_unprotected().unwrap_err().to_string();
        assert!(err.contains("protected with a password"), "{err}");
        assert!(err.contains("--kdf none"), "{err}");

        // Older clients without a kdf field are accepted.
        local::parse_key_show(br#"{"fingerprint":"aa"}"#)
            .unwrap()
            .require_unprotected()
            .unwrap();
        assert!(local::parse_key_show(br#"{"kdf":"none"}"#).is_err());
        assert!(local::parse_key_show(br#"{"kdf":"none","fingerprint":""}"#).is_err());
        assert!(local::parse_key_show(b"not json").is_err());
    }
}
