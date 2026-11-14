//! `nestegg check` and `POST /api/v1/pbs/check`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::client::{self, Client};
use crate::config::{Config, Paths};
use crate::secrets::{self, SecretStore};
use crate::state::Store;

/// Result of one check. A check that could not be made is neither passed
/// nor failed: reporting it as passed would claim something nobody looked
/// at, reporting it as failed would blame something that may be fine.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    /// Not looked at, for example because the server is asleep and nobody
    /// asked to wake it, or because the check needs root.
    NotChecked,
}

/// Name of the item that says whether the server answered. The daemon and
/// the web UI look for it by this name.
pub const REACHABLE_ITEM: &str = "PBS reachable";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckItem {
    pub name: String,
    /// `status == passed`. Kept for clients that only know passed and
    /// failed: they show a check that was not made as a problem rather than
    /// as fine.
    pub ok: bool,
    pub status: CheckStatus,
    pub detail: String,
}

/// Overall result of a list of checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Everything was checked and passed.
    Passed,
    /// Nothing failed, but some checks could not be made.
    Incomplete,
    /// At least one check failed.
    Failed,
}

impl Outcome {
    /// Exit code of `nestegg check`: 0 passed, 1 failed, 2 incomplete.
    pub fn exit_code(self) -> u8 {
        match self {
            Outcome::Passed => 0,
            Outcome::Failed => 1,
            Outcome::Incomplete => 2,
        }
    }
}

struct Report(Vec<CheckItem>);

impl Report {
    fn add(&mut self, status: CheckStatus, name: &str, detail: String) {
        self.0.push(CheckItem {
            name: name.to_string(),
            ok: status == CheckStatus::Passed,
            status,
            detail,
        });
    }

    fn report(&mut self, ok: bool, name: &str, detail: String) {
        let status = if ok {
            CheckStatus::Passed
        } else {
            CheckStatus::Failed
        };
        self.add(status, name, detail);
    }

    fn not_checked(&mut self, name: &str, detail: &str) {
        self.add(CheckStatus::NotChecked, name, detail.to_string());
    }
}

pub struct CheckOptions {
    /// Wake the PBS via Wake-on-LAN if it does not answer.
    pub wake: bool,
    /// Log in and list snapshots (needs access to the secrets, i.e. root).
    pub login: bool,
}

const LOGIN_ITEM: &str = "PBS login and datastore access";
const OVERVIEW_ITEM: &str = "overview access";

/// Result of [`run_reporting`]: the items, and whether a Wake-on-LAN packet
/// really left this computer.
pub struct Checked {
    pub items: Vec<CheckItem>,
    /// True only when at least one magic packet was sent. False when the
    /// server answered at once, Wake-on-LAN is off or not configured, no wake
    /// was asked for, or every send failed.
    pub wake_sent: bool,
}

/// Whether a wake packet was really sent, from the event
/// [`crate::wake::WakeEvent::PacketSent`] that `wake.rs` reports for every
/// packet that went out (not inferred from the order or wording of the other
/// events).
#[derive(Default)]
struct WakeTracker {
    sent: bool,
}

impl WakeTracker {
    fn event(&mut self, event: &crate::wake::WakeEvent) {
        if matches!(event, crate::wake::WakeEvent::PacketSent) {
            self.sent = true;
        }
    }

    fn sent(&self) -> bool {
        self.sent
    }
}

/// Run all checks. Never fails; problems become failed items.
pub fn run(paths: &Paths, options: &CheckOptions) -> Vec<CheckItem> {
    run_reporting(paths, options).items
}

/// Like [`run`], and also say whether a wake-up packet was really sent (the
/// daemon's minimum interval between wake attempts counts only those).
pub fn run_reporting(paths: &Paths, options: &CheckOptions) -> Checked {
    let mut wake = WakeTracker::default();
    let items = run_items(paths, options, &mut wake);
    Checked {
        items,
        wake_sent: wake.sent(),
    }
}

fn run_items(paths: &Paths, options: &CheckOptions, tracker: &mut WakeTracker) -> Vec<CheckItem> {
    let mut r = Report(Vec::new());
    let cfg = match Config::load(&paths.config_file) {
        Ok(c) => {
            r.report(
                true,
                "configuration",
                paths.config_file.display().to_string(),
            );
            c
        }
        Err(e) => {
            r.report(false, "configuration", format!("{e:#}"));
            return r.0;
        }
    };
    match client::local::version() {
        Ok(v) => r.report(true, "proxmox-backup-client", v),
        Err(e) => r.report(false, "proxmox-backup-client", format!("{e:#}")),
    }
    let store = SecretStore::new(&paths.credentials_dir);
    // A stick-only setup has no remote server: the local PBS of the stick
    // gets a fresh token at every start, so no stored secret is needed.
    if cfg.pbs.is_some() && options.login {
        let pw = store.load(secrets::PASSWORD);
        r.report(
            pw.is_ok(),
            "password / token secret",
            pw.err().map(|e| format!("{e:#}")).unwrap_or_default(),
        );
    } else if cfg.pbs.is_some() {
        r.report(
            store.exists(secrets::PASSWORD),
            "password / token secret",
            String::new(),
        );
    }
    if cfg.backup.encryption {
        if options.login {
            let key = store.load(secrets::ENCRYPTION_KEY);
            r.report(
                key.is_ok(),
                "encryption key",
                key.err().map(|e| format!("{e:#}")).unwrap_or_default(),
            );
        } else {
            r.report(
                store.exists(secrets::ENCRYPTION_KEY),
                "encryption key",
                String::new(),
            );
        }
        let state = Store::new(paths).load_state().ok();
        let master = state
            .as_ref()
            .map(|s| crate::masterkey::covered(&cfg, s))
            .transpose();
        let master_covered = matches!(&master, Ok(Some(Some(_))));
        let master_detail = match &master {
            Ok(Some(Some(proof))) => format!(
                "protected by the proven household master key ({})",
                proof.fingerprint
            ),
            Err(e) => format!("{e:#}. "),
            _ => String::new(),
        };
        let exported = Store::new(paths)
            .load_state()
            .map(|s| {
                s.key_exported()
                    || (cfg.pbs.is_none()
                        && cfg.stick.as_ref().is_some_and(|stick| {
                            stick
                                .data_uuids
                                .iter()
                                .any(|uuid| s.stick_key_exported(uuid))
                        }))
            })
            .unwrap_or(false);
        r.report(
            exported || master_covered,
            "encryption key exported",
            if master_covered {
                master_detail
            } else if exported {
                format!("own key copy proven. {master_detail}")
            } else {
                format!("{master_detail}export the key, then run `nestegg key confirm-exported --file <copy>` (or, for a printed paper key, `--paper-code <code> --printed`)")
            },
        );
    }
    for source in &cfg.backup.sources {
        let exists = source.path.exists();
        r.report(
            exists || source.optional,
            &format!("source {}", source.archive),
            format!(
                "{}{}",
                source.path.display(),
                if exists { "" } else { " (missing)" }
            ),
        );
    }
    if cfg.stick.is_some() {
        match crate::stick::target::attached(&cfg, None) {
            Ok(Some(_)) => r.report(true, "stick", "registered stick attached".into()),
            Ok(None) => r.not_checked("stick", "registered stick not plugged in"),
            Err(e) => r.report(false, "stick", format!("{e:#}")),
        }
    }
    let Some(pbs) = &cfg.pbs else {
        return r.0;
    };
    let mut reachable = crate::wake::is_reachable(&cfg, Duration::from_secs(5));
    let wake_on = cfg.active_wake().is_some();
    let mut wake_tried = false;
    // Set when the wake-up packet could not even be sent (no network).
    let mut wake_send_problem: Option<String> = None;
    if !reachable && options.wake && wake_on {
        wake_tried = true;
        reachable = matches!(
            crate::wake::ensure_available(&cfg, |event| {
                tracker.event(&event);
                match event {
                    crate::wake::WakeEvent::SendFailed(why) => {
                        wake_send_problem = Some(why.to_string())
                    }
                    crate::wake::WakeEvent::SendRecovered => wake_send_problem = None,
                    crate::wake::WakeEvent::Info(_) | crate::wake::WakeEvent::PacketSent => {}
                }
            }),
            Ok(crate::wake::Availability::ReachableAfterWake { .. })
                | Ok(crate::wake::Availability::Reachable)
        );
    }
    let server = format!("{}:{}", pbs.server, pbs.port);
    if reachable {
        r.report(true, REACHABLE_ITEM, server);
    } else if wake_tried {
        // The caller asked to wake the server and it did not come up.
        r.report(
            false,
            REACHABLE_ITEM,
            match &wake_send_problem {
                // Say why, instead of claiming a wake-up attempt that never
                // left this computer.
                Some(why) => {
                    format!("no answer, and the Wake-on-LAN packet could not be sent: {why}")
                }
                None => "no answer, not even after the Wake-on-LAN attempt".into(),
            },
        );
    } else if wake_on {
        // Asleep or off, and nobody asked to wake it: nothing is known.
        r.not_checked(
            REACHABLE_ITEM,
            "no answer; the server may be asleep (Wake-on-LAN is configured, a backup run \
             wakes it by itself)",
        );
    } else if options.wake {
        r.report(
            false,
            REACHABLE_ITEM,
            "no answer (Wake-on-LAN is not configured, so the server could not be woken)".into(),
        );
    } else {
        r.report(false, REACHABLE_ITEM, "no answer".into());
    }
    if !reachable {
        let why = if wake_on && !wake_tried {
            "not checked: the server did not answer"
        } else {
            "not checked: the server is not reachable"
        };
        r.not_checked(LOGIN_ITEM, why);
        if cfg.overview.is_some() {
            r.not_checked(OVERVIEW_ITEM, why);
        }
    } else if !options.login {
        let why = "not checked: needs root (run `sudo nestegg check`)";
        r.not_checked(LOGIN_ITEM, why);
        if cfg.overview.is_some() {
            r.not_checked(OVERVIEW_ITEM, why);
        }
    } else {
        let client = Client::new(&cfg, SecretStore::new(&paths.credentials_dir));
        match client.list_snapshots(&cfg.effective_backup_id()) {
            Ok(s) => r.report(true, LOGIN_ITEM, format!("{} snapshots", s.len())),
            Err(e) => r.report(false, LOGIN_ITEM, format!("{e:#}")),
        }
        if let Some(o) = &cfg.overview {
            let client = Client::with_credentials(
                &cfg,
                SecretStore::new(&paths.credentials_dir),
                &o.auth_id,
                secrets::OVERVIEW_PASSWORD,
            );
            match client.list_groups() {
                Ok(g) => r.report(true, OVERVIEW_ITEM, format!("{} machines", g.len())),
                Err(e) => r.report(false, OVERVIEW_ITEM, format!("{e:#}")),
            }
        }
    }
    r.report(
        crate::status::timer_enabled(paths),
        "backup timer enabled",
        crate::status::TIMER_UNIT.into(),
    );
    r.0
}

pub fn outcome(items: &[CheckItem]) -> Outcome {
    if items.iter().any(|i| i.status == CheckStatus::Failed) {
        Outcome::Failed
    } else if items.iter().any(|i| i.status == CheckStatus::NotChecked) {
        Outcome::Incomplete
    } else {
        Outcome::Passed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(status: CheckStatus) -> CheckItem {
        CheckItem {
            name: "x".into(),
            ok: status == CheckStatus::Passed,
            status,
            detail: String::new(),
        }
    }

    #[test]
    fn outcome_separates_failed_from_not_checked() {
        use CheckStatus::*;
        assert_eq!(outcome(&[]), Outcome::Passed);
        assert_eq!(outcome(&[item(Passed)]), Outcome::Passed);
        assert_eq!(
            outcome(&[item(Passed), item(NotChecked)]),
            Outcome::Incomplete
        );
        assert_eq!(outcome(&[item(NotChecked), item(Failed)]), Outcome::Failed);
        assert_eq!(Outcome::Passed.exit_code(), 0);
        assert_eq!(Outcome::Failed.exit_code(), 1);
        assert_eq!(Outcome::Incomplete.exit_code(), 2);
    }

    #[test]
    fn a_wake_packet_counts_only_when_it_was_really_sent() {
        use crate::wake::WakeEvent::*;
        let sent = |events: &[crate::wake::WakeEvent]| {
            let mut t = WakeTracker::default();
            for e in events {
                t.event(e);
            }
            t.sent()
        };
        // Server answered at once, or Wake-on-LAN off: no events.
        assert!(!sent(&[]));
        // Notices alone are no packet, whatever their number or wording.
        assert!(!sent(&[Info("sending")]));
        assert!(!sent(&[Info("waiting"), Info("sending"), Info("again")]));
        // The send failed and never recovered.
        assert!(!sent(&[Info("sending"), SendFailed("no network")]));
        // A packet went out.
        assert!(sent(&[Info("sending"), PacketSent]));
        assert!(sent(&[
            Info("sending"),
            SendFailed("no network"),
            PacketSent,
            SendRecovered
        ]));
        // A later resend failed: the earlier packet still counts.
        assert!(sent(&[
            Info("sending"),
            PacketSent,
            Info("again"),
            SendFailed("x")
        ]));
    }

    #[test]
    fn serialises_status_and_keeps_ok_false_for_not_checked() {
        let v = serde_json::to_value(item(CheckStatus::NotChecked)).unwrap();
        assert_eq!(v["status"], "not_checked");
        assert_eq!(v["ok"], false);
        let v = serde_json::to_value(item(CheckStatus::Passed)).unwrap();
        assert_eq!(v["status"], "passed");
        assert_eq!(v["ok"], true);
    }
}

#[cfg(test)]
mod run_tests {
    use super::*;

    /// A configuration whose PBS is a closed local port, optionally with
    /// Wake-on-LAN (the magic packet goes to the loopback address).
    fn setup(wake: bool) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        // A port nobody listens on: bind, note it, close again.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut text = format!(
            "[pbs]\nserver = \"127.0.0.1\"\nport = {port}\ndatastore = \"backups\"\n\
             auth_id = \"backup@pbs!desktop\"\n"
        );
        if wake {
            text.push_str(
                "[wake]\nmac = \"00:00:5e:00:53:01\"\nbroadcast = \"127.0.0.1\"\n\
                 port = 9\ntimeout_seconds = 1\n",
            );
        }
        let paths = Paths {
            config_file: dir.path().join("config.toml"),
            credentials_dir: dir.path().join("credentials"),
            state_dir: dir.path().join("state"),
            runtime_dir: dir.path().join("run"),
            systemctl: dir.path().join("no-systemctl"),
        };
        std::fs::write(&paths.config_file, text).unwrap();
        (dir, paths)
    }

    fn item<'a>(items: &'a [CheckItem], name: &str) -> &'a CheckItem {
        items.iter().find(|i| i.name == name).unwrap()
    }

    #[test]
    fn a_stick_only_setup_needs_no_stored_server_secret() {
        let (_dir, paths) = setup(false);
        std::fs::write(
            &paths.config_file,
            "[stick]\ndata_uuids = [\"01234567-89ab-cdef-0123-456789abcdef\"]\n",
        )
        .unwrap();
        let items = run(
            &paths,
            &CheckOptions {
                wake: false,
                login: true,
            },
        );
        assert!(
            items.iter().all(|i| i.name != "password / token secret"),
            "{items:?}"
        );
    }

    #[test]
    fn a_sleeping_server_with_wake_on_lan_is_not_checked_without_a_wake_request() {
        let (_dir, paths) = setup(true);
        let items = run(
            &paths,
            &CheckOptions {
                wake: false,
                login: true,
            },
        );
        let reach = item(&items, REACHABLE_ITEM);
        assert_eq!(reach.status, CheckStatus::NotChecked);
        assert!(!reach.ok, "old clients must not read it as fine");
        assert_eq!(
            item(&items, LOGIN_ITEM).status,
            CheckStatus::NotChecked,
            "the login cannot have been checked"
        );
    }

    #[test]
    fn a_server_that_does_not_wake_up_is_a_failed_check() {
        let (_dir, paths) = setup(true);
        let items = run(
            &paths,
            &CheckOptions {
                wake: true,
                login: true,
            },
        );
        let reach = item(&items, REACHABLE_ITEM);
        assert_eq!(reach.status, CheckStatus::Failed);
        assert!(
            reach.detail.contains("Wake-on-LAN attempt"),
            "{}",
            reach.detail
        );
        assert_eq!(item(&items, LOGIN_ITEM).status, CheckStatus::NotChecked);
        assert_eq!(outcome(&items), Outcome::Failed);
    }

    #[test]
    fn a_server_without_wake_on_lan_that_does_not_answer_is_failed() {
        let (_dir, paths) = setup(false);
        for wake in [false, true] {
            let items = run(&paths, &CheckOptions { wake, login: false });
            assert_eq!(item(&items, REACHABLE_ITEM).status, CheckStatus::Failed);
            assert_eq!(item(&items, LOGIN_ITEM).status, CheckStatus::NotChecked);
        }
    }

    #[test]
    fn check_reports_whether_a_wake_packet_was_sent() {
        let opts = |wake| CheckOptions { wake, login: false };
        // Not configured: nothing is sent, even when asked.
        let (_dir, paths) = setup(false);
        assert!(!run_reporting(&paths, &opts(true)).wake_sent);
        // Configured, but not asked for.
        let (_dir, paths) = setup(true);
        assert!(!run_reporting(&paths, &opts(false)).wake_sent);
        // Configured and asked for, the server does not answer: sent (to the
        // loopback address).
        assert!(run_reporting(&paths, &opts(true)).wake_sent);
        // The server answers at once: no packet.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let text = std::fs::read_to_string(&paths.config_file).unwrap();
        // Only the server's port (before the [wake] section) changes.
        let (pbs, wake) = text.split_once("[wake]").unwrap();
        let pbs: Vec<String> = pbs
            .lines()
            .map(|l| {
                if l.starts_with("port = ") {
                    format!("port = {port}")
                } else {
                    l.to_string()
                }
            })
            .collect();
        let text = format!("{}\n[wake]{wake}", pbs.join("\n"));
        std::fs::write(&paths.config_file, text).unwrap();
        assert!(!run_reporting(&paths, &opts(true)).wake_sent);
    }

    #[test]
    fn a_missing_configuration_is_one_failed_item() {
        let (_dir, mut paths) = setup(false);
        paths.config_file = paths.config_file.with_file_name("missing.toml");
        let items = run(
            &paths,
            &CheckOptions {
                wake: false,
                login: true,
            },
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, CheckStatus::Failed);
    }
}
