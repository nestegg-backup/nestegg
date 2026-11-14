use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use super::{
    config::{Config, Pbs},
    crypto,
    state::{self, Transaction},
};
use crate::secrets::Secret;
use anyhow::{Context, Result, ensure};
use clap::{Args as ClapArgs, Subcommand};

#[derive(ClapArgs)]
pub struct Args {
    /// Configuration file (alternate paths are useful in isolated tests).
    #[arg(long, default_value = "/etc/nestegg-hub/hub.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Set up a household hub, PBS access and the master key.
    Init(Box<Init>),
    /// Serve the HTTPS API, installer, package and web interface.
    Serve,
    /// Print local hub configuration and proof/package state (no secrets).
    Status,
    /// Change the administrator password and revoke all sessions.
    Password {
        /// Read the password from a private file instead of the terminal.
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
    /// Publish a nestegg Debian package.
    Package {
        #[command(subcommand)]
        command: PackageCommand,
    },
    /// Generate a single-use, 30-minute join command.
    JoinCode {
        #[arg(long)]
        note: Option<String>,
    },
    /// Prove the household master-key copy after an interrupted setup.
    MasterKey {
        #[command(subcommand)]
        command: MasterCommand,
    },
}
#[derive(Subcommand)]
enum PackageCommand {
    Add { file: PathBuf },
}
#[derive(Subcommand)]
enum MasterCommand {
    Confirm(super::master::Confirm),
}

#[derive(ClapArgs)]
struct Init {
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    url: Option<String>,
    #[arg(long, default_value = "0.0.0.0:8443")]
    listen: String,
    #[arg(long)]
    server: Option<String>,
    #[arg(long, default_value_t = 8007)]
    port: u16,
    #[arg(long)]
    datastore: Option<String>,
    #[arg(long)]
    namespace: Option<String>,
    /// Use the datastore root instead of a household namespace.
    #[arg(long, conflicts_with = "namespace")]
    root_namespace: bool,
    #[arg(long)]
    user: Option<String>,
    #[arg(long)]
    fingerprint: Option<String>,
    /// Use system certificate roots instead of discovering a PBS fingerprint.
    #[arg(long, conflicts_with = "fingerprint")]
    system_roots: bool,
    #[arg(long)]
    wake_mac: Option<String>,
    #[arg(long)]
    prepare_pbs: bool,
    #[arg(long)]
    pbs_admin: Option<String>,
    /// Private file; the password itself is never an argument.
    #[arg(long)]
    pbs_password_file: Option<PathBuf>,
    #[arg(long)]
    pbs_admin_password_file: Option<PathBuf>,
    #[arg(long)]
    admin_password_file: Option<PathBuf>,
    #[arg(long)]
    allow_plain: bool,
    #[arg(long)]
    package: Option<PathBuf>,
    #[arg(long)]
    paperkey: Option<PathBuf>,
    /// Confirm with `hub master-key confirm` later.
    #[arg(long)]
    defer_confirm: bool,
    #[arg(long, default_value = "/var/lib/nestegg-hub")]
    state_dir: PathBuf,
}

pub fn run(args: Args) -> Result<u8> {
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
    // CLI setup can invoke the official client directly on the terminal.
    // Runtime creation does not interpose on that terminal or its secrets.
    let runtime = tokio::runtime::Runtime::new()?;
    match args.command {
        Command::Init(init) => {
            require_root()?;
            init_hub(&runtime, &args.config, *init)?;
        }
        command => {
            let config = Config::load(&args.config)?;
            match command {
                Command::Serve => runtime.block_on(super::server::serve(config))?,
                Command::Status => {
                    let t = Transaction::open(&config.state_dir)?;
                    println!(
                        "Household: {}\nURL: {}\nPin: {}\nMaster key proven: {}\nPackage available: {}\nJoined computers: {}",
                        config.name,
                        config.url,
                        super::tls::pin(&config.certificate)?,
                        t.state.master.proven_at.is_some(),
                        t.state.package.is_some(),
                        t.state.machines.len()
                    );
                }
                Command::Password { password_file } => {
                    require_root()?;
                    let password =
                        password(password_file.as_deref(), "New administrator password", true)?;
                    let mut t = Transaction::open(&config.state_dir)?;
                    t.state.admin_hash = crypto::password_hash(password.as_bytes())?;
                    t.state.password_generation = crypto::random_secret();
                    t.save()?;
                    eprintln!("hub administrator password changed");
                    println!("Administrator password changed; existing sessions are revoked.");
                }
                Command::Package {
                    command: PackageCommand::Add { file },
                } => {
                    require_root()?;
                    super::package::add(&config.state_dir, &file)?;
                    println!("Package added.");
                }
                Command::JoinCode { note } => {
                    require_root()?;
                    let mut t = Transaction::open(&config.state_dir)?;
                    let code = super::server::create_code(
                        &config,
                        &super::tls::pin(&config.certificate)?,
                        &mut t,
                        note.as_deref(),
                    )?;
                    println!("{}", code["command"].as_str().context("join command")?);
                }
                Command::MasterKey {
                    command: MasterCommand::Confirm(proof),
                } => {
                    require_root()?;
                    super::master::confirm(&config, proof)?;
                }
                Command::Init(_) => unreachable!(),
            }
        }
    }
    Ok(0)
}
fn require_root() -> Result<()> {
    ensure!(
        nix::unistd::Uid::effective().is_root(),
        "this hub administration command needs root"
    );
    Ok(())
}
fn prompt(text: &str) -> Result<String> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .with_context(|| {
            format!("{text} missing; provide the corresponding option without a terminal")
        })?;
    write!(tty, "{text}: ")?;
    tty.flush()?;
    let mut s = String::new();
    std::io::BufReader::new(tty).read_line(&mut s)?;
    Ok(s.trim().to_string())
}
fn value(option: Option<String>, label: &str) -> Result<String> {
    option.map(Ok).unwrap_or_else(|| prompt(label))
}

fn password(file: Option<&Path>, label: &str, twice: bool) -> Result<Secret> {
    let bytes = if let Some(file) = file {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(file)?;
        ensure!(
            meta.mode() & 0o077 == 0,
            "password file must not be group- or world-readable"
        );
        crate::restore::read_small_regular_file(file, 4096)?
    } else {
        fn read(label: &str) -> Result<Vec<u8>> {
            let mut tty = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .context("password needs a terminal or a private --password-file")?;
            use nix::sys::termios::{self, LocalFlags, SetArg};
            let old = termios::tcgetattr(&tty)?;
            let mut hidden = old.clone();
            hidden.local_flags.remove(LocalFlags::ECHO);
            write!(tty, "{label}: ")?;
            tty.flush()?;
            termios::tcsetattr(&tty, SetArg::TCSAFLUSH, &hidden)?;
            let mut bytes = Vec::new();
            let result = std::io::BufReader::new(&mut tty).read_until(b'\n', &mut bytes);
            let restored = termios::tcsetattr(&tty, SetArg::TCSAFLUSH, &old);
            writeln!(tty)?;
            result?;
            restored?;
            while bytes.last().is_some_and(|b| b"\r\n".contains(b)) {
                bytes.pop();
            }
            Ok(bytes)
        }
        let first = Secret::new(read(label)?);
        if twice {
            let second = Secret::new(read("Repeat password")?);
            use subtle::ConstantTimeEq;
            ensure!(
                bool::from(first.as_bytes().ct_eq(second.as_bytes())),
                "passwords do not match"
            );
        }
        first.as_bytes().to_vec()
    };
    let mut bytes = bytes;
    while bytes.last().is_some_and(|b| b"\r\n".contains(b)) {
        bytes.pop();
    }
    ensure!(!bytes.is_empty(), "password must not be empty");
    if twice {
        validate_admin_password(&bytes)?;
    }
    Ok(Secret::new(bytes))
}

fn init_hub(runtime: &tokio::runtime::Runtime, path: &Path, args: Init) -> Result<()> {
    ensure!(
        !path.exists(),
        "hub configuration already exists; refusing to overwrite it; use hub master-key confirm for pending proof"
    );
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
    let parent = path.parent().context("configuration parent")?;
    std::fs::create_dir_all(parent)?;
    let mut config = Config {
        name: value(args.name, "Household name")?,
        url: value(args.url, "Hub HTTPS URL used by computers")?
            .trim_end_matches('/')
            .into(),
        listen: args.listen,
        state_dir: args.state_dir,
        credentials_dir: parent.join("credentials"),
        certificate: parent.join("tls.crt"),
        private_key: parent.join("tls.key"),
        pbs: Pbs {
            server: value(args.server, "PBS server")?,
            port: args.port,
            datastore: value(args.datastore, "PBS datastore")?,
            namespace: if args.root_namespace {
                None
            } else {
                let namespace = value(args.namespace, "PBS namespace (empty for datastore root)")?;
                if namespace.is_empty() {
                    None
                } else {
                    Some(namespace)
                }
            },
            user: value(args.user, "PBS household user (user@realm)")?,
            fingerprint: args.fingerprint,
        },
        wake: args.wake_mac.map(|mac| crate::config::WakeConfig {
            mac,
            broadcast: "255.255.255.255".parse().unwrap(),
            port: 9,
            timeout_seconds: 120,
            enabled: true,
        }),
    };
    config.validate()?;
    let admin_password = password(
        args.admin_password_file.as_deref(),
        "Hub administrator password",
        true,
    )?;

    if config.pbs.fingerprint.is_none() && !args.system_roots {
        let fp = super::tls::discover(&config.pbs.server, config.pbs.port)?;
        println!("PBS certificate SHA-256: {fp}");
        ensure!(
            prompt("Compare with the PBS console and type yes to trust this fingerprint")? == "yes",
            "PBS fingerprint not confirmed"
        );
        config.pbs.fingerprint = Some(fp);
    }
    let pbs_password = if args.prepare_pbs {
        let admin_user = value(
            args.pbs_admin,
            "One-time PBS administrator (for example root@pam)",
        )?;
        let admin_password = password(
            args.pbs_admin_password_file.as_deref(),
            "One-time PBS administrator password",
            false,
        )?;
        let mut admin_config = config.pbs.clone();
        admin_config.user = admin_user;
        let mut client = super::pbs::Client::new(&admin_config)?;
        let generated = if let Some(file) = args.pbs_password_file.as_deref() {
            password(Some(file), "Existing PBS household user password", false)?
        } else if config.secrets().exists(crate::secrets::PASSWORD) {
            config.secrets().load(crate::secrets::PASSWORD)?
        } else {
            Secret::new(crypto::random_secret().into_bytes())
        };
        // Retain a newly generated user password even if namespace/ACL setup
        // is interrupted, so a retry can authenticate instead of resetting it.
        config
            .secrets()
            .store(crate::secrets::PASSWORD, &generated, args.allow_plain)?;
        runtime.block_on(async {
            client.login(admin_password.as_bytes()).await?;
            client.prepare(&config.pbs.user, generated.as_bytes()).await
        })?;
        generated
    } else {
        password(
            args.pbs_password_file.as_deref(),
            "PBS household user password",
            false,
        )?
    };
    let mut client = super::pbs::Client::new(&config.pbs)?;
    runtime
        .block_on(async {
            client.login(pbs_password.as_bytes()).await?;
            let probe = format!("nestegg-probe-{}", &crypto::random_secret()[..16]);
            let (id, _) = client.create_token(&probe).await?;
            let acl = client.acl(&id).await;
            let deleted = client.delete_token(&probe).await;
            acl?;
            deleted?;
            client.groups().await?;
            anyhow::Ok(())
        })
        .context("PBS login/token/ACL verification failed; hub is not initialized")?;
    crate::secrets::create_private_dir(&config.state_dir)?;
    config
        .secrets()
        .store(crate::secrets::PASSWORD, &pbs_password, args.allow_plain)?;
    super::tls::create_certificate(&config.certificate, &config.private_key, &config.url)?;
    {
        let mut t = Transaction::open(&config.state_dir)?;
        t.state.admin_hash = crypto::password_hash(admin_password.as_bytes())?;
        t.state.password_generation = crypto::random_secret();
        t.save()?;
    }
    state::atomic(path, toml::to_string_pretty(&config)?.as_bytes())?;
    // Public configuration/certificate, private root-only credentials/key.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    std::fs::set_permissions(&config.certificate, std::fs::Permissions::from_mode(0o644))?;
    if let Some(package) = args.package {
        super::package::add(&config.state_dir, &package)?;
    }
    assign_state_owner(&config)?;
    let paper = args
        .paperkey
        .map(Ok)
        .unwrap_or_else(|| prompt("New file for the master paper key (HTML)").map(PathBuf::from))?;
    super::master::create(&config, &paper)?;
    if !args.defer_confirm {
        let proof = <ProofParser as clap::Parser>::parse_from(["proof"]);
        super::master::confirm(&config, proof.proof)?;
    }
    assign_state_owner(&config)?;
    println!(
        "Hub initialized. Pin: {}",
        super::tls::pin(&config.certificate)?
    );
    Ok(())
}
#[derive(clap::Parser)]
struct ProofParser {
    #[command(flatten)]
    proof: super::master::Confirm,
}

fn assign_state_owner(config: &Config) -> Result<()> {
    if let Some(user) = nix::unistd::User::from_name("nestegg-hub")? {
        fn owner(
            path: &Path,
            private: &Path,
            uid: nix::unistd::Uid,
            gid: nix::unistd::Gid,
        ) -> Result<()> {
            // Only the administrator needs the pending encrypted private key.
            if path == private {
                return Ok(());
            }
            nix::unistd::chown(path, Some(uid), Some(gid))?;
            if path.is_dir() {
                for entry in std::fs::read_dir(path)? {
                    owner(&entry?.path(), private, uid, gid)?;
                }
            }
            Ok(())
        }
        owner(
            &config.state_dir,
            &config.state_dir.join("master"),
            user.uid,
            user.gid,
        )?;
    }
    Ok(())
}

fn validate_admin_password(bytes: &[u8]) -> Result<()> {
    ensure!(
        std::str::from_utf8(bytes)?.chars().count() >= 12,
        "administrator password must contain at least 12 characters"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn administrator_password_requires_twelve_characters() {
        assert!(super::validate_admin_password(b"short").is_err());
        assert!(super::validate_admin_password(b"twelve-chars").is_ok());
        assert!(super::validate_admin_password("1234567890é".as_bytes()).is_err());
    }
}
