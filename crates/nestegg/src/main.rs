use std::io::{BufRead, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use nestegg::client::{self, Client};
use nestegg::config::{self, Config, Paths};
use nestegg::run::{RunOptions, execute};
use nestegg::secrets::{self, Secret, SecretStore, Storage};
use nestegg::state::{Run, RunState, Store, Trigger, format_time};

#[derive(Parser)]
#[command(
    name = "nestegg",
    version,
    about = "Encrypted backups of this computer to a Proxmox Backup Server"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the configuration and directories (root).
    Init(InitArgs),
    /// Manage the PBS password / API token secret (root).
    #[command(subcommand)]
    Credentials(CredCmd),
    /// Manage the encryption key (root).
    #[command(subcommand)]
    Key(KeyCmd),
    /// Check configuration, secrets, client and server access.
    Check,
    /// Start a backup now.
    Backup,
    /// Show the backup status of this computer.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Show recent backup runs.
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// List the snapshots of this computer on the PBS.
    Snapshots {
        #[arg(long)]
        json: bool,
    },
    /// Restore an archive (or matching paths) into a new directory (root).
    Restore {
        /// Snapshot, e.g. host/desktop/2026-11-08T21:30:12Z (see `nestegg snapshots`).
        snapshot: String,
        /// Archive, e.g. home.pxar
        archive: String,
        /// Target directory; must not exist yet.
        target: PathBuf,
        /// Only restore paths matching this pattern (repeatable).
        #[arg(long)]
        pattern: Vec<String>,
        /// Use this key file (e.g. an exported key of an older key) instead
        /// of the stored key.
        #[arg(long, conflicts_with = "no_key")]
        keyfile: Option<PathBuf>,
        /// Do not pass any key (unencrypted, unsigned backups).
        #[arg(long)]
        no_key: bool,
        /// Root-owned directory for the temporary copy when restoring into a
        /// user directory (default: /var/lib/nestegg/restore-staging).
        #[arg(long)]
        staging_dir: Option<PathBuf>,
    },
    /// Restore this or another computer onto an empty disk (rescue stick).
    Rescue {
        #[command(subcommand)]
        command: Option<nestegg::rescue::cli::RescueCmd>,
    },
    /// Show the configuration.
    Config {
        /// Print an example configuration instead.
        #[arg(long)]
        example: bool,
    },
    /// Run the backup pipeline (used by the systemd timer).
    #[command(hide = true)]
    Run {
        #[arg(long, value_enum, default_value_t = TriggerArg::Cli)]
        trigger: TriggerArg,
        #[arg(long)]
        run_id: Option<String>,
        /// Ignore the schedule interval.
        #[arg(long)]
        force: bool,
    },
    /// Internal: create a new 0600 file from stdin (runs as the sudo user).
    #[command(name = "__write-new-file", hide = true)]
    WriteNewFile { path: PathBuf },
    /// Internal: print a file (runs as the sudo user).
    #[command(name = "__read-file", hide = true)]
    ReadFile { path: PathBuf },
    /// Internal: rename without replacing an existing path (runs as the sudo user).
    #[command(name = "__rename-noreplace", hide = true)]
    RenameNoReplace { from: PathBuf, to: PathBuf },
    /// Internal: remove an own directory tree (runs as the sudo user).
    #[command(name = "__remove-tree", hide = true)]
    RemoveTree { path: PathBuf },
    /// Run the local API daemon (used by nestegg.service).
    #[command(hide = true)]
    Daemon,
    /// Open the web interface (only reachable from this computer).
    Ui {
        /// Port on 127.0.0.1 (0 = random).
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Do not open a browser.
        #[arg(long)]
        no_browser: bool,
        /// Forward non-API requests to a UI development server.
        #[arg(long, value_name = "URL")]
        dev_proxy: Option<String>,
    },
}

#[derive(clap::Args)]
struct InitArgs {
    /// PBS host name or IP address.
    #[arg(long)]
    server: String,
    #[arg(long)]
    datastore: String,
    /// user@realm!token
    #[arg(long)]
    auth_id: String,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    namespace: Option<String>,
    /// SHA-256 certificate fingerprint (if the PBS uses a self-signed certificate).
    #[arg(long)]
    fingerprint: Option<String>,
    #[arg(long)]
    backup_id: Option<String>,
    /// MAC address of the PBS for Wake-on-LAN.
    #[arg(long)]
    wake_mac: Option<String>,
    /// Disable client-side encryption (not recommended).
    #[arg(long)]
    no_encryption: bool,
    /// Back up only the configured folders, not the whole system (no
    /// restore onto an empty disk with the rescue stick).
    #[arg(long)]
    no_system: bool,
    /// Overwrite an existing configuration.
    #[arg(long)]
    force: bool,
}

#[derive(Subcommand)]
enum CredCmd {
    /// Store the password or API token secret (read from stdin).
    SetPassword {
        /// Store as root-only file if systemd-creds is unavailable.
        #[arg(long)]
        allow_plain: bool,
        /// Store the secret of the read-only overview token (`[overview]`).
        #[arg(long)]
        overview: bool,
    },
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Create a new encryption key.
    Create {
        #[arg(long)]
        allow_plain: bool,
        /// Replace an existing key (old backups stay readable only with the old key!).
        #[arg(long)]
        force: bool,
    },
    /// Print a paper key (QR code) for safe offline storage.
    Paperkey {
        #[arg(long, value_enum, default_value_t = PaperFormat::Text)]
        format: PaperFormat,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Store an exported key file again (e.g. after new hardware). It counts
    /// as exported, because you already have the file.
    Import {
        /// The key file written by `nestegg key export` (no password).
        file: PathBuf,
        #[arg(long)]
        allow_plain: bool,
        /// Replace a different existing key (old backups stay readable only
        /// with the old key!).
        #[arg(long)]
        force: bool,
    },
    /// Write the key file to a location of your choice (e.g. a USB stick).
    Export { file: PathBuf },
    /// Confirm that the key is stored safely outside this computer.
    ConfirmExported,
    /// Show key fingerprint and export status.
    Show,
}

#[derive(Clone, Copy, ValueEnum)]
enum PaperFormat {
    Text,
    Html,
}

#[derive(Clone, Copy, ValueEnum)]
enum TriggerArg {
    Timer,
    Manual,
    Cli,
}

impl From<TriggerArg> for Trigger {
    fn from(t: TriggerArg) -> Trigger {
        match t {
            TriggerArg::Timer => Trigger::Timer,
            TriggerArg::Manual => Trigger::Manual,
            TriggerArg::Cli => Trigger::Cli,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let paths = Paths::from_env();
    match dispatch(cli.command, &paths) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(cmd: Cmd, paths: &Paths) -> Result<u8> {
    match cmd {
        Cmd::Init(args) => init(paths, args),
        Cmd::Credentials(CredCmd::SetPassword {
            allow_plain,
            overview,
        }) => set_password(paths, allow_plain, overview),
        Cmd::Key(k) => key(paths, k),
        Cmd::Check => check(paths),
        Cmd::Rescue { command } => nestegg::rescue::cli::run(command),
        Cmd::Backup => backup_now(paths),
        Cmd::Status { json } => status(paths, json),
        Cmd::History { limit, json } => history(paths, limit, json),
        Cmd::Snapshots { json } => snapshots(paths, json),
        Cmd::Restore {
            snapshot,
            archive,
            target,
            pattern,
            keyfile,
            no_key,
            staging_dir,
        } => restore(
            paths,
            &RestoreArgs {
                snapshot: &snapshot,
                archive: &archive,
                target: &target,
                patterns: &pattern,
                keyfile: keyfile.as_deref(),
                no_key,
                staging_dir: staging_dir.as_deref(),
            },
        ),
        Cmd::Config { example } => {
            if example {
                print!(
                    "{}",
                    config::example("pbs.example.com", "backups", "backup@pbs!desktop")
                        .to_toml()?
                );
            } else {
                print!("{}", Config::load(&paths.config_file)?.to_toml()?);
            }
            Ok(0)
        }
        Cmd::Run {
            trigger,
            run_id,
            force,
        } => {
            let options = RunOptions {
                trigger: trigger.into(),
                run_id,
                force: force || !matches!(trigger, TriggerArg::Timer),
            };
            Ok(execute(paths, &options)?.code() as u8)
        }
        Cmd::WriteNewFile { path } => {
            let mut data = Vec::new();
            std::io::stdin().read_to_end(&mut data)?;
            secrets::create_new_private_file(&path, &data)?;
            Ok(0)
        }
        Cmd::ReadFile { path } => {
            let data =
                std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
            std::io::stdout().write_all(&data)?;
            Ok(0)
        }
        Cmd::RenameNoReplace { from, to } => {
            rename_noreplace(&from, &to)?;
            Ok(0)
        }
        Cmd::RemoveTree { path } => {
            nestegg::staging::remove_tree(&path)?;
            Ok(0)
        }
        Cmd::Daemon => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(nestegg::daemon::serve(paths.clone()))?;
            Ok(0)
        }
        Cmd::Ui {
            port,
            no_browser,
            dev_proxy,
        } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(nestegg::ui::serve(nestegg::ui::UiOptions {
                port,
                open_browser: !no_browser,
                dev_proxy,
                socket: paths.socket(),
            }))?;
            Ok(0)
        }
    }
}

fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

fn require_root(what: &str) -> Result<()> {
    if !is_root() && std::env::var_os("NESTEGG_ALLOW_NON_ROOT").is_none() {
        bail!("{what} needs root; run it with sudo");
    }
    Ok(())
}

fn init(paths: &Paths, args: InitArgs) -> Result<u8> {
    require_root("nestegg init")?;
    if paths.config_file.exists() && !args.force {
        bail!(
            "{} already exists; use --force to overwrite",
            paths.config_file.display()
        );
    }
    let mut cfg = config::example(&args.server, &args.datastore, &args.auth_id);
    if let Some(p) = args.port {
        cfg.pbs.port = p;
    }
    cfg.pbs.namespace = args.namespace;
    cfg.pbs.fingerprint = args.fingerprint;
    cfg.backup_id = args.backup_id;
    cfg.backup.encryption = !args.no_encryption;
    cfg.backup.system = !args.no_system;
    if let Some(mac) = args.wake_mac {
        cfg.wake = Some(config::WakeConfig {
            mac,
            broadcast: std::net::Ipv4Addr::BROADCAST,
            port: 9,
            timeout_seconds: 180,
            enabled: true,
        });
    }
    cfg.validate()?;
    if let Some(dir) = paths.config_file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&paths.config_file, config_header() + &cfg.to_toml()?)?;
    std::fs::set_permissions(&paths.config_file, std::fs::Permissions::from_mode(0o644))?;
    secrets::create_private_dir(&paths.credentials_dir)?;
    std::fs::create_dir_all(paths.runs_dir())?;
    let group = ensure_group();
    for dir in [paths.state_dir.clone(), paths.runs_dir()] {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750))?;
        if let Some(gid) = group {
            let _ = nix::unistd::chown(&dir, None, Some(gid));
        }
    }
    println!("Configuration written to {}", paths.config_file.display());
    println!();
    println!("Next steps:");
    println!("  1. sudo nestegg credentials set-password      # API token secret, read from stdin");
    if cfg.backup.encryption {
        println!(
            "  2. sudo nestegg key create                    # or, to keep an exported key: sudo nestegg key import <file>"
        );
        println!(
            "     sudo nestegg key paperkey                  # print it, or: sudo nestegg key export <file>"
        );
        println!("     sudo nestegg key confirm-exported");
    }
    println!("  3. sudo nestegg check");
    println!("  4. sudo systemctl enable --now nestegg.service nestegg-backup.timer");
    println!(
        "  5. sudo usermod -aG nestegg $USER             # then log out and in for `nestegg ui`"
    );
    Ok(0)
}

fn config_header() -> String {
    "# nestegg configuration. Contains no secrets.\n\
     # Reference: docs/configuration.md\n\n"
        .to_string()
}

fn ensure_group() -> Option<nix::unistd::Gid> {
    if let Ok(Some(g)) = nix::unistd::Group::from_name(nestegg::daemon::GROUP) {
        return Some(g.gid);
    }
    let ok = std::process::Command::new("groupadd")
        .args(["--system", nestegg::daemon::GROUP])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        eprintln!(
            "warning: could not create group '{}'",
            nestegg::daemon::GROUP
        );
        return None;
    }
    nix::unistd::Group::from_name(nestegg::daemon::GROUP)
        .ok()
        .flatten()
        .map(|g| g.gid)
}

/// Read one line from stdin without echo when it is a terminal.
fn read_secret_line(prompt: &str) -> Result<Secret> {
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    let stdin = std::io::stdin();
    let tty = nix::unistd::isatty(&stdin).unwrap_or(false);
    let saved = if tty {
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let original = tcgetattr(&stdin)?;
        let mut silent = original.clone();
        silent.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(&stdin, SetArg::TCSANOW, &silent)?;
        Some(original)
    } else {
        None
    };
    let mut line = String::new();
    let result = stdin.lock().read_line(&mut line);
    if let Some(original) = saved {
        let _ = tcsetattr(&stdin, SetArg::TCSANOW, &original);
        eprintln!();
    }
    result?;
    let value = line.trim_end_matches(['\n', '\r']).to_string();
    if value.is_empty() {
        bail!("empty input");
    }
    Ok(Secret::new(value.into_bytes()))
}

fn set_password(paths: &Paths, allow_plain: bool, overview: bool) -> Result<u8> {
    require_root("storing credentials")?;
    let (name, prompt) = if overview {
        (secrets::OVERVIEW_PASSWORD, "Overview token secret: ")
    } else {
        (secrets::PASSWORD, "PBS password or API token secret: ")
    };
    let secret = read_secret_line(prompt)?;
    let store = SecretStore::new(&paths.credentials_dir);
    match store.store(name, &secret, allow_plain)? {
        Storage::SystemdCreds => println!("Stored encrypted with systemd-creds."),
        Storage::PlainFile => println!("Stored as root-only file."),
    }
    Ok(0)
}

/// Decrypt the key into a private temporary file for client commands that
/// need a path. The file is removed when the guard is dropped.
struct TempKey(PathBuf);

impl Drop for TempKey {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::metadata(&self.0) {
            let zeros = vec![0u8; meta.len() as usize];
            let _ = std::fs::write(&self.0, zeros);
        }
        let _ = std::fs::remove_file(&self.0);
    }
}

fn temp_key(paths: &Paths) -> Result<TempKey> {
    let key = SecretStore::new(&paths.credentials_dir).load(secrets::ENCRYPTION_KEY)?;
    secrets::create_private_dir(&paths.tmp_dir())?;
    let path = paths
        .tmp_dir()
        .join(format!("key-{}.json", std::process::id()));
    secrets::write_private_file(&path, key.as_bytes())?;
    Ok(TempKey(path))
}

fn key(paths: &Paths, cmd: KeyCmd) -> Result<u8> {
    require_root("key management")?;
    let store = Store::new(paths);
    let secrets_store = SecretStore::new(&paths.credentials_dir);
    // Key changes and backup runs exclude each other (same lock), so a run
    // never sees a half-replaced key or overwrites the key state.
    let _lock = match &cmd {
        KeyCmd::Create { .. }
        | KeyCmd::Import { .. }
        | KeyCmd::ConfirmExported
        | KeyCmd::Export { .. }
        | KeyCmd::Paperkey { .. } => Some(
            nestegg::run::RunLock::try_acquire(paths)?
                .context("a backup is running; try again when it has finished")?,
        ),
        _ => None,
    };
    match cmd {
        KeyCmd::Create { allow_plain, force } => {
            if secrets_store.exists(secrets::ENCRYPTION_KEY) && !force {
                bail!(
                    "an encryption key already exists. Replacing it makes existing backups \
                     unreadable without the old key. Use --force only if you exported the old key."
                );
            }
            secrets::create_private_dir(&paths.tmp_dir())?;
            let tmp = TempKey(
                paths
                    .tmp_dir()
                    .join(format!("new-key-{}.json", std::process::id())),
            );
            let _ = std::fs::remove_file(&tmp.0);
            client::local::create_key(&tmp.0)?;
            let fingerprint = client::local::key_fingerprint(&tmp.0)?;
            let key = Secret::new(std::fs::read(&tmp.0)?);
            // Invalidate any previous export confirmation before the key is
            // replaced. (Backups compare against the digest of the key that
            // is actually loaded, so a failure here cannot let them run.)
            let mut state = store.load_state()?;
            state.key_exported_digest = None;
            state.key_digest = None;
            state.key_fingerprint = None;
            store.save_state(&state)?;
            let storage = secrets_store.store(secrets::ENCRYPTION_KEY, &key, allow_plain)?;
            state.key_fingerprint = Some(fingerprint.clone());
            state.key_digest = Some(secrets::digest(&key));
            store.save_state(&state)?;
            println!("Encryption key created (fingerprint {fingerprint}).");
            if storage == Storage::SystemdCreds {
                println!("It is stored encrypted on this computer and is lost if this disk fails.");
            }
            println!(
                "Export it now: sudo nestegg key paperkey   or   sudo nestegg key export <file>"
            );
            println!("Then confirm:  sudo nestegg key confirm-exported");
        }
        KeyCmd::Import {
            file,
            allow_plain,
            force,
        } => {
            let path = absolute_user_path(&file)?;
            // Through sudo the file is read with the invoking user's rights.
            let sudo = nestegg::identity::UserIdentity::from_sudo()?;
            let key = nestegg::restore::read_user_key(&path, sudo.as_ref())?;
            // Validate a private copy: `key show` needs a path.
            secrets::create_private_dir(&paths.tmp_dir())?;
            let tmp = TempKey(
                paths
                    .tmp_dir()
                    .join(format!("import-key-{}.json", std::process::id())),
            );
            secrets::write_private_file(&tmp.0, key.as_bytes())?;
            let info = client::local::key_info(&tmp.0)
                .with_context(|| format!("{} is not a usable key file", path.display()))?;
            drop(tmp);
            info.require_unprotected()?;
            let mut state = store.load_state()?;
            // Importing the key that is already recorded (e.g. after a
            // restore onto new hardware, where the stored copy cannot be
            // decrypted any more) replaces nothing.
            let same_key = state.key_fingerprint.as_deref() == Some(info.fingerprint.as_str());
            if secrets_store.exists(secrets::ENCRYPTION_KEY) && !force && !same_key {
                bail!(
                    "a different encryption key is stored (fingerprint {}). Replacing it makes \
                     backups made with it unreadable without that key. Use --force only if you \
                     exported that key.",
                    state.key_fingerprint.as_deref().unwrap_or("unknown")
                );
            }
            let digest = secrets::digest(&key);
            // Same order as `key create`: no confirmation may survive for a
            // key that is about to be replaced.
            state.key_exported_digest = None;
            state.key_digest = None;
            state.key_fingerprint = None;
            store.save_state(&state)?;
            let storage = secrets_store.store(secrets::ENCRYPTION_KEY, &key, allow_plain)?;
            state.key_fingerprint = Some(info.fingerprint.clone());
            state.key_digest = Some(digest.clone());
            // The user already holds this key outside the computer.
            state.key_last_exported_digest = Some(digest.clone());
            state.key_exported_digest = Some(digest);
            store.save_state(&state)?;
            println!(
                "Encryption key imported (fingerprint {}) and stored {}.",
                info.fingerprint,
                match storage {
                    Storage::SystemdCreds => "encrypted with systemd-creds",
                    Storage::PlainFile => "as a root-only file",
                }
            );
            println!("It counts as exported, because you already have the file. Keep it safe.");
        }
        KeyCmd::Paperkey { format, output } => {
            let key = secrets_store.load(secrets::ENCRYPTION_KEY)?;
            let tmp = temp_key(paths)?;
            let fmt = match format {
                PaperFormat::Text => "text",
                PaperFormat::Html => "html",
            };
            let data = client::local::paperkey(&tmp.0, fmt)?;
            match output {
                Some(file) => {
                    write_user_file(&file, &data)?;
                    println!(
                        "Paper key written to {}. Print it and store it safely.",
                        file.display()
                    );
                }
                None => std::io::stdout().write_all(&data)?,
            }
            record_export(&store, &key)?;
        }
        KeyCmd::Export { file } => {
            let key = secrets_store.load(secrets::ENCRYPTION_KEY)?;
            write_user_file(&file, key.as_bytes())?;
            record_export(&store, &key)?;
            println!(
                "Key written to {}. Move it to a safe place outside this computer.",
                file.display()
            );
        }
        KeyCmd::ConfirmExported => {
            if !secrets_store.exists(secrets::ENCRYPTION_KEY) {
                bail!("no encryption key exists yet");
            }
            // Only confirm the key that was actually written out.
            let digest = secrets::digest(&secrets_store.load(secrets::ENCRYPTION_KEY)?);
            let mut state = store.load_state()?;
            if state.key_last_exported_digest.as_deref() != Some(digest.as_str()) {
                bail!(
                    "the current key has not been exported yet; run `nestegg key paperkey` or \
                     `nestegg key export <file>` first"
                );
            }
            state.key_digest = Some(digest.clone());
            state.key_exported_digest = Some(digest);
            store.save_state(&state)?;
            println!("Noted. Backups can run now.");
        }
        KeyCmd::Show => {
            let state = store.load_state()?;
            println!(
                "key present:  {}",
                if secrets_store.exists(secrets::ENCRYPTION_KEY) {
                    "yes"
                } else {
                    "no"
                }
            );
            println!(
                "fingerprint:  {}",
                state.key_fingerprint.as_deref().unwrap_or("-")
            );
            println!(
                "exported:     {}",
                if state.key_exported() { "yes" } else { "NO" }
            );
        }
    }
    Ok(0)
}

/// Remember which key was exported (paper key or file), under the run lock.
fn record_export(store: &Store, key: &Secret) -> Result<()> {
    let mut state = store.load_state()?;
    state.key_last_exported_digest = Some(secrets::digest(key));
    store.save_state(&state)
}

/// Create a new mode-0600 file at a user-chosen path. Through sudo, a
/// helper process with the invoking user's identity creates it, so the
/// kernel applies that user's permissions and the file belongs to them. It
/// never follows a symlink and never replaces an existing file.
fn write_user_file(path: &Path, data: &[u8]) -> Result<()> {
    let path = &absolute_user_path(path)?;
    match nestegg::identity::UserIdentity::from_sudo()? {
        Some(user) if is_root() => nestegg::identity::write_new_file_as(&user, path, data),
        _ => secrets::create_new_private_file(path, data),
    }
}

fn check(paths: &Paths) -> Result<u8> {
    let items = nestegg::check::run(
        paths,
        &nestegg::check::CheckOptions {
            wake: false,
            login: is_root(),
        },
    );
    for item in &items {
        println!(
            "[{}] {}{}",
            if item.ok { " ok " } else { "FAIL" },
            item.name,
            if item.detail.is_empty() {
                String::new()
            } else {
                format!(": {}", item.detail)
            }
        );
    }
    Ok(u8::from(!nestegg::check::passed(&items)?))
}

// ---------- commands that work through the daemon for non-root users ----------

/// Minimal HTTP/1.0 request over the daemon socket.
fn daemon_request(paths: &Paths, method: &str, path: &str) -> Result<(u16, serde_json::Value)> {
    let mut stream = UnixStream::connect(paths.socket()).with_context(|| {
        format!(
            "cannot connect to {} (is nestegg.service running and are you in group 'nestegg'?)",
            paths.socket().display()
        )
    })?;
    write!(
        stream,
        "{method} {path} HTTP/1.0\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n"
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .context("invalid daemon response")?;
    let code: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .context("invalid daemon status line")?;
    let value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    Ok((code, value))
}

fn backup_now(paths: &Paths) -> Result<u8> {
    if is_root() {
        let options = RunOptions {
            trigger: Trigger::Cli,
            run_id: None,
            force: true,
        };
        return Ok(execute(paths, &options)?.code() as u8);
    }
    let (code, body) = daemon_request(paths, "POST", "/api/v1/runs")?;
    match code {
        202 => {
            println!(
                "Backup started (run {}). Follow it with: nestegg status",
                body["id"].as_str().unwrap_or("?")
            );
            Ok(0)
        }
        409 => {
            println!("A backup is already running.");
            Ok(0)
        }
        _ => bail!(
            "daemon answered {code}: {}",
            body["message"].as_str().unwrap_or("")
        ),
    }
}

fn snapshots(paths: &Paths, json: bool) -> Result<u8> {
    let list: Vec<serde_json::Value> = if is_root() {
        let cfg = Config::load(&paths.config_file)?;
        let client = Client::new(&cfg, SecretStore::new(&paths.credentials_dir));
        client
            .list_snapshots(&cfg.effective_backup_id())?
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<_, _>>()?
    } else {
        let (code, body) = daemon_request(paths, "GET", "/api/v1/snapshots")?;
        if code != 200 {
            bail!(
                "daemon answered {code}: {}",
                body["message"].as_str().unwrap_or("")
            );
        }
        body["snapshots"].as_array().cloned().unwrap_or_default()
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(0);
    }
    if list.is_empty() {
        println!("No snapshots yet.");
    }
    for s in &list {
        println!(
            "{}  {:>10}  {}{}",
            s["snapshot"].as_str().unwrap_or("?"),
            s["size_bytes"]
                .as_u64()
                .map(human_bytes)
                .unwrap_or_else(|| "-".into()),
            if s["encrypted"].as_bool() == Some(true) {
                "encrypted"
            } else {
                ""
            },
            s["verified"]
                .as_str()
                .map(|v| format!(", verified {v}"))
                .unwrap_or_default(),
        );
    }
    Ok(0)
}

fn status(paths: &Paths, json: bool) -> Result<u8> {
    let cfg = Config::load(&paths.config_file).ok();
    let status =
        nestegg::status::build(paths, cfg.as_ref(), nestegg::status::timer_enabled(paths))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(0);
    }
    let health = serde_json::to_value(status.health)?;
    println!("Health:       {}", health.as_str().unwrap_or("?"));
    println!(
        "Machine:      {} (backup id {})",
        status.machine.hostname, status.machine.backup_id
    );
    if !status.pbs.server.is_empty() {
        println!(
            "PBS:          {} / {}{}",
            status.pbs.server,
            status.pbs.datastore,
            status
                .pbs
                .namespace
                .as_deref()
                .map(|n| format!(" / {n}"))
                .unwrap_or_default()
        );
    }
    if let Some(run) = &status.current_run {
        println!("Running:      {} ({:?})", run.id, run.state);
    }
    println!("Last success: {}", describe(status.last_success.as_ref()));
    println!("Last run:     {}", describe(status.last_run.as_ref()));
    println!(
        "Schedule:     {}{}",
        if status.schedule.enabled {
            &status.schedule.calendar
        } else {
            "timer not enabled"
        },
        status
            .schedule
            .next_run
            .map(|t| format!(", next {}", format_time(t)))
            .unwrap_or_default()
    );
    if status.encryption.enabled {
        println!(
            "Encryption:   on{}",
            if status.encryption.key_exported {
                ""
            } else {
                " - KEY NOT EXPORTED YET"
            }
        );
    } else {
        println!("Encryption:   off");
    }
    Ok(0)
}

fn describe(run: Option<&Run>) -> String {
    let Some(run) = run else {
        return "never".into();
    };
    let when = format_time(run.finished_at.unwrap_or(run.started_at));
    let mut s = format!("{when} {:?}", run.state);
    if let Some(d) = run.duration_seconds {
        s.push_str(&format!(" ({d} s)"));
    }
    if let Some(e) = &run.error {
        s.push_str(&format!(" - {}", e.message));
    }
    s
}

fn history(paths: &Paths, limit: usize, json: bool) -> Result<u8> {
    let runs = Store::new(paths).list_runs(limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
        return Ok(0);
    }
    if runs.is_empty() {
        println!("No runs yet.");
    }
    for run in runs {
        let marker = match run.state {
            RunState::Succeeded => "ok  ",
            RunState::PbsUnreachable => "away",
            _ => "FAIL",
        };
        println!("{marker} {}", describe(Some(&run)));
    }
    Ok(0)
}

/// Make a user-supplied path absolute against the current directory
/// (helpers run with cwd `/`). Symlinks are not resolved here; access is
/// checked later with the user's identity.
fn absolute_user_path(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    })
}

fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let from_c = std::ffi::CString::new(from.as_os_str().as_bytes())?;
    let to_c = std::ffi::CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: valid NUL-terminated paths, AT_FDCWD.
    let rc = unsafe {
        nix::libc::renameat2(
            nix::libc::AT_FDCWD,
            from_c.as_ptr(),
            nix::libc::AT_FDCWD,
            to_c.as_ptr(),
            nix::libc::RENAME_NOREPLACE,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot rename to {}", to.display()));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct RestoreArgs<'a> {
    snapshot: &'a str,
    archive: &'a str,
    target: &'a Path,
    patterns: &'a [String],
    keyfile: Option<&'a Path>,
    no_key: bool,
    staging_dir: Option<&'a Path>,
}

/// Keep the process alive on Ctrl+C / SIGTERM / SIGHUP during a restore:
/// the child processes (same process group for Ctrl+C) end, their failure
/// propagates and the normal cleanup removes staging and partial copies.
/// A handler (not SIG_IGN) is used so children get default behaviour.
fn install_restore_signal_handlers() {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    extern "C" fn note(_: nix::libc::c_int) {}
    let action = SigAction::new(
        SigHandler::Handler(note),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    for sig in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP] {
        // SAFETY: the handler does nothing.
        unsafe {
            let _ = sigaction(sig, &action);
        }
    }
}

fn restore(paths: &Paths, args: &RestoreArgs<'_>) -> Result<u8> {
    use nestegg::restore::{self, Job, Progress, Requester};
    require_root("restore")?;
    let target = restore::absolute_target(args.target)?;
    let sudo = nestegg::identity::UserIdentity::from_sudo()?;
    let key = if args.no_key {
        client::RestoreKey::Without
    } else if let Some(path) = args.keyfile {
        let path = absolute_user_path(path)?;
        client::RestoreKey::File(restore::read_user_key(&path, sudo.as_ref())?)
    } else {
        client::RestoreKey::Auto
    };
    if !restore::root_controlled_parent(&target)? {
        if sudo.is_some() {
            eprintln!(
                "note: {} is not in a root-controlled directory; the files are restored \
                 into a private staging directory first and then copied with your user's \
                 permissions (owned by you; ACLs, xattrs and capabilities are dropped)",
                target.display()
            );
        }
        install_restore_signal_handlers();
    }
    let cfg = Config::load(&paths.config_file)?;
    let mut stderr = std::io::stderr();
    let warnings = restore::execute(
        paths,
        &cfg,
        Job {
            snapshot: args.snapshot,
            archive: args.archive,
            target: &target,
            patterns: args.patterns,
            key,
            staging_dir: args.staging_dir,
        },
        &Requester::Root { sudo },
        &Progress::default(),
        &mut stderr,
    )?;
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    println!("Restored into {}", target.display());
    Ok(0)
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
