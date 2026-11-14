use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    pub url: String,
    pub listen: String,
    pub state_dir: PathBuf,
    pub credentials_dir: PathBuf,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub pbs: Pbs,
    pub wake: Option<crate::config::WakeConfig>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Pbs {
    pub server: String,
    pub port: u16,
    pub datastore: String,
    pub namespace: Option<String>,
    pub user: String,
    pub fingerprint: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let c: Self =
            toml::from_str(&std::fs::read_to_string(path).context("read hub configuration")?)?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.name.trim().is_empty(), "household name is empty");
        let url = url::Url::parse(&self.url).context("invalid hub URL")?;
        ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "hub URL must be an HTTPS origin without credentials or path"
        );
        self.listen
            .parse::<std::net::SocketAddr>()
            .context("invalid listen address")?;
        ensure!(
            !self.pbs.server.is_empty() && !self.pbs.server.contains(['/', '@', ' ', '\n']),
            "invalid PBS server"
        );
        ensure!(valid_name(&self.pbs.datastore), "invalid PBS datastore");
        ensure!(
            self.pbs.user.contains('@') && !self.pbs.user.contains(['!', '/', '\n', ' ']),
            "PBS user must be user@realm, not a token"
        );
        if let Some(ns) = &self.pbs.namespace {
            ensure!(
                !ns.is_empty() && ns.split('/').all(valid_name) && ns.split('/').count() <= 7,
                "invalid PBS namespace"
            );
        }
        if let Some(fp) = &self.pbs.fingerprint {
            super::tls::parse_fingerprint(fp)?;
        }
        if let Some(wake) = &self.wake {
            crate::config::parse_mac(&wake.mac)?;
            ensure!(
                wake.timeout_seconds <= 600,
                "Wake-on-LAN wait must not exceed 600 seconds"
            );
        }
        Ok(())
    }

    pub fn password(&self) -> Result<crate::secrets::Secret> {
        if let Some(dir) = std::env::var_os("CREDENTIALS_DIRECTORY") {
            return Ok(crate::secrets::Secret::new(std::fs::read(
                Path::new(&dir).join("password"),
            )?));
        }
        self.secrets().load(crate::secrets::PASSWORD)
    }

    pub fn secrets(&self) -> crate::secrets::SecretStore {
        crate::secrets::SecretStore::new(&self.credentials_dir)
    }
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_bounded_path_segments() {
        for good in ["desktop", "a-b_c.d", "9"] {
            assert!(valid_name(good));
        }
        for bad in ["", ".", "..", "a/b", "x!y", "x y", "é", &"a".repeat(65)] {
            assert!(!valid_name(bad));
        }
    }
}
