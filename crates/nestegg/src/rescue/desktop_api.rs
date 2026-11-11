//! Single-process desktop stick worker. Mount namespaces are never changed in
//! the multi-threaded daemon. Only the registered target and configured key
//! are used; API callers cannot supply an image, identity or credential path.
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::Client;
use crate::config::{Config, Paths};
use crate::restore::{Requester, RestoreError};
use crate::secrets::SecretStore;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub archive: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    List,
    Browse {
        time: String,
        archive: String,
        path: String,
    },
    Validate {
        time: String,
        selections: Vec<Selection>,
        target: PathBuf,
    },
    Restore {
        time: String,
        selections: Vec<Selection>,
        target: PathBuf,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub uuid: Option<String>,
    /// Filled by the daemon from the Unix socket peer, never from HTTP input.
    pub uid: u32,
    pub action: Action,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    RestoreError::BadRequest(message.into()).into()
}
fn forbidden(message: impl Into<String>) -> anyhow::Error {
    RestoreError::Forbidden(message.into()).into()
}

fn archive_name(name: &str) -> Result<()> {
    ensure!(
        name.strip_suffix(".pxar").is_some_and(|n| !n.is_empty()
            && n.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))),
        invalid("invalid archive name")
    );
    Ok(())
}

fn snapshot(config: &Config, time: &str) -> Result<String> {
    let time = chrono::DateTime::parse_from_rfc3339(time)
        .map_err(|_| invalid("invalid snapshot time"))?
        .with_timezone(&chrono::Utc);
    Ok(crate::client::snapshot_path(
        &config.effective_backup_id(),
        time.timestamp(),
    ))
}

fn scope(
    client: &Client<'_>,
    snapshot: &str,
    archive: &str,
    requester: &Requester,
    scratch: &Path,
) -> Result<Option<crate::restore::UserScope>> {
    let Requester::User(user) = requester else {
        return Ok(None);
    };
    let fetched = crate::metadata::fetch(client, snapshot, scratch)
        .map_err(|e| {
            if crate::client::manifest_unsigned_error(&format!("{e:#}")) {
                forbidden("snapshot metadata is not signed with the configured key")
            } else {
                e
            }
        })?
        .ok_or_else(|| {
            forbidden(
                "this snapshot has no signed source metadata; ask an administrator to restore it",
            )
        })?;
    let meta = crate::metadata::BackupMetadata::parse(&fetched.bytes)
        .map_err(|_| forbidden("invalid snapshot source metadata"))?;
    ensure!(
        meta.belongs_to(snapshot),
        forbidden("metadata belongs to a different snapshot")
    );
    let source = meta
        .source_of(archive)
        .ok_or_else(|| forbidden("archive is absent from snapshot metadata"))?;
    Ok(Some(
        crate::restore::user_scope(Path::new(source), &user.home, user.uid)
            .ok_or_else(|| forbidden("this archive does not contain your home directory"))?,
    ))
}

fn validate(
    client: &Client<'_>,
    snapshot: &str,
    mut selections: Vec<Selection>,
    target: &Path,
    requester: &Requester,
    scratch: &Path,
) -> Result<Vec<Selection>> {
    ensure!(
        !selections.is_empty(),
        invalid("at least one archive is required")
    );
    ensure!(target.is_absolute(), invalid("target_dir must be absolute"));
    crate::restore::absolute_target(target)?;
    let exists = match requester {
        Requester::User(u) => crate::identity::exists_as(u, target)?,
        Requester::Root { .. } => fs::symlink_metadata(target).is_ok(),
    };
    if exists {
        return Err(RestoreError::TargetExists(target.into()).into());
    }
    if selections.len() > 1 && matches!(requester, Requester::Root { .. }) {
        ensure!(
            crate::restore::root_controlled_parent(target)?,
            forbidden("restore several archives only into a root-owned location")
        );
    }
    let crate::client::RestoreKey::File(key) = client.configured_key()? else {
        return Err(invalid(
            "stick restores require the configured encryption key",
        ));
    };
    let manifest = client.read_manifest_with_key(snapshot, &key)?;
    let archives: Vec<_> = selections.iter().map(|s| s.archive.as_str()).collect();
    ensure!(
        super::protection_from_manifest(&manifest, &archives)? == super::Protection::Encrypted,
        forbidden("stick restores require encrypted archives signed with the configured key")
    );
    let mut seen = std::collections::HashSet::new();
    for sel in &mut selections {
        archive_name(&sel.archive)?;
        ensure!(
            seen.insert(sel.archive.clone()),
            invalid("duplicate archive")
        );
        for p in &mut sel.paths {
            *p = crate::restore::normalize_archive_path(p)?;
        }
        if let Some(scope) = scope(client, snapshot, &sel.archive, requester, scratch)? {
            if sel.paths.is_empty() || sel.paths.iter().any(|p| p == "/") {
                sel.paths = vec![scope.prefix.clone()];
            }
            ensure!(
                sel.paths.iter().all(|p| scope.contains(p)),
                forbidden("selected path is outside your home directory")
            );
        }
    }
    Ok(selections)
}

pub fn execute(paths: &Paths, request: Request) -> Result<Value> {
    ensure!(
        nix::unistd::Uid::effective().is_root(),
        "stick API worker requires root"
    );
    let mut config = Config::load(&paths.config_file)?;
    config.backup.encryption = true;
    let uuid = crate::stick::target::attached(&config, request.uuid.as_deref())?
        .context("registered stick not plugged in")?;
    let _lock =
        crate::run::RunLock::try_acquire(paths)?.context("a backup or stick restore is running")?;
    crate::stick::target::isolate_mounts()?;
    let mut session = crate::stick::target::Session::open(paths, &uuid)
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    session.start(paths)?;
    let requester = if request.uid == 0 {
        Requester::Root { sudo: None }
    } else {
        Requester::User(crate::identity::UserIdentity::from_uid(request.uid)?)
    };
    crate::secrets::create_private_dir(&paths.tmp_dir())?;
    let scratch = tempfile::Builder::new()
        .prefix("stick-api-")
        .tempdir_in(paths.tmp_dir())?;
    let client = Client::local(
        &config,
        SecretStore::new(&paths.credentials_dir),
        session.pbs(),
    );
    let result = match request.action {
        Action::List => {
            json!({ "snapshots": client.list_snapshots(&config.effective_backup_id())? })
        }
        Action::Browse {
            time,
            archive,
            path,
        } => {
            archive_name(&archive)?;
            let snapshot = snapshot(&config, &time)?;
            let path = crate::restore::normalize_archive_path(&path)?;
            let scope = scope(&client, &snapshot, &archive, &requester, scratch.path())?;
            if let Some(scope) = &scope {
                ensure!(
                    scope.contains(&path) || scope.is_ancestor(&path),
                    forbidden("you can only browse your home directory")
                );
            }
            let catalog = scratch.path().join("catalog");
            client.catalog_dump(&snapshot, &catalog)?;
            json!({ "entries": crate::catalog::list_children(&catalog, &archive, &path, scope.as_ref())? })
        }
        Action::Validate {
            time,
            selections,
            target,
        } => {
            let snapshot = snapshot(&config, &time)?;
            let selections = validate(
                &client,
                &snapshot,
                selections,
                &target,
                &requester,
                scratch.path(),
            )?;
            json!({ "selections": selections })
        }
        Action::Restore {
            time,
            selections,
            target,
        } => {
            let snapshot = snapshot(&config, &time)?;
            let selections = validate(
                &client,
                &snapshot,
                selections,
                &target,
                &requester,
                scratch.path(),
            )?;
            let multiple = selections.len() > 1;
            if multiple {
                match &requester {
                    Requester::User(u) => crate::identity::make_dir_as(u, &target)?,
                    Requester::Root { .. } => {
                        crate::secrets::create_private_dir(&target)?;
                    }
                }
            }
            let mut warnings = Vec::new();
            let mut bytes = 0;
            for sel in selections {
                let target = if multiple {
                    target.join(sel.archive.trim_end_matches(".pxar"))
                } else {
                    target.clone()
                };
                let patterns = crate::restore::patterns_for(&sel.paths);
                let progress = crate::restore::Progress::default();
                // Stick backups must be encrypted and signed with this key.
                // Passing it unconditionally verifies even a forged listing.
                let key = client.configured_key()?;
                warnings.extend(crate::restore::execute_with_client(
                    paths,
                    &client,
                    crate::restore::Job {
                        snapshot: &snapshot,
                        archive: &sel.archive,
                        target: &target,
                        patterns: &patterns,
                        key,
                        staging_dir: None,
                    },
                    &requester,
                    &progress,
                    &mut std::io::stderr(),
                )?);
                bytes += progress
                    .restored_bytes
                    .load(std::sync::atomic::Ordering::Relaxed);
            }
            json!({ "warnings": warnings, "restored_bytes": bytes })
        }
    };
    session.close()?;
    Ok(result)
}

/// Root-only stdin protocol for the daemon's child. Never accepts credentials
/// or namespace/image paths, and never writes secret material to stdout.
pub fn run() -> Result<u8> {
    let mut input = Vec::new();
    std::io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut input)?;
    ensure!(input.len() <= 1024 * 1024, "stick API request too large");
    let request: Request = serde_json::from_slice(&input)?;
    let result = execute(&Paths::from_env(), request);
    let envelope = match result {
        Ok(value) => json!({ "result": value }),
        Err(e) => {
            let code = match e.downcast_ref::<RestoreError>() {
                Some(RestoreError::Forbidden(_)) => "forbidden",
                Some(RestoreError::BadRequest(_)) => "bad_request",
                Some(RestoreError::TargetExists(_)) => "target_exists",
                None => "stick_unavailable",
            };
            json!({ "error": { "code": code, "message": super::printable_block(&format!("{e:#}")) } })
        }
    };
    serde_json::to_writer(std::io::stdout().lock(), &envelope)?;
    std::io::stdout().flush()?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_protocol_refuses_foreign_paths_and_invalid_archive_names() {
        assert!(serde_json::from_value::<Request>(json!({"uid": 0, "uuid": null, "image": "/media/foreign.squashfs", "action": {"operation":"list"}})).is_err());
        for name in ["../home.pxar", "system.pxar.didx", ".pxar", "a/b.pxar"] {
            assert!(archive_name(name).is_err());
        }
        assert!(archive_name("system.pxar").is_ok());
    }
}
