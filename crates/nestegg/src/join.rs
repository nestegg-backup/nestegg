//! Computer-side join/leave. The hub server lives in a separate module.
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

use crate::config::{self, Config, HubConfig, Paths};
use crate::masterkey::Proof;
use crate::pinned::Endpoint;
use crate::secrets::{self, Secret, SecretStore};
use crate::state::Store;

#[derive(clap::Args)]
pub struct JoinArgs {
    /// Hub HTTPS address, copied from the hub.
    pub hub_url: String,
    /// sha256// pin copied from the hub (curl --pinnedpubkey format).
    #[arg(long)]
    pub pin: String,
    /// One-time join code from the hub.
    #[arg(long)]
    pub code: String,
    /// Backup name on the PBS (default: this computer's host name).
    #[arg(long)]
    pub name: Option<String>,
    /// Take over this computer's existing backup group. Never use for another computer.
    #[arg(long)]
    pub adopt: bool,
    /// Replace hub settings and token, keeping other settings and the encryption key.
    #[arg(long)]
    pub replace_config: bool,
}

#[derive(Serialize)]
struct JoinRequest<'a> {
    code: &'a str,
    attempt: &'a str,
    hostname: String,
    backup_id: &'a str,
    adopt: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    os: Option<String>,
    nestegg_version: &'static str,
}

// Deliberately no Debug: these types contain secrets returned only once.
#[derive(Deserialize)]
struct JoinResponse {
    existing_snapshots: u64,
    machine_id: String,
    backup_id: String,
    machine_credential: String,
    pbs: Pbs,
    wake: Option<Wake>,
    master_key: MasterKey,
    hub: Hub,
}
#[derive(Deserialize)]
struct Pbs {
    server: String,
    port: u16,
    datastore: String,
    namespace: Option<String>,
    auth_id: String,
    fingerprint: Option<String>,
    secret: String,
}
#[derive(Deserialize)]
struct Wake {
    mac: String,
    broadcast: Option<std::net::Ipv4Addr>,
    timeout_seconds: Option<u64>,
}
#[derive(Deserialize)]
struct MasterKey {
    public_key_pem: String,
    fingerprint: String,
    proven_at: chrono::DateTime<chrono::Utc>,
}
#[derive(Deserialize)]
struct Hub {
    name: String,
    url: String,
    pin: String,
}
#[derive(Deserialize)]
struct HubError {
    code: String,
}

/// Do not echo the server's message: a failed server may echo request secrets.
fn error_message(code: &str) -> &'static str {
    match code {
        "invalid_code" => {
            "this join code no longer works. Open 'Add computer' in the hub and get a new code"
        }
        "name_taken" => {
            "another computer already uses this backup name. Choose a different name with --name <name>"
        }
        "name_exists_on_pbs" => {
            "backups with this name already exist on the backup server. Use --adopt only if they belong to THIS SAME computer or its replacement; recover its original key after joining if needed; otherwise choose --name <another-name>"
        }
        "hub_error" => {
            "the hub has an internal error. Ask its administrator to check the hub service and its logs, then repeat this command"
        }
        "pbs_unreachable" => {
            "the hub cannot reach the backup server. Check that the backup server is switched on and connected, then try again"
        }
        "pbs_refused" => {
            "the backup server refused the hub's request. Ask the hub administrator to check the PBS login and backup permissions, then try again"
        }
        "master_key_unproven" => {
            "the household's master key copy has not been proven yet. Ask the hub administrator to finish the master key confirmation, then get a new join code"
        }
        "no_package" => {
            "the hub has no nestegg package ready. Ask the hub administrator to add the package, then try again"
        }
        "unauthorized" => {
            "the hub no longer recognises this computer. Use nestegg leave --force, then join again with a new code"
        }
        "rate_limited" => "too many attempts were made. Wait a few minutes, then try again",
        "not_found" => {
            "this computer is no longer registered in the hub. Use nestegg leave --force to detach locally"
        }
        "bad_request" => {
            "the hub could not accept these details. Check the address, code and --name, then copy a fresh command from the hub"
        }
        _ => {
            "the hub could not complete the request. Ask the hub administrator to check it, then try again"
        }
    }
}
fn response_ok(status: u16, body: &[u8], wanted: u16) -> Result<()> {
    if status == wanted {
        return Ok(());
    }
    let code = serde_json::from_slice::<HubError>(body).ok();
    bail!(
        "{} (HTTP {status})",
        error_message(code.as_ref().map(|e| e.code.as_str()).unwrap_or("unknown"))
    )
}

fn make_config(
    response: &JoinResponse,
    endpoint: &Endpoint,
    pin: &str,
    name: &str,
    paths: &Paths,
) -> Result<Config> {
    if response.backup_id != name
        || response.machine_id.is_empty()
        || response.machine_credential.is_empty()
        || response.pbs.secret.is_empty()
    {
        bail!(
            "the hub returned incomplete or different computer details; ask its administrator to check it"
        );
    }
    let returned = Endpoint::parse(&response.hub.url)?;
    if returned.origin != endpoint.origin || response.hub.pin != pin {
        bail!(
            "the hub returned a different address or public-key pin; ask its administrator to check the hub address"
        );
    }
    if crate::masterkey::fingerprint(response.master_key.public_key_pem.as_bytes())?
        != response.master_key.fingerprint
    {
        bail!(
            "the master public key does not match the fingerprint supplied by the hub; ask its administrator to check it"
        );
    }
    if response.master_key.proven_at > chrono::Utc::now() + crate::state::future_tolerance() {
        bail!(
            "the hub's master key proof is dated in the future; check the clocks of this computer and the hub"
        );
    }
    let mut cfg = if paths.config_file.exists() {
        Config::load(&paths.config_file)?
    } else {
        let mut cfg = config::example(
            &response.pbs.server,
            &response.pbs.datastore,
            &response.pbs.auth_id,
        );
        cfg.backup.system = true;
        cfg
    };
    cfg.pbs = config::example(
        &response.pbs.server,
        &response.pbs.datastore,
        &response.pbs.auth_id,
    )
    .pbs;
    let pbs = cfg.pbs.as_mut().expect("example has PBS");
    pbs.port = response.pbs.port;
    pbs.namespace = response.pbs.namespace.clone();
    pbs.fingerprint = response.pbs.fingerprint.clone();
    cfg.backup_id = Some(response.backup_id.clone());
    cfg.backup.master_public_key = Some(paths.master_public_key());
    cfg.hub = Some(HubConfig {
        name: response.hub.name.clone(),
        url: endpoint.origin.clone(),
        pin: pin.into(),
        machine_id: response.machine_id.clone(),
    });
    cfg.wake = response.wake.as_ref().map(|w| config::WakeConfig {
        mac: w.mac.clone(),
        broadcast: w.broadcast.unwrap_or(std::net::Ipv4Addr::BROADCAST),
        port: 9,
        timeout_seconds: w.timeout_seconds.unwrap_or(180),
        enabled: true,
    });
    cfg.validate()?;
    Ok(cfg)
}

const UNITS: &[&str] = &[
    "nestegg.service",
    "nestegg-backup.timer",
    "nestegg-notify.timer",
];
#[derive(Serialize, Deserialize)]
struct UnitState {
    name: String,
    enabled: bool,
    active: bool,
}

trait Activation {
    fn snapshot(&mut self) -> Result<Vec<UnitState>>;
    fn enable(&mut self) -> Result<()>;
    fn user_to_add(&self) -> Result<Option<String>>;
    fn add_user(&mut self) -> Result<()>;
    fn start_backup(&mut self) -> Result<()>;
}
struct SystemActivation<'a> {
    paths: &'a Paths,
}
impl SystemActivation<'_> {
    fn command(&self, args: &[&str]) -> Result<()> {
        let success = Command::new(&self.paths.systemctl)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("cannot run systemctl; install the nestegg package on a systemd computer")?
            .success();
        if !success {
            bail!(
                "could not {}. Check systemctl status and the nestegg package installation",
                args.join(" ")
            );
        }
        Ok(())
    }
}
impl Activation for SystemActivation<'_> {
    fn snapshot(&mut self) -> Result<Vec<UnitState>> {
        let mut states = Vec::new();
        for unit in UNITS.iter().copied().chain(["nestegg-backup.service"]) {
            let query = |verb| -> Result<bool> {
                let out = Command::new(&self.paths.systemctl)
                    .args([verb, unit])
                    .stdin(Stdio::null())
                    .output()
                    .context("cannot run systemctl")?;
                let text = String::from_utf8_lossy(&out.stdout);
                match (verb, text.trim()) {
                    ("is-enabled", "enabled" | "enabled-runtime" | "static" | "indirect") => {
                        Ok(true)
                    }
                    ("is-enabled", "disabled" | "masked" | "masked-runtime") => Ok(false),
                    ("is-active", "active" | "activating" | "reloading") => Ok(true),
                    ("is-active", "inactive" | "failed" | "deactivating") => Ok(false),
                    _ => bail!(
                        "cannot read the state of {unit}; install the nestegg package and check that systemd is running"
                    ),
                }
            };
            states.push(UnitState {
                name: unit.into(),
                enabled: query("is-enabled")?,
                active: query("is-active")?,
            });
        }
        Ok(states)
    }
    fn enable(&mut self) -> Result<()> {
        self.command(&["enable", "--now", UNITS[0], UNITS[1], UNITS[2]])
    }
    fn user_to_add(&self) -> Result<Option<String>> {
        let Some(name) = std::env::var("SUDO_USER")
            .ok()
            .filter(|name| name != "root")
        else {
            return Ok(None);
        };
        if name.is_empty() || name.starts_with('-') || name.chars().any(char::is_control) {
            bail!("invalid SUDO_USER; run join from your normal login using sudo");
        }
        let user = nix::unistd::User::from_name(&name)?
            .context("the user who called sudo does not exist")?;
        if user.uid.is_root() {
            return Ok(None);
        }
        let group = nix::unistd::Group::from_name(crate::daemon::GROUP)?
            .context("group nestegg is missing; install the nestegg package before joining")?;
        let groups = nix::unistd::getgrouplist(&std::ffi::CString::new(name.clone())?, user.gid)?;
        Ok((!groups.contains(&group.gid)).then_some(name))
    }
    fn add_user(&mut self) -> Result<()> {
        let Some(user) = std::env::var("SUDO_USER").ok().filter(|u| u != "root") else {
            return Ok(());
        };
        if user.is_empty() || user.starts_with('-') || user.chars().any(char::is_control) {
            bail!("invalid SUDO_USER; run join from your normal login using sudo");
        }
        let success = Command::new("usermod")
            .args(["-aG", crate::daemon::GROUP, "--", &user])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("cannot add the calling user to group nestegg")?
            .success();
        if !success {
            bail!(
                "could not add the calling user to group nestegg; check the user account and package installation"
            );
        }
        Ok(())
    }
    fn start_backup(&mut self) -> Result<()> {
        self.command(&["start", "--no-block", "nestegg-backup.service"])
    }
}

// A durable join journal. Config is published last, after credentials and key
// state. Before activation files can be restored; after activation retries
// complete the setup. Only files written by join are tracked.
#[derive(Serialize, Deserialize)]
struct Original {
    path: PathBuf,
    backup: Option<String>,
    mode: u32,
    uid: u32,
    gid: u32,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    files: Vec<Original>,
    units: Vec<UnitState>,
    remote: Option<(String, String)>,
    activated: bool,
    user_to_add: Option<String>,
    #[serde(default)]
    enrollment: Option<Enrollment>,
}

#[derive(Serialize, Deserialize)]
struct Enrollment {
    url: String,
    pin: String,
    code_digest: String,
    name: String,
    attempt: String,
}
struct Transaction {
    dir: PathBuf,
    journal: Journal,
}
impl Transaction {
    fn begin(
        paths: &Paths,
        units: Vec<UnitState>,
        key_exists: bool,
        user_to_add: Option<String>,
    ) -> Result<Self> {
        let dir = paths.state_dir.join("join-transaction");
        fs::create_dir(&dir).context(
            "cannot create join undo journal; run join again to recover an interrupted setup",
        )?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let mut tx = Self {
            dir,
            journal: Journal {
                files: Vec::new(),
                units,
                remote: None,
                activated: false,
                user_to_add,
                enrollment: None,
            },
        };
        let mut files = vec![
            paths.config_file.clone(),
            paths.master_public_key(),
            paths.state_file(),
        ];
        for name in [secrets::PASSWORD, secrets::MACHINE_CREDENTIAL]
            .into_iter()
            .chain((!key_exists).then_some(secrets::ENCRYPTION_KEY))
        {
            files.push(paths.credentials_dir.join(name));
            files.push(paths.credentials_dir.join(format!("{name}.cred")));
        }
        for path in files {
            let (backup, mode, uid, gid) = match fs::symlink_metadata(&path) {
                Ok(meta) => {
                    if !meta.is_file() {
                        bail!(
                            "{} is not a regular file; nothing was changed",
                            path.display()
                        );
                    }
                    let name = format!("original-{}", tx.journal.files.len());
                    secrets::create_new_private_file(&tx.dir.join(&name), &fs::read(&path)?)?;
                    (Some(name), meta.mode() & 0o777, meta.uid(), meta.gid())
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
                    None,
                    0o600,
                    nix::unistd::geteuid().as_raw(),
                    nix::unistd::getegid().as_raw(),
                ),
                Err(e) => return Err(e.into()),
            };
            tx.journal.files.push(Original {
                path,
                backup,
                mode,
                uid,
                gid,
            });
        }
        tx.save()?;
        Ok(tx)
    }
    fn commit(&self, paths: &Paths) -> Result<()> {
        let completed = paths
            .state_dir
            .join(format!(".join-completed-{:016x}", rand::random::<u64>()));
        fs::rename(&self.dir, &completed)?;
        if let Err(e) = fs::File::open(&paths.state_dir).and_then(|dir| dir.sync_all()) {
            eprintln!(
                "warning: joined setup is committed, but its commit marker could not be synced: {e}"
            );
        }
        if let Err(e) = fs::remove_dir_all(&completed) {
            eprintln!(
                "warning: joined setup is committed, but its private undo files at {} could not be removed: {e}",
                completed.display()
            );
        }
        Ok(())
    }
    fn save(&self) -> Result<()> {
        secrets::write_private_file(
            &self.dir.join("journal.json"),
            &serde_json::to_vec(&self.journal)?,
        )?;
        fs::File::open(&self.dir)?.sync_all()?;
        fs::File::open(self.dir.parent().context("journal has no parent")?)?.sync_all()?;
        Ok(())
    }
    fn load(paths: &Paths) -> Result<Option<Self>> {
        let dir = paths.state_dir.join("join-transaction");
        if !dir.exists() {
            return Ok(None);
        }
        let journal = dir.join("journal.json");
        if !journal.exists() {
            // A crash while collecting originals: nothing has been installed yet.
            fs::remove_dir_all(dir)?;
            return Ok(None);
        }
        Ok(Some(Self { journal: serde_json::from_slice(&fs::read(journal)?).context("the join undo journal is damaged; ask an administrator to recover it before joining")?, dir }))
    }
    fn rollback_files(&self, paths: &Paths) -> Result<()> {
        let state = Store::new(paths).load_state()?;
        let used = state.last_run.is_some()
            || paths.current_run_file().exists()
            || state.last_success.is_some()
            || paths
                .runs_dir()
                .read_dir()
                .is_ok_and(|mut entries| entries.next().is_some());
        for original in &self.journal.files {
            let key = original
                .path
                .file_name()
                .is_some_and(|n| n == "encryption-key" || n == "encryption-key.cred");
            if key && (used || original.backup.is_some()) {
                continue;
            }
            match &original.backup {
                Some(backup) => {
                    secrets::write_private_file(&original.path, &fs::read(self.dir.join(backup))?)?;
                    if nix::unistd::geteuid().is_root() {
                        nix::unistd::chown(
                            &original.path,
                            Some(nix::unistd::Uid::from_raw(original.uid)),
                            Some(nix::unistd::Gid::from_raw(original.gid)),
                        )?;
                    }
                    fs::set_permissions(&original.path, fs::Permissions::from_mode(original.mode))?;
                }
                None => match fs::remove_file(&original.path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                    _ => {}
                },
            }
        }
        Ok(())
    }
    async fn rollback(&self, paths: &Paths, _activation: &mut dyn Activation) -> Result<()> {
        if self.journal.activated {
            bail!("activated setup must be completed; run the same join command again");
        }
        self.rollback_files(paths)?;
        if let Some((url, pin)) = &self.journal.remote {
            let credential = fs::read(self.dir.join("remote-credential"))?;
            let (status, body) = Endpoint::parse(url)?.post(pin, "/machines/self/leave", Some(&credential), b"{}".to_vec()).await
                .context("local setup was restored, but the hub still has this computer and its PBS token. Run the same join command again when the hub is reachable to finish cleanup")?;
            // A previous cleanup may have succeeded before the connection broke.
            if status != 401 && status != 404 {
                response_ok(status, &body, 204)?;
            }
        }
        fs::remove_dir_all(&self.dir)?;
        Ok(())
    }
}

fn configured(paths: &Paths, replace: bool) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(&paths.config_file) {
        if !meta.is_file() {
            bail!("the configuration is not a regular file; nothing was changed");
        }
        let cfg = Config::load(&paths.config_file)?;
        if cfg.hub.is_some() && !replace {
            bail!(
                "this computer has already joined a hub. Run sudo nestegg leave first; its encryption key will be kept"
            );
        }
        if !replace {
            bail!(
                "this computer is already configured. Use --replace-config to replace its configuration and token, keeping its encryption key unchanged"
            );
        }
    }
    Ok(())
}

pub async fn join(paths: &Paths, args: JoinArgs) -> Result<()> {
    join_with(paths, args, &mut SystemActivation { paths }).await
}
async fn join_with(paths: &Paths, args: JoinArgs, activation: &mut dyn Activation) -> Result<()> {
    let endpoint = Endpoint::parse(&args.hub_url)?;
    crate::pinned::validate_pin(&args.pin)?;
    let lock = crate::run::RunLock::try_acquire(paths)?
        .context("a backup is running; wait for it to finish before joining")?;
    let previous = Transaction::load(paths)?;
    if let Some(tx) = &previous
        && tx.journal.activated
    {
        activation.enable()?;
        activation.add_user()?;
        tx.commit(paths)?;
        drop(lock);
        if SecretStore::new(&paths.credentials_dir).exists(secrets::ENCRYPTION_KEY) {
            activation.start_backup()?;
        }
        println!("Interrupted join completed; the existing encryption key was kept.");
        return Ok(());
    }
    if previous.is_none() {
        configured(paths, args.replace_config)?;
    }
    let name = args.name.clone().unwrap_or_else(config::hostname);
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("choose --name with 1 to 64 letters, digits, '-', '_' or '.'");
    }
    // Preflight local prerequisites before consuming the one-time code.
    crate::client::local::version()?;
    let units = activation.snapshot()?;
    if units
        .iter()
        .any(|u| u.name == "nestegg-backup.service" && u.active)
    {
        bail!("a backup is starting; wait for it to finish before joining");
    }
    fs::create_dir_all(
        paths
            .config_file
            .parent()
            .context("configuration has no directory")?,
    )?;
    fs::create_dir_all(&paths.state_dir)?;
    let mut tx = if let Some(tx) = previous {
        if let Some(e) = &tx.journal.enrollment {
            if e.url != endpoint.origin
                || e.pin != args.pin
                || e.code_digest != secrets::digest(&Secret::new(args.code.as_bytes().to_vec()))
                || e.name != name
            {
                bail!(
                    "an interrupted join is pending; repeat the same command with the original code and name"
                );
            }
            tx.rollback_files(paths)?;
            tx
        } else {
            tx.rollback(paths, activation).await?;
            Transaction::begin(
                paths,
                units,
                SecretStore::new(&paths.credentials_dir).exists(secrets::ENCRYPTION_KEY),
                activation.user_to_add()?,
            )?
        }
    } else {
        Transaction::begin(
            paths,
            units,
            SecretStore::new(&paths.credentials_dir).exists(secrets::ENCRYPTION_KEY),
            activation.user_to_add()?,
        )?
    };
    if tx.journal.enrollment.is_none() {
        tx.journal.enrollment = Some(Enrollment {
            url: endpoint.origin.clone(),
            pin: args.pin.clone(),
            name: name.clone(),
            code_digest: secrets::digest(&Secret::new(args.code.as_bytes().to_vec())),
            attempt: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
        });
        tx.save()?;
    }
    let secret_store = SecretStore::new(&paths.credentials_dir);
    let key_exists = secret_store.exists(secrets::ENCRYPTION_KEY);
    // Reading an existing key is required; a broken sealed key must be imported,
    // never replaced by join, even with --replace-config.
    let existing = if key_exists {
        Some(secret_store.load(secrets::ENCRYPTION_KEY)?)
    } else {
        None
    };
    let mut uncertain = false;
    let mut needs_key = false;
    let result = async {
        let staged = SecretStore::new(&tx.dir.join("credentials"));
        let request = JoinRequest { code: &args.code, attempt: &tx.journal.enrollment.as_ref().expect("enrollment").attempt, hostname: config::hostname(), backup_id: &name, adopt: args.adopt, os: crate::status::os_name(), nestegg_version: env!("CARGO_PKG_VERSION") };
        let bytes = serde_json::to_vec(&request)?;
        let mut response = None;
        let mut last_error = None;
        // Same durable attempt for all retries, including after process death.
        for _ in 0..3 {
            match endpoint.post(&args.pin, "/join", None, bytes.clone()).await {
                Ok(answer) => { response = Some(answer); break; }
                Err(error) => {
                    let not_sent = error.is::<crate::pinned::NotSent>();
                    uncertain |= !not_sent;
                    last_error = Some(error);
                    if not_sent && !uncertain { break; }
                },
            }
        }
        let (status, body) = response.ok_or_else(|| last_error.unwrap_or_else(|| anyhow::anyhow!("no hub response"))).context("connection to the hub broke or timed out; repeat the SAME join command to repair enrollment")?;
        uncertain = status >= 500;
        response_ok(status, &body, 200)?;
        let response: serde_json::Value = serde_json::from_slice(&body).map_err(|_| anyhow::anyhow!("the hub returned unreadable setup details; ask its administrator to check whether this computer joined before trying again"))?;
        // Keep a usable cleanup credential even if another response field is
        // malformed. Neither response data nor serde errors are printed.
        let credential = response.get("machine_credential").and_then(serde_json::Value::as_str)
            .filter(|c| !c.is_empty()).context("the hub returned no cleanup credential; ask its administrator to remove any new registration before joining again")?;
        secrets::write_private_file(&tx.dir.join("remote-credential"), credential.as_bytes())?;
        tx.journal.remote = Some((endpoint.origin.clone(), args.pin.clone()));
        tx.save()?;
        let response: JoinResponse = serde_json::from_value(response).map_err(|_| anyhow::anyhow!("the hub returned incomplete setup details; ask its administrator to check it"))?;
        let cfg = make_config(&response, &endpoint, &args.pin, &name, paths)?;
        staged.store(secrets::PASSWORD, &Secret::new(response.pbs.secret.into_bytes()), true)?;
        staged.store(secrets::MACHINE_CREDENTIAL, &Secret::new(response.machine_credential.into_bytes()), true)?;
        let mut state = Store::new(paths).load_state()?;
        needs_key = !key_exists && response.existing_snapshots > 0;
        if !needs_key {
            let key_path = tx.dir.join("key.json");
            if let Some(key) = existing.as_ref() { secrets::write_private_file(&key_path, key.as_bytes())?; }
            else if !key_path.exists() { crate::client::local::create_key(&key_path)?; }
            let info = crate::client::local::key_info(&key_path)?;
            info.require_unprotected()?;
            let key = Secret::new(fs::read(&key_path)?);
            if !key_exists { staged.store(secrets::ENCRYPTION_KEY, &key, true)?; }
            state.key_fingerprint = Some(info.fingerprint);
            state.key_digest = Some(secrets::digest(&key));
        }
        state.master_key = Some(Proof { fingerprint: response.master_key.fingerprint, proven_at: response.master_key.proven_at });
        state.join_backup_pending = !needs_key;
        state.consecutive_failures = 0;
        state.failed_config = None;
        secrets::create_private_dir(&paths.credentials_dir)?;
        for entry in fs::read_dir(tx.dir.join("credentials"))? {
            let entry = entry?;
            let file_name = entry.file_name();
            let name = file_name.to_str().context("invalid staged credential name")?;
            let plain = name.strip_suffix(".cred").unwrap_or(name);
            // A resumed journal can contain an earlier staged key. Never
            // publish it over a primary key, or while recovery is required.
            if plain == secrets::ENCRYPTION_KEY && (key_exists || needs_key) { continue; }
            let alternate = if name.ends_with(".cred") { paths.credentials_dir.join(plain) } else { paths.credentials_dir.join(format!("{name}.cred")) };
            secrets::write_private_file(&paths.credentials_dir.join(name), &fs::read(entry.path())?)?;
            match fs::remove_file(alternate) { Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()), _ => {} }
        }
        fs::File::open(&paths.credentials_dir)?.sync_all()?;
        // The public key contains no secrets; group members must read it to
        // calculate status. It is never writable by the group.
        crate::settings::write_file_atomic(&paths.master_public_key(), &response.master_key.public_key_pem, 0o644)?;
        Store::new(paths).save_state(&state)?;
        let text = merged_config(paths, &cfg)?;
        if paths.config_file.exists() {
            let backup = paths.config_file.with_file_name(format!("config.toml.before-join-{:016x}", rand::random::<u64>()));
            secrets::create_new_private_file(&backup, &fs::read(&paths.config_file)?)?;
        }
        crate::settings::write_file_atomic(&paths.config_file, &text, 0o640)?;
        let group = nix::unistd::Group::from_name(crate::daemon::GROUP).ok().flatten();
        if let Some(group) = group {
            for path in [paths.config_file.clone(), paths.state_dir.clone()] { nix::unistd::chown(&path, None, Some(group.gid))?; }
            fs::set_permissions(&paths.state_dir, fs::Permissions::from_mode(0o750))?;
        }
        tx.journal.activated = true;
        tx.save()?;
        activation.enable()?;
        activation.add_user()?;
        tx.commit(paths)?;
        Ok::<_, anyhow::Error>(cfg)
    }.await;
    match result {
        Ok(cfg) => {
            drop(lock);
            if !needs_key {
                activation.start_backup()?;
            }
            println!(
                "Joined {} as {}. Backup configuration, credentials and master-key protection are ready.",
                cfg.hub.as_ref().expect("joined").name,
                name
            );
            if key_exists {
                println!("Your existing encryption key was kept unchanged.");
            }
            if needs_key {
                println!(
                    "Existing backups found. No encryption key was created and no backup was started."
                );
                println!(
                    "Continue with the old key: sudo nestegg key recover --master-key <file> --install (one key reads old and new backups)."
                );
                println!(
                    "Or start with a new key: sudo nestegg key create (old backups then require the master key)."
                );
            } else {
                println!(
                    "The first backup is starting in the background. Open the interface with: nestegg ui"
                );
            }
            println!("Log out and back in so membership in group nestegg takes effect.");
            Ok(())
        }
        Err(error) => {
            if tx.journal.activated || uncertain {
                return Err(error
                    .context("join is unfinished; repeat the same command to complete it safely"));
            }
            if let Err(rollback) = tx.rollback(paths, activation).await {
                bail!(
                    "{error:#}. Cleanup is unfinished: {rollback:#}. Run the same join command again to recover"
                );
            }
            Err(error.context("join did not finish; the previous local setup was restored (get a new code if the hub accepted the join)"))
        }
    }
}

pub async fn leave(paths: &Paths, force: bool) -> Result<()> {
    let _lock = crate::run::RunLock::try_acquire(paths)?
        .context("a backup is running; wait for it to finish before leaving")?;
    let cfg = Config::load(&paths.config_file)?;
    let hub = cfg
        .hub
        .as_ref()
        .context("this computer has not joined a hub")?;
    let remote = async {
        let credential =
            SecretStore::new(&paths.credentials_dir).load(secrets::MACHINE_CREDENTIAL)?;
        let (status, body) = Endpoint::parse(&hub.url)?
            .post(
                &hub.pin,
                "/machines/self/leave",
                Some(credential.as_bytes()),
                b"{}".to_vec(),
            )
            .await?;
        response_ok(status, &body, 204)
    }
    .await;
    match remote {
        Err(e) if !force => return Err(e.context(
            "nothing was detached locally; use --force only to detach without contacting the hub",
        )),
        Err(_) => eprintln!(
            "The hub could not confirm removal. Detached locally only: the computer registration and PBS token may remain on the hub. Ask its administrator to remove this computer."
        ),
        Ok(()) => println!(
            "The hub removed this computer and deleted its PBS token. Existing backups stay on the server."
        ),
    }
    let text = fs::read_to_string(&paths.config_file)?;
    let mut document: toml_edit::DocumentMut = text.parse()?;
    document.remove("hub");
    Config::parse(&document.to_string())?;
    crate::settings::write_atomically(&paths.config_file, &document.to_string())?;
    for name in [
        secrets::MACHINE_CREDENTIAL,
        "machine-credential.cred",
        secrets::PASSWORD,
        "password.cred",
    ] {
        match fs::remove_file(paths.credentials_dir.join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    println!(
        "This computer no longer backs up to the hub's PBS target until it joins again or receives its own token with nestegg init. Its encryption key and existing backups were kept."
    );
    Ok(())
}

/// Retain comments and all settings outside the hub's authority.
fn merged_config(paths: &Paths, cfg: &Config) -> Result<String> {
    if !paths.config_file.exists() {
        return cfg.to_toml();
    }
    let mut old: toml_edit::DocumentMut = fs::read_to_string(&paths.config_file)?.parse()?;
    let new: toml_edit::DocumentMut = cfg.to_toml()?.parse()?;
    for name in ["pbs", "wake", "hub", "backup_id"] {
        old.remove(name);
        if let Some(value) = new.get(name) {
            old[name] = value.clone();
        }
    }
    old["backup"]["master_public_key"] = new["backup"]["master_public_key"].clone();
    let text = old.to_string();
    Config::parse(&text)?;
    Ok(text)
}

/// Called after committing key recovery, with no run lock held.
pub fn start_first_backup(paths: &Paths) -> Result<()> {
    SystemActivation { paths }.start_backup()
}
