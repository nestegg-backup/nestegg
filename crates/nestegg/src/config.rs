//! Configuration file (`/etc/nestegg/config.toml`) and well-known paths.
//!
//! The configuration never contains secrets. Passwords and encryption keys
//! live in the credentials directory (see [`crate::secrets`]).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// File system locations. Overridable through environment variables so the
/// whole pipeline can run unprivileged in tests.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    pub credentials_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let env_path = |var: &str, default: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(default))
        };
        Paths {
            config_file: env_path("NESTEGG_CONFIG", "/etc/nestegg/config.toml"),
            credentials_dir: env_path("NESTEGG_CREDENTIALS_DIR", "/etc/nestegg/credentials"),
            state_dir: env_path("NESTEGG_STATE_DIR", "/var/lib/nestegg"),
            runtime_dir: env_path("NESTEGG_RUNTIME_DIR", "/run/nestegg"),
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("api.sock")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.runtime_dir.join("run.lock")
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.state_dir.join("runs")
    }

    pub fn inventory_dir(&self) -> PathBuf {
        self.state_dir.join("inventory")
    }

    pub fn current_run_file(&self) -> PathBuf {
        self.state_dir.join("current.json")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.runtime_dir.join("tmp")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Backup ID on the PBS. Defaults to the host name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_id: Option<String>,
    /// Dashboard turns to "overdue" when the last success is older than this.
    #[serde(default = "default_overdue_hours")]
    pub overdue_after_hours: u32,
    pub pbs: PbsConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<WakeConfig>,
    #[serde(default)]
    pub backup: BackupConfig,
    #[serde(default)]
    pub schedule: ScheduleConfig,
    /// Optional read-only credentials for the overview of all machines in
    /// the namespace (role DatastoreAudit or DatastoreReader). Without it,
    /// the overview only shows the machine's own backups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overview: Option<OverviewConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OverviewConfig {
    /// `user@realm!token` with read-only access to the namespace.
    pub auth_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PbsConfig {
    /// Host name or IP address of the Proxmox Backup Server.
    pub server: String,
    #[serde(default = "default_pbs_port")]
    pub port: u16,
    pub datastore: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// `user@realm` or API token `user@realm!tokenname`.
    pub auth_id: String,
    /// SHA-256 fingerprint of the server certificate, if it is not signed by
    /// a trusted CA. Not a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WakeConfig {
    /// MAC address of the PBS network interface.
    pub mac: String,
    #[serde(default = "default_broadcast")]
    pub broadcast: Ipv4Addr,
    #[serde(default = "default_wol_port")]
    pub port: u16,
    /// How long to wait for the PBS after sending the magic packet.
    #[serde(default = "default_wake_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// Archive name without extension (`home` becomes `home.pxar`).
    pub archive: String,
    pub path: PathBuf,
    /// Skip this source silently if the path does not exist. By default a
    /// missing source fails the run.
    #[serde(default)]
    pub optional: bool,
    /// The path must be a mounted file system (protects against backing up
    /// the empty mount point of a disk that failed to mount).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub require_mount: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChangeDetectionMode {
    Legacy,
    Data,
    Metadata,
}

impl ChangeDetectionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeDetectionMode::Legacy => "legacy",
            ChangeDetectionMode::Data => "data",
            ChangeDetectionMode::Metadata => "metadata",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    #[serde(default = "default_sources")]
    pub sources: Vec<Source>,
    /// Patterns passed to `proxmox-backup-client --exclude` (gitignore-like).
    #[serde(default = "default_excludes")]
    pub excludes: Vec<String>,
    /// Additional mount points to include (the client does not cross mount
    /// points by default).
    #[serde(default)]
    pub include_dev: Vec<PathBuf>,
    #[serde(default = "default_true")]
    pub encryption: bool,
    /// Add the generated system inventory as `inventory.pxar`.
    #[serde(default = "default_true")]
    pub inventory: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_detection_mode: Option<ChangeDetectionMode>,
    /// Upload rate limit, e.g. `10MiB` (passed to `--rate`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<String>,
    /// Number of run records kept locally.
    #[serde(default = "default_keep_runs")]
    pub keep_runs: usize,
}

impl Default for BackupConfig {
    fn default() -> Self {
        BackupConfig {
            sources: default_sources(),
            excludes: default_excludes(),
            include_dev: Vec::new(),
            encryption: true,
            inventory: true,
            change_detection_mode: None,
            rate_limit: None,
            keep_runs: default_keep_runs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfig {
    /// The systemd timer fires hourly; a run only starts when the last
    /// success is older than this. Retries therefore happen automatically
    /// when the PBS was unreachable or the machine was off.
    #[serde(default = "default_interval_hours")]
    pub interval_hours: u32,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        ScheduleConfig {
            interval_hours: default_interval_hours(),
        }
    }
}

fn default_overdue_hours() -> u32 {
    36
}
fn default_pbs_port() -> u16 {
    8007
}
fn default_broadcast() -> Ipv4Addr {
    Ipv4Addr::BROADCAST
}
fn default_wol_port() -> u16 {
    9
}
fn default_wake_timeout() -> u64 {
    180
}
fn default_true() -> bool {
    true
}
fn default_keep_runs() -> usize {
    200
}
fn default_interval_hours() -> u32 {
    24
}

pub fn default_sources() -> Vec<Source> {
    [
        ("home", "/home", false),
        ("etc", "/etc", false),
        ("root", "/root", true),
        ("usr-local", "/usr/local", true),
    ]
    .into_iter()
    .map(|(archive, path, optional)| Source {
        archive: archive.to_string(),
        path: PathBuf::from(path),
        optional,
        require_mount: false,
    })
    .collect()
}

pub fn default_excludes() -> Vec<String> {
    [
        "**/.cache",
        "**/.local/share/Trash",
        "**/.var/app/*/cache",
        "**/.local/share/Steam/steamapps",
        "**/node_modules",
        "**/.venv",
        "**/__pycache__",
        "**/target/debug",
        "**/target/release",
        "**/*.tmp",
        "**/.thumbnails",
        "**/.mozilla/firefox/*/cache2",
        "**/.config/google-chrome/*/Cache",
        "**/.config/chromium/*/Cache",
        "**/lost+found",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read configuration {}", path.display()))?;
        Config::parse(&text).with_context(|| format!("invalid configuration {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn validate(&self) -> Result<()> {
        if self.pbs.server.trim().is_empty() {
            bail!("pbs.server must not be empty");
        }
        if self.pbs.server.contains(':') && !self.pbs.server.starts_with('[') {
            bail!(
                "pbs.server must not contain a port; use pbs.port (IPv6 addresses need [brackets])"
            );
        }
        if !is_valid_name(&self.pbs.datastore) {
            bail!("pbs.datastore contains invalid characters");
        }
        if let Some(ns) = &self.pbs.namespace
            && (ns.is_empty() || ns.split('/').any(|part| !is_valid_name(part)))
        {
            bail!("pbs.namespace must be a slash-separated list of names");
        }
        if !self.pbs.auth_id.contains('@') {
            bail!("pbs.auth_id must look like user@realm or user@realm!token");
        }
        if let Some(id) = &self.backup_id
            && !is_valid_name(id)
        {
            bail!("backup_id may only contain letters, digits, '-', '_' and '.'");
        }
        if self.backup.sources.is_empty() {
            bail!("backup.sources must not be empty");
        }
        let mut seen = std::collections::HashSet::new();
        for source in &self.backup.sources {
            if !is_valid_archive_name(&source.archive) {
                bail!(
                    "archive name '{}' may only contain letters, digits, '-' and '_'",
                    source.archive
                );
            }
            if source.archive == "inventory" {
                bail!("archive name 'inventory' is reserved");
            }
            if !seen.insert(&source.archive) {
                bail!("archive name '{}' is used twice", source.archive);
            }
            if !source.path.is_absolute()
                || source
                    .path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                bail!(
                    "source path {} must be absolute and must not contain '..'",
                    source.path.display()
                );
            }
        }
        if let Some(wake) = &self.wake {
            parse_mac(&wake.mac).context("wake.mac")?;
        }
        if let Some(o) = &self.overview
            && !o.auth_id.contains('@')
        {
            bail!("overview.auth_id must look like user@realm!token");
        }
        if self.schedule.interval_hours == 0 {
            bail!("schedule.interval_hours must be at least 1");
        }
        Ok(())
    }

    /// Backup ID, falling back to the host name.
    pub fn effective_backup_id(&self) -> String {
        self.backup_id.clone().unwrap_or_else(hostname)
    }

    /// Repository string for `proxmox-backup-client`
    /// (`[[auth_id@]server[:port]:]datastore`).
    pub fn repository(&self) -> String {
        format!(
            "{}@{}:{}:{}",
            self.pbs.auth_id, self.pbs.server, self.pbs.port, self.pbs.datastore
        )
    }
}

fn is_valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn is_valid_archive_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

pub fn parse_mac(mac: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = mac.split([':', '-']).collect();
    if parts.len() != 6 {
        bail!("'{mac}' is not a MAC address");
    }
    let mut out = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        if part.len() != 2 {
            bail!("'{mac}' is not a MAC address");
        }
        out[i] = u8::from_str_radix(part, 16)
            .with_context(|| format!("'{mac}' is not a MAC address"))?;
    }
    Ok(out)
}

pub fn hostname() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|h| h.split('.').next().unwrap_or("").to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

/// Example configuration written by `nestegg init`.
pub fn example(server: &str, datastore: &str, auth_id: &str) -> Config {
    Config {
        backup_id: None,
        overdue_after_hours: default_overdue_hours(),
        pbs: PbsConfig {
            server: server.to_string(),
            port: default_pbs_port(),
            datastore: datastore.to_string(),
            namespace: None,
            auth_id: auth_id.to_string(),
            fingerprint: None,
        },
        wake: None,
        backup: BackupConfig::default(),
        schedule: ScheduleConfig::default(),
        overview: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[pbs]
server = "pbs.example.com"
datastore = "backups"
auth_id = "backup@pbs!desktop"
"#;

    #[test]
    fn minimal_config_gets_defaults() {
        let c = Config::parse(MINIMAL).unwrap();
        assert_eq!(c.pbs.port, 8007);
        assert!(c.backup.encryption);
        assert!(c.backup.inventory);
        assert_eq!(c.backup.sources.len(), 4);
        assert_eq!(c.schedule.interval_hours, 24);
        assert_eq!(
            c.repository(),
            "backup@pbs!desktop@pbs.example.com:8007:backups"
        );
    }

    #[test]
    fn roundtrip() {
        let c = example("pbs.example.com", "backups", "backup@pbs!desktop");
        let text = c.to_toml().unwrap();
        assert_eq!(Config::parse(&text).unwrap(), c);
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = format!("{MINIMAL}\npassword = \"x\"\n");
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn rejects_port_in_server() {
        let text = MINIMAL.replace("pbs.example.com", "pbs.example.com:8007");
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn rejects_bad_archive_and_duplicates() {
        let bad = format!(
            "{MINIMAL}\n[backup]\nsources = [{{ archive = \"my home\", path = \"/home\" }}]\n"
        );
        assert!(Config::parse(&bad).is_err());
        let dup = format!(
            "{MINIMAL}\n[backup]\nsources = [{{ archive = \"a\", path = \"/a\" }}, {{ archive = \"a\", path = \"/b\" }}]\n"
        );
        assert!(Config::parse(&dup).is_err());
        let reserved = format!(
            "{MINIMAL}\n[backup]\nsources = [{{ archive = \"inventory\", path = \"/a\" }}]\n"
        );
        assert!(Config::parse(&reserved).is_err());
        let relative =
            format!("{MINIMAL}\n[backup]\nsources = [{{ archive = \"a\", path = \"home\" }}]\n");
        assert!(Config::parse(&relative).is_err());
    }

    #[test]
    fn wake_mac_is_validated() {
        let ok = format!("{MINIMAL}\n[wake]\nmac = \"00:00:5e:00:53:01\"\n");
        let c = Config::parse(&ok).unwrap();
        assert_eq!(c.wake.unwrap().broadcast, Ipv4Addr::BROADCAST);
        let bad = format!("{MINIMAL}\n[wake]\nmac = \"00:00:5e:00:53\"\n");
        assert!(Config::parse(&bad).is_err());
    }

    #[test]
    fn mac_parsing() {
        assert_eq!(
            parse_mac("00-00-5E-00-53-01").unwrap(),
            [0, 0, 0x5e, 0, 0x53, 1]
        );
        assert!(parse_mac("zz:00:5e:00:53:01").is_err());
    }

    #[test]
    fn namespace_validation() {
        let ok = MINIMAL.replace(
            "datastore = \"backups\"",
            "datastore = \"backups\"\nnamespace = \"home/desktops\"",
        );
        assert!(Config::parse(&ok).is_ok());
        let bad = MINIMAL.replace(
            "datastore = \"backups\"",
            "datastore = \"backups\"\nnamespace = \"a//b\"",
        );
        assert!(Config::parse(&bad).is_err());
    }
}
