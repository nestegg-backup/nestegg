//! Single-process desktop stick worker. Mount namespaces are never changed in
//! the multi-threaded daemon. Only the registered target and configured key
//! are used; API callers cannot supply an image, identity or credential path.
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    #[error("registered stick not plugged in")]
    NotPluggedIn,
    #[error(
        "several registered sticks are attached; choose a stick in the restore source selector"
    )]
    Ambiguous,
    #[error("a backup or stick restore is running; try again afterwards")]
    Busy,
    #[error("this snapshot is not on the selected stick")]
    SnapshotNotOnStick,
    #[error("the stick cannot be read")]
    Unreadable,
    #[error("stick restores are not supported on this system")]
    Unsupported,
}
fn unavailable_reason(e: &anyhow::Error) -> UnavailableReason {
    if let Some(reason) = e.downcast_ref::<UnavailableReason>() {
        return *reason;
    }
    match e.downcast_ref::<crate::state::RunError>().map(|e| e.kind) {
        Some(crate::state::ErrorKind::StickNotPluggedIn) => UnavailableReason::NotPluggedIn,
        Some(crate::state::ErrorKind::StickAmbiguous) => UnavailableReason::Ambiguous,
        Some(crate::state::ErrorKind::StickBusy | crate::state::ErrorKind::StickAlreadyMounted) => {
            UnavailableReason::Busy
        }
        Some(crate::state::ErrorKind::StickUnsupported) => UnavailableReason::Unsupported,
        _ => UnavailableReason::Unreadable,
    }
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    RestoreError::BadRequest(message.into()).into()
}
fn forbidden(message: impl Into<String>) -> anyhow::Error {
    RestoreError::Forbidden(message.into()).into()
}

fn scope_forbidden(
    reason: crate::restore::ForbiddenReason,
    message: impl Into<String>,
) -> anyhow::Error {
    RestoreError::ScopeForbidden {
        reason,
        message: message.into(),
    }
    .into()
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
                scope_forbidden(
                    crate::restore::ForbiddenReason::RestoreSourceUnverified,
                    "snapshot metadata is not signed with the configured key",
                )
            } else {
                e
            }
        })?
        .ok_or_else(|| {
            scope_forbidden(
                crate::restore::ForbiddenReason::RestoreSourceUnverified,
                "this snapshot has no signed source metadata; ask an administrator to restore it",
            )
        })?;
    let meta = crate::metadata::BackupMetadata::parse(&fetched.bytes).map_err(|_| {
        scope_forbidden(
            crate::restore::ForbiddenReason::RestoreSourceUnverified,
            "invalid snapshot source metadata",
        )
    })?;
    ensure!(
        meta.belongs_to(snapshot),
        scope_forbidden(
            crate::restore::ForbiddenReason::RestoreSourceUnverified,
            "metadata belongs to a different snapshot"
        )
    );
    Ok(Some(metadata_scope(&meta, archive, user)?))
}

fn metadata_scope(
    meta: &crate::metadata::BackupMetadata,
    archive: &str,
    user: &crate::identity::UserIdentity,
) -> Result<crate::restore::UserScope> {
    let source = meta.source_of(archive).ok_or_else(|| {
        scope_forbidden(
            crate::restore::ForbiddenReason::RestoreNoHome,
            "this archive does not contain your home directory",
        )
    })?;
    crate::restore::scope_for_source(
        Path::new(source),
        meta.bound_sources.contains(archive),
        user,
    )
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
                scope_forbidden(
                    crate::restore::ForbiddenReason::RestoreOutsideHome,
                    "selected path is outside your home directory"
                )
            );
        }
    }
    Ok(selections)
}

/// Receives the running byte count of a restore (bytes written so far and,
/// when known, the total) from a helper thread.
pub type Reporter = std::sync::Arc<dyn Fn(u64, Option<u64>) + Send + Sync>;

/// Environment variable the daemon sets when it reads progress lines. Without
/// it the worker writes only the final envelope, as before (decision 0029).
pub const PROGRESS_ENV: &str = "NESTEGG_STICK_PROGRESS";

/// One progress line on the worker's stdout, before the final envelope.
pub fn progress_line(restored_bytes: u64, total_bytes: Option<u64>) -> String {
    json!({ "progress": { "restored_bytes": restored_bytes, "total_bytes": total_bytes } })
        .to_string()
}

/// A progress line as written by [`progress_line`]; None for anything else
/// (such as the final envelope).
pub fn parse_progress_line(line: &[u8]) -> Option<(u64, Option<u64>)> {
    let value: Value = serde_json::from_slice(line).ok()?;
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let progress = object.get("progress")?;
    let done = progress.get("restored_bytes")?.as_u64()?;
    let total = progress.get("total_bytes").and_then(Value::as_u64);
    Some((done, total))
}

/// Reports `done_before` plus the live count of the archive being restored
/// once a second. Dropping it stops the thread and waits for it, so no line
/// can follow the final envelope.
struct ProgressLines {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ProgressLines {
    fn start(
        report: Option<&Reporter>,
        done_before: u64,
        current: std::sync::Arc<std::sync::atomic::AtomicU64>,
        total: Option<u64>,
    ) -> ProgressLines {
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = report.cloned().map(|report| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    report(done_before + current.load(Ordering::Relaxed), total);
                    for _ in 0..10 {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            })
        });
        ProgressLines { stop, thread }
    }
}

impl Drop for ProgressLines {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn execute(paths: &Paths, request: Request) -> Result<Value> {
    execute_reporting(paths, request, None)
}

/// Like [`execute`]; a restore also reports its running byte count to `report`.
pub fn execute_reporting(
    paths: &Paths,
    request: Request,
    report: Option<&Reporter>,
) -> Result<Value> {
    ensure!(
        nix::unistd::Uid::effective().is_root(),
        UnavailableReason::Unsupported
    );
    let mut config = Config::load(&paths.config_file)?;
    config.backup.encryption = true;
    let registered = config
        .stick
        .as_ref()
        .ok_or(UnavailableReason::Unsupported)?;
    if let Some(uuid) = &request.uuid {
        ensure!(
            crate::stick::valid_uuid(uuid)
                && registered
                    .data_uuids
                    .iter()
                    .any(|u| u.eq_ignore_ascii_case(uuid)),
            invalid("stick UUID is not registered")
        );
    } else {
        ensure!(
            registered
                .data_uuids
                .iter()
                .filter(|u| crate::stick::target::present(u))
                .count()
                <= 1,
            UnavailableReason::Ambiguous
        );
    }
    let uuid = crate::stick::target::attached(&config, request.uuid.as_deref())?
        .ok_or(UnavailableReason::NotPluggedIn)?;
    let _lock = crate::run::RunLock::try_acquire(paths)?.ok_or(UnavailableReason::Busy)?;
    crate::stick::launcher::preflight()
        .map_err(|e| anyhow::Error::new(UnavailableReason::Unsupported).context(e))?;
    crate::stick::target::isolate_mounts()?;
    let mut session = crate::stick::target::Session::open_for_restore(paths, &uuid)?;
    session.start(paths)?;
    if let Action::Browse { time, .. }
    | Action::Validate { time, .. }
    | Action::Restore { time, .. } = &request.action
    {
        let wanted = snapshot(&config, time)?;
        let client = Client::local(
            &config,
            SecretStore::new(&paths.credentials_dir),
            session.pbs(),
        );
        ensure!(
            client
                .list_snapshots(&config.effective_backup_id())?
                .iter()
                .any(|s| s.snapshot == wanted),
            UnavailableReason::SnapshotNotOnStick
        );
    }
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
    let mut result = match request.action {
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
                    scope_forbidden(
                        crate::restore::ForbiddenReason::RestoreOutsideHome,
                        "you can only browse your home directory"
                    )
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
            // As for the backup server: a total only when whole archives are
            // restored, so a percentage never rests on a guess.
            let total = if selections.iter().all(|s| s.paths.is_empty()) {
                let mut sum = Some(0u64);
                for sel in &selections {
                    let size = client.archive_size(&snapshot, &sel.archive).ok().flatten();
                    sum = sum.zip(size).map(|(a, b)| a + b);
                }
                sum.filter(|&t| t > 0)
            } else {
                None
            };
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
                let lines =
                    ProgressLines::start(report, bytes, progress.restored_bytes.clone(), total);
                let result = crate::restore::execute_with_client(
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
                );
                drop(lines);
                warnings.extend(result?);
                bytes += progress
                    .restored_bytes
                    .load(std::sync::atomic::Ordering::Relaxed);
            }
            json!({ "warnings": warnings, "restored_bytes": bytes })
        }
    };
    session.close()?;
    result["target"] = json!("stick");
    result["stick_uuid"] = json!(uuid);
    Ok(result)
}

pub(crate) fn error_envelope(e: &anyhow::Error) -> Value {
    let code = match e.downcast_ref::<RestoreError>() {
        Some(RestoreError::Forbidden(_) | RestoreError::ScopeForbidden { .. }) => "forbidden",
        Some(RestoreError::BadRequest(_)) => "bad_request",
        Some(RestoreError::TargetExists(_)) => "target_exists",
        None => "stick_unavailable",
    };
    let reason = match e.downcast_ref::<RestoreError>() {
        Some(RestoreError::ScopeForbidden { reason, .. }) => json!(reason),
        _ if code == "stick_unavailable" => json!(unavailable_reason(e)),
        _ => Value::Null,
    };
    json!({ "error": { "code": code, "reason": reason, "message": super::printable_block(&format!("{e:#}")) } })
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
    // Progress lines only for a daemon that asked for them; an older daemon
    // expects the envelope alone on stdout.
    let report: Option<Reporter> =
        (std::env::var_os(PROGRESS_ENV).as_deref() == Some(std::ffi::OsStr::new("1"))).then(|| {
            std::sync::Arc::new(|done: u64, total: Option<u64>| {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{}", progress_line(done, total));
                let _ = out.flush();
            }) as Reporter
        });
    let result = execute_reporting(&Paths::from_env(), request, report.as_ref());
    let envelope = match result {
        Ok(value) => json!({ "result": value }),
        Err(e) => error_envelope(&e),
    };
    serde_json::to_writer(std::io::stdout().lock(), &envelope)?;
    std::io::stdout().flush()?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn metadata() -> crate::metadata::BackupMetadata {
        crate::metadata::BackupMetadata::parse(
            &serde_json::to_vec(&json!({
                "format": 1,
                "nestegg_version": "test",
                "backup_id": "desktop",
                "hostname": "desktop.example.com",
                "backup_time": 0,
                "snapshot": "host/desktop/1970-01-01T00:00:00Z",
                "archive_sources": {},
                "sources": [],
                "system_mode": false
            }))
            .unwrap(),
        )
        .unwrap()
    }

    fn user(home: &Path) -> crate::identity::UserIdentity {
        crate::identity::UserIdentity {
            uid: nix::unistd::geteuid().as_raw(),
            gid: nix::unistd::getegid().as_raw(),
            groups: vec![],
            home: home.into(),
        }
    }

    #[test]
    fn archive_absent_from_stick_metadata_has_the_same_reason_as_pbs() {
        let error =
            metadata_scope(&metadata(), "docs.pxar", &user(Path::new("/home/alice"))).unwrap_err();
        let envelope = error_envelope(&error);
        assert_eq!(envelope["error"]["code"], "forbidden");
        assert_eq!(envelope["error"]["reason"], "restore_no_home");
    }

    #[test]
    fn stick_metadata_without_binding_cannot_grant_a_below_home_source() {
        let home = tempfile::tempdir().unwrap();
        let user = user(home.path());
        // The source need not exist on this computer at restore time.
        let source = home.path().join("Documents");
        let mut meta = metadata();
        meta.archive_sources
            .insert("docs.pxar".into(), source.to_str().unwrap().into());
        assert!(meta.bound_sources.is_empty());
        let error = metadata_scope(&meta, "docs.pxar", &user).unwrap_err();
        let envelope = error_envelope(&error);
        assert_eq!(envelope["error"]["code"], "forbidden");
        assert_eq!(envelope["error"]["reason"], "restore_source_unverified");
        // A binding for another archive must not authorize this one either.
        meta.bound_sources.insert("home.pxar".into());
        assert!(metadata_scope(&meta, "docs.pxar", &user).is_err());
        // With this archive bound, the same source grants the archive root.
        meta.bound_sources.insert("docs.pxar".into());
        assert_eq!(
            metadata_scope(&meta, "docs.pxar", &user).unwrap().prefix,
            "/"
        );
    }

    #[test]
    fn scope_refusals_have_stable_worker_reasons() {
        use crate::restore::ForbiddenReason::*;
        for (reason, code) in [
            (RestoreNoHome, "restore_no_home"),
            (RestoreSourceUnverified, "restore_source_unverified"),
            (RestoreOutsideHome, "restore_outside_home"),
        ] {
            let error = scope_forbidden(reason, "wording can change");
            let envelope = error_envelope(&error);
            assert_eq!(envelope["error"]["code"], "forbidden");
            assert_eq!(envelope["error"]["reason"], code);
        }
    }

    #[test]
    fn unavailable_reasons_are_stable_and_preserve_typed_causes() {
        for (kind, expected) in [
            (
                crate::state::ErrorKind::StickNotPluggedIn,
                UnavailableReason::NotPluggedIn,
            ),
            (crate::state::ErrorKind::StickBusy, UnavailableReason::Busy),
            (
                crate::state::ErrorKind::StickAlreadyMounted,
                UnavailableReason::Busy,
            ),
            (
                crate::state::ErrorKind::StickAmbiguous,
                UnavailableReason::Ambiguous,
            ),
            (
                crate::state::ErrorKind::StickUnsupported,
                UnavailableReason::Unsupported,
            ),
            (
                crate::state::ErrorKind::FilesystemDamaged,
                UnavailableReason::Unreadable,
            ),
        ] {
            let error = anyhow::Error::new(crate::stick::target::error(kind, "fixture"))
                .context("opening stick");
            assert_eq!(unavailable_reason(&error), expected);
        }
        for reason in [
            UnavailableReason::NotPluggedIn,
            UnavailableReason::Ambiguous,
            UnavailableReason::Busy,
            UnavailableReason::SnapshotNotOnStick,
            UnavailableReason::Unreadable,
            UnavailableReason::Unsupported,
        ] {
            assert_eq!(
                unavailable_reason(&anyhow::Error::new(reason).context("fixture")),
                reason
            );
            assert_eq!(
                serde_json::from_value::<UnavailableReason>(serde_json::to_value(reason).unwrap())
                    .unwrap(),
                reason
            );
        }
    }

    #[test]
    fn worker_protocol_refuses_foreign_paths_and_invalid_archive_names() {
        assert!(serde_json::from_value::<Request>(json!({"uid": 0, "uuid": null, "image": "/media/foreign.squashfs", "action": {"operation":"list"}})).is_err());
        for name in ["../home.pxar", "system.pxar.didx", ".pxar", "a/b.pxar"] {
            assert!(archive_name(name).is_err());
        }
        assert!(archive_name("system.pxar").is_ok());
    }
}
