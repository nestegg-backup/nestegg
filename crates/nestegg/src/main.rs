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
    Check {
        /// Wake the backup server (Wake-on-LAN) if it does not answer, and
        /// check again. Without it a sleeping server is reported as "not
        /// checked".
        #[arg(long)]
        wake: bool,
    },
    /// Start a backup now.
    Backup,
    /// Show the backup status of this computer.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Tell logged-in users (and an optional command) about failed,
    /// overdue, incomplete or damaged backups (root; used by
    /// nestegg-notify.timer).
    Notify {
        /// Send a test message through all configured channels.
        #[arg(long)]
        test: bool,
        /// Print what would be sent; send and record nothing.
        #[arg(long)]
        dry_run: bool,
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
    /// Write a paper key (QR code, check code) for safe offline storage.
    /// Print it, or pipe it to a printer; a terminal needs --output.
    Paperkey {
        #[arg(long, value_enum, default_value_t = PaperFormat::Text)]
        format: PaperFormat,
        /// Write to this file (e.g. key.html) instead of stdout.
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
    /// The file is read back and compared with the key.
    Export { file: PathBuf },
    /// Confirm that the key is stored safely outside this computer. Needs
    /// proof: a copy of the key or the check code of the paper key.
    ConfirmExported(ConfirmArgs),
    /// Check that a copy of the key still matches the stored key and opens
    /// the newest backup.
    Verify {
        /// A key file or a saved paper key (text or HTML).
        file: PathBuf,
    },
    /// Show key fingerprint and export status.
    Show,
}

#[derive(clap::Args)]
struct ConfirmArgs {
    /// A copy of the key: the file written by `key export`, or a saved
    /// paper key (text or HTML). Compared with the stored key.
    #[arg(long, conflicts_with = "paper_code")]
    file: Option<PathBuf>,
    /// The check code printed at the top of the paper key (XXXX-XXXX).
    #[arg(long, value_name = "XXXX-XXXX")]
    paper_code: Option<String>,
    /// The copy given with --file lies on a disk of this computer; state
    /// without a terminal that another copy exists outside this computer.
    #[arg(long, requires = "file")]
    accept_local_copy: bool,
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
        Cmd::Check { wake } => check(paths, wake),
        Cmd::Rescue { command } => nestegg::rescue::cli::run(command),
        Cmd::Backup => backup_now(paths),
        Cmd::Status { json } => status(paths, json),
        Cmd::Notify { test, dry_run } => {
            require_root("nestegg notify")?;
            nestegg::notify::run(paths, &nestegg::notify::Options { test, dry_run })?;
            Ok(0)
        }
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
            // Only ever used for key files named by the user.
            let data = nestegg::restore::read_small_regular_file(
                &path,
                nestegg::restore::MAX_KEY_FILE_BYTES,
            )?;
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
            "     sudo nestegg key paperkey --output key.html   # print it - or: sudo nestegg key export <file> (USB stick)"
        );
        println!(
            "     sudo nestegg key confirm-exported --file <copy>   # or: --paper-code <code on the paper key>"
        );
    }
    println!("  3. sudo nestegg check");
    println!(
        "  4. sudo systemctl enable --now nestegg.service nestegg-backup.timer nestegg-notify.timer"
    );
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
    // Echo off before the prompt is shown (no window for echoed input).
    let saved = if tty {
        let original = tcgetattr(&stdin)?;
        let mut silent = original.clone();
        silent.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(&stdin, SetArg::TCSANOW, &silent)?;
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
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
    // New credentials may cure a failing backup: let the timer retry at once.
    if let Err(e) = Store::new(paths).clear_failures() {
        eprintln!("warning: {e:#}");
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
            state.consecutive_failures = 0;
            store.save_state(&state)?;
            println!("Encryption key created (fingerprint {fingerprint}).");
            if storage == Storage::SystemdCreds {
                println!("It is stored encrypted on this computer and is lost if this disk fails.");
            }
            println!(
                "Export it now: sudo nestegg key paperkey --output key.html   (then print it)\n\
                 \x20         or: sudo nestegg key export <file>                 (then move it to a USB stick)"
            );
            println!(
                "Then prove you hold a copy: sudo nestegg key confirm-exported --file <copy>\n\
                 \x20                      or: sudo nestegg key confirm-exported --paper-code <code on the paper key>"
            );
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
            // Does this import prove a copy outside this computer? For the
            // key that is already stored it proves nothing new (it could be
            // the file `key export` just wrote): an earlier confirmation
            // stays, none is created. For a new key the file counts when it
            // comes from a removable medium; a file on this computer's own
            // disk needs `key confirm-exported` like any other copy.
            let location = nestegg::keyproof::locate(&path);
            let counts_as_exported = if same_key {
                state.key_exported_digest.as_deref() == Some(digest.as_str())
            } else {
                !location.needs_statement()
            };
            // Same order as `key create`: no confirmation may survive for a
            // key that is about to be replaced.
            state.key_exported_digest = None;
            state.key_digest = None;
            state.key_fingerprint = None;
            store.save_state(&state)?;
            let storage = secrets_store.store(secrets::ENCRYPTION_KEY, &key, allow_plain)?;
            state.key_fingerprint = Some(info.fingerprint.clone());
            state.key_digest = Some(digest.clone());
            state.key_last_exported_digest = Some(digest.clone());
            state.key_exported_digest = counts_as_exported.then_some(digest);
            state.consecutive_failures = 0;
            store.save_state(&state)?;
            println!(
                "Encryption key imported (fingerprint {}) and stored {}.",
                info.fingerprint,
                match storage {
                    Storage::SystemdCreds => "encrypted with systemd-creds",
                    Storage::PlainFile => "as a root-only file",
                }
            );
            if counts_as_exported {
                println!(
                    "It counts as exported: you hold a copy outside this computer. Keep it safe."
                );
            } else {
                println!(
                    "It does NOT count as exported yet: {} lies on a disk of this computer, and                      a copy that is lost together with the computer is no protection.",
                    path.display()
                );
                println!(
                    "Backups start after: sudo nestegg key confirm-exported --file <copy on a USB                      stick>   (or --paper-code <code on the paper key>)"
                );
            }
        }
        KeyCmd::Paperkey { format, output } => {
            if !nestegg::keyproof::paperkey_stdout_allowed(output.is_some(), stdout_is_terminal()) {
                bail!(
                    "a paper key scrolling past in a terminal is not a copy of the key. Write it \
                     to a file and print that: nestegg key paperkey --format html --output key.html \
                     (or pipe it to a printer: nestegg key paperkey | lpr)"
                );
            }
            let cfg = Config::load(&paths.config_file)?;
            let key = secrets_store.load(secrets::ENCRYPTION_KEY)?;
            let material = nestegg::keyproof::KeyMaterial::from_key_json(&String::from_utf8_lossy(
                key.as_bytes(),
            ))
            .context("the stored encryption key is damaged")?;
            let tmp = temp_key(paths)?;
            let fingerprint = client::local::key_fingerprint(&tmp.0)?;
            let fmt = match format {
                PaperFormat::Text => "text",
                PaperFormat::Html => "html",
            };
            let raw = client::local::paperkey(&tmp.0, fmt)?;
            let header = nestegg::keyproof::PaperHeader {
                backup_id: cfg.effective_backup_id(),
                hostname: config::hostname(),
                date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
                server: format!("{}:{}", cfg.pbs.server, cfg.pbs.port),
                datastore: cfg.pbs.datastore.clone(),
                fingerprint,
                check_code: material.check_code(),
            };
            let data = match format {
                PaperFormat::Text => nestegg::keyproof::compose_text(&header, &raw),
                PaperFormat::Html => nestegg::keyproof::compose_html(&header, &raw)?,
            };
            match output {
                Some(file) => {
                    write_user_file(&file, &data)?;
                    // The check code stays on the paper: it is not shown here.
                    println!(
                        "Paper key written to {}. Print it and keep the paper away from this \
                         computer, then delete the file.\n\
                         Then prove you hold it: sudo nestegg key confirm-exported --paper-code \
                         <check code at the top of the paper>",
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
            // Read the file back with the invoking user's rights: what is
            // on disk must be the key, or the "copy" is worthless.
            let path = absolute_user_path(&file)?;
            let sudo = nestegg::identity::UserIdentity::from_sudo()?;
            let back =
                nestegg::restore::read_user_key(&path, sudo.as_ref()).with_context(|| {
                    format!("{} was written but cannot be read back", path.display())
                })?;
            if back.as_bytes() != key.as_bytes() {
                bail!(
                    "{} was written, but reading it back gave different content. Do not rely on \
                     this file; delete it and export again to another location.",
                    path.display()
                );
            }
            record_export(&store, &key)?;
            println!(
                "Key written to {} and read back: the file matches the stored key.",
                path.display()
            );
            match nestegg::keyproof::locate(&path) {
                nestegg::keyproof::Location::Removable => println!(
                    "It is on a removable medium. Keep that medium somewhere safe, away from \
                     this computer."
                ),
                other => {
                    println!();
                    println!("WARNING: this copy is not safe yet.");
                    if let nestegg::keyproof::Location::Unknown(why) = &other {
                        println!("It could not be determined where the file lies ({why}).");
                    } else {
                        println!("The file is on a disk that belongs to this computer.");
                    }
                    println!(
                        "It is lost together with this disk, and it is useless inside the \
                         backup, because the backup is encrypted with this very key.\n\
                         Move it to a USB stick or into a password manager, then run:\n\
                         \x20 sudo nestegg key confirm-exported --file <the copy>"
                    );
                }
            }
        }
        KeyCmd::ConfirmExported(args) => confirm_exported(paths, &store, &secrets_store, args)?,
        KeyCmd::Verify { file } => return key_verify(paths, &secrets_store, &file),
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

fn stdin_is_terminal() -> bool {
    nix::unistd::isatty(std::io::stdin()).unwrap_or(false)
}

fn stdout_is_terminal() -> bool {
    nix::unistd::isatty(std::io::stdout()).unwrap_or(false)
}

/// Ask one question on the terminal; the prompt goes to stderr.
fn prompt_line(question: &str) -> Result<String> {
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// The stored key as key material, for comparisons.
fn stored_key_material(
    secrets_store: &SecretStore,
) -> Result<(Secret, nestegg::keyproof::KeyMaterial)> {
    if !secrets_store.exists(secrets::ENCRYPTION_KEY) {
        bail!("no encryption key exists yet");
    }
    let key = secrets_store.load(secrets::ENCRYPTION_KEY)?;
    let material =
        nestegg::keyproof::KeyMaterial::from_key_json(&String::from_utf8_lossy(key.as_bytes()))
            .context("the stored encryption key is damaged")?;
    Ok((key, material))
}

/// Read a copy of the key (key file or saved paper key) with the invoking
/// user's rights. Returns the path, the key JSON and its material.
fn read_key_copy(file: &Path) -> Result<(PathBuf, String, nestegg::keyproof::KeyMaterial)> {
    let path = absolute_user_path(file)?;
    let sudo = nestegg::identity::UserIdentity::from_sudo()?;
    let bytes = nestegg::restore::read_user_key(&path, sudo.as_ref())?;
    let json = nestegg::keyproof::key_json_of_copy(bytes.as_bytes())
        .with_context(|| format!("{}", path.display()))?;
    let material = nestegg::keyproof::KeyMaterial::from_key_json(&json)
        .with_context(|| format!("{}", path.display()))?;
    Ok((path, json, material))
}

/// A leading `~/` typed at a prompt (no shell expands it there): the home
/// directory of the user who called sudo, else of the current user.
fn expand_home(input: &str) -> PathBuf {
    let Some(rest) = input.strip_prefix("~/") else {
        return PathBuf::from(input);
    };
    let home = match nestegg::identity::UserIdentity::from_sudo() {
        Ok(Some(user)) => Some(user.home),
        _ => std::env::var_os("HOME").map(PathBuf::from),
    };
    match home {
        Some(home) => home.join(rest),
        None => PathBuf::from(input),
    }
}

/// `nestegg key confirm-exported`: record that the key is safe outside this
/// computer, but only against proof (see docs/decisions/0018).
fn confirm_exported(
    paths: &Paths,
    store: &Store,
    secrets_store: &SecretStore,
    args: ConfirmArgs,
) -> Result<()> {
    use nestegg::keyproof::{self, LocalCopy};
    let (key, stored) = stored_key_material(secrets_store)?;
    let terminal = stdin_is_terminal();
    let (file, code) = match (args.file, args.paper_code) {
        (Some(file), _) => (Some(file), None),
        (None, Some(code)) => (None, Some(code)),
        (None, None) if terminal => {
            eprintln!("The backups are worthless without this key. How do you hold a copy of it");
            eprintln!("outside this computer?");
            eprintln!("  1) as a file (from `key export`, or a saved paper key)");
            eprintln!("  2) as a printed paper key");
            match prompt_line("Choose 1 or 2: ")?.as_str() {
                "1" => (Some(expand_home(&prompt_line("Path of the file: ")?)), None),
                "2" => (
                    None,
                    Some(prompt_line("Check code at the top of the paper key: ")?),
                ),
                _ => bail!("no proof given; nothing was recorded"),
            }
        }
        (None, None) => bail!(
            "proof needed: give a copy of the key with --file <path> (the exported file or a saved \
             paper key), or the check code of the paper key with --paper-code <XXXX-XXXX>"
        ),
    };
    if let Some(file) = file {
        let (path, _, copy) = read_key_copy(&file)?;
        // The place nestegg itself keeps the key in is not a copy.
        let resolved = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        for own in [&paths.credentials_dir, &paths.tmp_dir()] {
            let own = std::fs::canonicalize(own).unwrap_or_else(|_| own.clone());
            if resolved.starts_with(&own) {
                bail!(
                    "{} is where nestegg itself stores the key, not a copy of it; nothing was                      recorded. Export a copy: sudo nestegg key export <file>",
                    path.display()
                );
            }
        }
        if copy != stored {
            bail!(
                "{} does not contain the key stored on this computer; nothing was recorded. \
                 Export the current key again.",
                path.display()
            );
        }
        let location = keyproof::locate(&path);
        match keyproof::decide_local_copy(&location, args.accept_local_copy, terminal) {
            LocalCopy::Accept => {}
            LocalCopy::Ask => {
                eprintln!("The file matches the key, but it lies on a disk of this computer.");
                eprintln!("If this disk fails or is stolen, that copy is gone with it, and the");
                eprintln!("backup cannot help: it is encrypted with this very key.");
                let answer = prompt_line(
                    "Type yes if another copy exists outside this computer (USB stick, \
                     password manager, paper): ",
                )?;
                if answer != "yes" {
                    bail!("not confirmed; nothing was recorded");
                }
            }
            LocalCopy::Refuse => bail!(
                "{} lies on a disk of this computer ({}). A copy that is lost together with \
                 the computer is no protection. Move it to a USB stick or a password manager and \
                 give that file, or, if another copy exists outside this computer, add \
                 --accept-local-copy. Nothing was recorded.",
                path.display(),
                match &location {
                    keyproof::Location::Unknown(why) => why.as_str(),
                    _ => "internal disk",
                }
            ),
        }
    } else if let Some(code) = code {
        let given = keyproof::normalize_check_code(&code).context(
            "that is not a check code; it looks like ABCD-0123 and is printed at the top of \
             the paper key",
        )?;
        if given != stored.check_code() {
            bail!(
                "this check code does not belong to the key stored on this computer; nothing was \
                 recorded. Print a new paper key and use its code."
            );
        }
    }
    // Key changes and backup runs exclude each other. Take the lock only
    // now, so that waiting for the user does not block the backup timer.
    let _lock = nestegg::run::RunLock::try_acquire(paths)?
        .context("a backup is running; try again when it has finished")?;
    let digest = secrets::digest(&key);
    if secrets::digest(&secrets_store.load(secrets::ENCRYPTION_KEY)?) != digest {
        bail!("the stored key changed while waiting; nothing was recorded");
    }
    let mut state = store.load_state()?;
    state.key_digest = Some(digest.clone());
    state.key_last_exported_digest = Some(digest.clone());
    state.key_exported_digest = Some(digest);
    // Backups were failing for want of this confirmation: retry at once.
    state.consecutive_failures = 0;
    store.save_state(&state)?;
    println!("Confirmed: your copy is the stored key. Backups can run now.");
    println!("Check it again at any time: sudo nestegg key verify <copy>");
    Ok(())
}

/// `nestegg key verify`: does this copy still match the stored key, and does
/// it open the newest snapshot of this computer? Exit code 1 if not.
fn key_verify(paths: &Paths, secrets_store: &SecretStore, file: &Path) -> Result<u8> {
    let (_, stored) = stored_key_material(secrets_store)?;
    let (path, json, copy) = match read_key_copy(file) {
        Ok(read) => read,
        Err(e) => {
            println!("Copy matches the stored key:  NO - {e:#}");
            println!("Copy opens the newest backup: not tried");
            return Ok(1);
        }
    };
    if copy != stored {
        println!(
            "Copy matches the stored key:  NO - {} is a key, but not the one stored on this \
             computer",
            path.display()
        );
        println!("Copy opens the newest backup: not tried");
        return Ok(1);
    }
    println!("Copy matches the stored key:  yes");
    let opened = verify_opens_newest(paths, json);
    match &opened {
        Ok(text) => println!("Copy opens the newest backup: {text}"),
        Err((fatal, text)) => println!(
            "Copy opens the newest backup: {}{text}",
            if *fatal { "NO - " } else { "not checked - " }
        ),
    }
    Ok(if matches!(opened, Err((true, _))) {
        1
    } else {
        0
    })
}

/// Err((true, ..)): the check ran and failed; Err((false, ..)): it could not
/// be tried (nothing wrong with the copy).
fn verify_opens_newest(paths: &Paths, json: String) -> Result<String, (bool, String)> {
    let cfg = Config::load(&paths.config_file).map_err(|e| (false, format!("{e:#}")))?;
    if !cfg.backup.encryption {
        return Err((
            false,
            "encryption is switched off in the configuration".into(),
        ));
    }
    if !nestegg::wake::is_reachable(&cfg, std::time::Duration::from_secs(5)) {
        return Err((
            false,
            "the PBS does not answer; run this again when it is reachable".into(),
        ));
    }
    // A snapshot that is still being written has no manifest to check.
    let Ok(Some(_no_run)) = nestegg::run::RunLock::try_acquire(paths) else {
        return Err((
            false,
            "a backup is running; run this again when it has finished".into(),
        ));
    };
    let client = Client::new(&cfg, SecretStore::new(&paths.credentials_dir));
    let snapshots = client
        .list_snapshots(&cfg.effective_backup_id())
        .map_err(|e| (true, format!("cannot list the snapshots: {e:#}")))?;
    let Some(newest) = snapshots.first() else {
        return Err((false, "there is no snapshot of this computer yet".into()));
    };
    match client.open_snapshot_with_key(&newest.snapshot, &Secret::new(json.into_bytes())) {
        Ok(true) => Ok(format!("yes ({})", newest.snapshot)),
        Ok(false) => Err((
            false,
            format!("the newest snapshot {} is not encrypted", newest.snapshot),
        )),
        Err(e) => Err((
            true,
            format!(
                "{e:#}\n  (snapshot {}; if the key was replaced after that backup, run a \
                 backup first)",
                newest.snapshot
            ),
        )),
    }
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

fn check(paths: &Paths, wake: bool) -> Result<u8> {
    use nestegg::check::{CheckStatus, Outcome};
    let items = nestegg::check::run(
        paths,
        &nestegg::check::CheckOptions {
            wake,
            login: is_root(),
        },
    );
    for item in &items {
        println!(
            "[{}] {}{}",
            match item.status {
                CheckStatus::Passed => " ok ",
                CheckStatus::Failed => "FAIL",
                CheckStatus::NotChecked => "skip",
            },
            item.name,
            if item.detail.is_empty() {
                String::new()
            } else {
                format!(": {}", item.detail)
            }
        );
    }
    let outcome = nestegg::check::outcome(&items);
    match outcome {
        Outcome::Passed => {}
        Outcome::Failed => println!("\nSomething failed (see FAIL above)."),
        Outcome::Incomplete => println!(
            "\nNot everything could be checked (see skip above). That is not a pass: \
             the skipped checks say nothing about whether they would succeed."
        ),
    }
    Ok(outcome.exit_code())
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
    if let Some(success) = &status.last_success {
        for (i, reason) in success.incomplete.iter().enumerate() {
            let label = if i == 0 { "Incomplete:" } else { "" };
            println!("{label:<14}{reason}");
        }
    }
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
    if !status.server_verify_failed.is_empty() {
        println!(
            "Server check: FAILED for {} snapshot(s): {}",
            status.server_verify_failed.len(),
            status.server_verify_failed.join(", ")
        );
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
            RunState::Succeeded if !run.incomplete.is_empty() => "part",
            RunState::Succeeded => "ok  ",
            RunState::PbsUnreachable => "away",
            _ => "FAIL",
        };
        let incomplete = if run.incomplete.is_empty() {
            String::new()
        } else {
            format!(" - incomplete: {}", run.incomplete.join("; "))
        };
        println!("{marker} {}{incomplete}", describe(Some(&run)));
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
