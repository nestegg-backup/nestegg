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
}

/// Arguments after `proxmox-backup-client` for a backup (no secrets).
pub fn backup_args(spec: &BackupSpec) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["backup".into()];
    for archive in &spec.archives {
        let mut a = OsString::from(format!("{}:", archive.file_name()));
        a.push(archive.path.as_os_str());
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
        let crypt_modes: Vec<&str> = files
            .iter()
            .filter_map(|f| f["crypt-mode"].as_str())
            .collect();
        let encrypted = if crypt_modes.is_empty() {
            None
        } else {
            Some(crypt_modes.contains(&"encrypt"))
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
        });
    }
    out.sort_by(|a, b| b.time.cmp(&a.time));
    Ok(out)
}

pub struct BackupOutcome {
    pub archives: Vec<ArchiveStats>,
}

pub struct Client<'a> {
    config: &'a Config,
    secrets: SecretStore,
}

impl<'a> Client<'a> {
    pub fn new(config: &'a Config, secrets: SecretStore) -> Client<'a> {
        Client { config, secrets }
    }

    /// Base command with repository, fingerprint and password descriptor.
    fn command(&self, fds: &mut FdPassing) -> Result<Command> {
        let mut cmd = Command::new(client_binary());
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
        cmd.env("PBS_REPOSITORY", self.config.repository());
        if let Some(fp) = &self.config.pbs.fingerprint {
            cmd.env("PBS_FINGERPRINT", fp);
        }
        let password = self.secrets.load(secrets::PASSWORD)?;
        let fd = fds.add(&trim_secret(password), PASSWORD_FD)?;
        cmd.env("PBS_PASSWORD_FD", fd.to_string());
        cmd.stdin(Stdio::null());
        Ok(cmd)
    }

    fn add_namespace(&self, cmd: &mut Command) {
        if let Some(ns) = &self.config.pbs.namespace {
            cmd.args(["--ns", ns]);
        }
    }

    /// Run a backup, streaming all client output into `log`.
    pub fn backup(
        &self,
        spec: &BackupSpec,
        log: &mut dyn Write,
    ) -> std::result::Result<BackupOutcome, RunError> {
        let internal = |e: anyhow::Error| RunError {
            kind: ErrorKind::Internal,
            message: format!("{e:#}"),
        };
        let mut fds = FdPassing::new();
        let mut cmd = self.command(&mut fds).map_err(|e| RunError {
            kind: ErrorKind::Config,
            message: format!("{e:#}"),
        })?;
        if spec.encrypt {
            let key = self
                .secrets
                .load(secrets::ENCRYPTION_KEY)
                .map_err(|e| RunError {
                    kind: ErrorKind::Config,
                    message: format!("{e:#}"),
                })?;
            fds.add(&key, KEY_FD).map_err(internal)?;
        }
        cmd.args(backup_args(spec));
        fds.apply(&mut cmd);
        let mut stats = Vec::new();
        let (status, tail) = run_streaming(&mut cmd, log, |line| {
            if let Some(s) = parse_archive_stats(line) {
                stats.push(s);
            }
        })
        .map_err(|e| RunError {
            kind: ErrorKind::ClientMissing,
            message: format!("{e:#}"),
        })?;
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

    /// Restore an archive (or the paths matching `patterns`) into `target`.
    pub fn restore(
        &self,
        snapshot: &str,
        archive: &str,
        target: &Path,
        patterns: &[String],
        log: &mut dyn Write,
    ) -> Result<()> {
        let mut fds = FdPassing::new();
        let mut cmd = self.command(&mut fds)?;
        cmd.args(["restore", snapshot, archive]).arg(target);
        for p in patterns {
            cmd.args(["--pattern", p]);
        }
        if self.secrets.exists(secrets::ENCRYPTION_KEY) {
            let key = self.secrets.load(secrets::ENCRYPTION_KEY)?;
            fds.add(&key, KEY_FD)?;
            cmd.args(["--keyfd", &KEY_FD.to_string()]);
        }
        self.add_namespace(&mut cmd);
        fds.apply(&mut cmd);
        let (status, tail) = run_streaming(&mut cmd, log, |_| {})?;
        if !status.success() {
            bail!(
                "{}",
                last_error_line(&tail).unwrap_or_else(|| format!("restore failed with {status}"))
            );
        }
        Ok(())
    }
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
        let v: Value =
            serde_json::from_slice(&output.stdout).context("unexpected key show output")?;
        v["fingerprint"]
            .as_str()
            .map(String::from)
            .context("key show did not report a fingerprint")
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

/// Run a command, forwarding stdout and stderr line by line into `log` and
/// to `on_line`. Returns the exit status and the last 40 lines.
fn run_streaming(
    cmd: &mut Command,
    log: &mut dyn Write,
    mut on_line: impl FnMut(&str),
) -> Result<(std::process::ExitStatus, Vec<String>)> {
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
    let mut tail: Vec<String> = Vec::new();
    for line in rx {
        let _ = writeln!(log, "{line}");
        on_line(&line);
        tail.push(line);
        if tail.len() > 40 {
            tail.remove(0);
        }
    }
    for r in readers {
        let _ = r.join();
    }
    let status = child.wait()?;
    let _ = log.flush();
    Ok((status, tail))
}

pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(&args[..3], ["backup", "home.pxar:/home", "etc.pxar:/etc"]);
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
    fn trailing_newline_is_trimmed_from_secrets() {
        let s = trim_secret(Secret::new(b"abc\n".to_vec()));
        assert_eq!(s.as_bytes(), b"abc");
    }
}
