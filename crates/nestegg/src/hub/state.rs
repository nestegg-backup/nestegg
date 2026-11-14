//! A separate, stable lock inode protects read/modify/atomic-replace transactions.
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub codes: Vec<Code>,
    pub machines: Vec<Machine>,
    pub pending_joins: Vec<String>,
    pub master: Master,
    pub package: Option<Package>,
    pub pbs_groups: Vec<Value>,
    pub pbs_read_at: Option<String>,
    pub password_generation: String,
    pub admin_hash: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Master {
    pub public_key_pem: Option<String>,
    pub fingerprint: Option<String>,
    pub proven_at: Option<String>,
    pub paper_file: Option<PathBuf>,
    pub generation_started: bool,
    pub paper_written: bool,
    pub private_digest: Option<String>,
    pub paper_code_digest: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Package {
    pub version: String,
    pub architecture: String,
    pub sha256: String,
    pub file: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub struct Code {
    pub code_id: String,
    pub digest: String,
    pub note: Option<String>,
    pub created_at: String,
    pub expires_at: String,
    #[serde(default)]
    pub enrollment: Option<Enrollment>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Enrollment {
    pub attempt_digest: String,
    pub backup_id: String,
    pub machine_id: String,
}
impl Code {
    pub fn valid(&self, now: &str) -> bool {
        self.expires_at.as_str() > now
    }
    pub fn info(&self) -> Value {
        serde_json::json!({"code_id":self.code_id,"note":self.note,"created_at":self.created_at,"expires_at":self.expires_at})
    }
}

#[derive(Serialize, Deserialize)]
pub struct Machine {
    pub machine_id: String,
    pub backup_id: String,
    pub hostname: String,
    pub os: Option<String>,
    pub nestegg_version: Option<String>,
    pub joined_at: String,
    pub credential_digest: String,
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub struct Transaction {
    _lock: Flock<File>,
    dir: PathBuf,
    pub state: State,
}
impl Transaction {
    pub fn open(dir: &Path) -> Result<Self> {
        Self::open_inner(dir, false)
    }
    pub fn open_bounded(dir: &Path) -> Result<Self> {
        Self::open_inner(dir, true)
    }
    fn open_inner(dir: &Path, bounded: bool) -> Result<Self> {
        crate::secrets::create_private_dir(dir)?;
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(dir.join(".hub.lock"))?;
        let started = std::time::Instant::now();
        let mut file = f;
        let lock = loop {
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(lock) => break lock,
                Err((returned, nix::errno::Errno::EWOULDBLOCK))
                    if !bounded || started.elapsed() < std::time::Duration::from_secs(5) =>
                {
                    file = returned;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err((_, e)) => {
                    return Err(e).context("hub state lock unavailable after bounded wait");
                }
            }
        };
        let state = match std::fs::read(dir.join("state.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("read hub state")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            _lock: lock,
            dir: dir.into(),
            state,
        })
    }
    pub fn save(&self) -> Result<()> {
        atomic(
            &self.dir.join("state.json"),
            &serde_json::to_vec_pretty(&self.state)?,
        )
    }
}

pub fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file has no parent")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    let meta = std::fs::metadata(parent)?;
    use std::os::unix::fs::MetadataExt;
    if nix::unistd::Uid::effective().is_root() {
        nix::unistd::fchown(
            tmp.as_file(),
            Some(nix::unistd::Uid::from_raw(meta.uid())),
            Some(nix::unistd::Gid::from_raw(meta.gid())),
        )?;
    }
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn concurrent_updates_do_not_lose_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for n in 0..16 {
                let path = dir.path();
                scope.spawn(move || {
                    let mut t = Transaction::open(path).unwrap();
                    t.state.pbs_groups.push(serde_json::json!(n));
                    t.save().unwrap();
                });
            }
        });
        let t = Transaction::open(dir.path()).unwrap();
        assert_eq!(t.state.pbs_groups.len(), 16);
        assert_eq!(
            std::fs::metadata(dir.path().join("state.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[test]
    fn failed_transaction_does_not_publish() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut t = Transaction::open(dir.path()).unwrap();
            t.state.password_generation = "unsaved".into();
        }
        assert!(
            Transaction::open(dir.path())
                .unwrap()
                .state
                .password_generation
                .is_empty()
        );
    }
}
