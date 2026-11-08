//! `nestegg check` and `POST /api/v1/pbs/check`.

use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::client::{self, Client};
use crate::config::{Config, Paths};
use crate::secrets::{self, SecretStore};
use crate::state::Store;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CheckItem {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

pub struct CheckOptions {
    /// Wake the PBS via Wake-on-LAN if it does not answer.
    pub wake: bool,
    /// Log in and list snapshots (needs access to the secrets, i.e. root).
    pub login: bool,
}

/// Run all checks. Never fails; problems become failed items.
pub fn run(paths: &Paths, options: &CheckOptions) -> Vec<CheckItem> {
    let mut items = Vec::new();
    let mut report = |ok: bool, name: &str, detail: String| {
        items.push(CheckItem {
            name: name.to_string(),
            ok,
            detail,
        })
    };
    let cfg = match Config::load(&paths.config_file) {
        Ok(c) => {
            report(
                true,
                "configuration",
                paths.config_file.display().to_string(),
            );
            c
        }
        Err(e) => {
            report(false, "configuration", format!("{e:#}"));
            return items;
        }
    };
    match client::local::version() {
        Ok(v) => report(true, "proxmox-backup-client", v),
        Err(e) => report(false, "proxmox-backup-client", format!("{e:#}")),
    }
    let store = SecretStore::new(&paths.credentials_dir);
    if options.login {
        let pw = store.load(secrets::PASSWORD);
        report(
            pw.is_ok(),
            "password / token secret",
            pw.err().map(|e| format!("{e:#}")).unwrap_or_default(),
        );
    } else {
        report(
            store.exists(secrets::PASSWORD),
            "password / token secret",
            String::new(),
        );
    }
    if cfg.backup.encryption {
        if options.login {
            let key = store.load(secrets::ENCRYPTION_KEY);
            report(
                key.is_ok(),
                "encryption key",
                key.err().map(|e| format!("{e:#}")).unwrap_or_default(),
            );
        } else {
            report(
                store.exists(secrets::ENCRYPTION_KEY),
                "encryption key",
                String::new(),
            );
        }
        let exported = Store::new(paths)
            .load_state()
            .map(|s| s.key_exported())
            .unwrap_or(false);
        report(
            exported,
            "encryption key exported",
            if exported {
                String::new()
            } else {
                "run `nestegg key confirm-exported` after exporting".into()
            },
        );
    }
    for source in &cfg.backup.sources {
        let exists = source.path.exists();
        report(
            exists || source.optional,
            &format!("source {}", source.archive),
            format!(
                "{}{}",
                source.path.display(),
                if exists { "" } else { " (missing)" }
            ),
        );
    }
    let mut reachable = crate::wake::is_reachable(&cfg, Duration::from_secs(5));
    if !reachable && options.wake && cfg.wake.is_some() {
        reachable = matches!(
            crate::wake::ensure_available(&cfg, |_| {}),
            Ok(crate::wake::Availability::ReachableAfterWake { .. })
                | Ok(crate::wake::Availability::Reachable)
        );
    }
    report(
        reachable || cfg.wake.is_some(),
        "PBS reachable",
        if reachable {
            format!("{}:{}", cfg.pbs.server, cfg.pbs.port)
        } else if cfg.wake.is_some() {
            "not now (Wake-on-LAN configured; a run will wake it)".into()
        } else {
            "no answer".into()
        },
    );
    if reachable && options.login {
        let client = Client::new(&cfg, SecretStore::new(&paths.credentials_dir));
        match client.list_snapshots(&cfg.effective_backup_id()) {
            Ok(s) => report(
                true,
                "PBS login and datastore access",
                format!("{} snapshots", s.len()),
            ),
            Err(e) => report(false, "PBS login and datastore access", format!("{e:#}")),
        }
        if let Some(o) = &cfg.overview {
            let client = Client::with_credentials(
                &cfg,
                SecretStore::new(&paths.credentials_dir),
                &o.auth_id,
                secrets::OVERVIEW_PASSWORD,
            );
            match client.list_groups() {
                Ok(g) => report(true, "overview access", format!("{} machines", g.len())),
                Err(e) => report(false, "overview access", format!("{e:#}")),
            }
        }
    }
    report(
        crate::status::timer_enabled(),
        "backup timer enabled",
        crate::status::TIMER_UNIT.into(),
    );
    items
}

pub fn passed(items: &[CheckItem]) -> Result<bool> {
    Ok(items.iter().all(|i| i.ok))
}
