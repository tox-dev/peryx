//! Startup-loaded netrc credentials for upstream HTTP origins.

use std::fmt;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use url::{Host, Url};

use super::Auth;

/// Credentials parsed from one operator-selected netrc file.
pub struct Netrc {
    parsed: uv_netrc::Netrc,
}

impl fmt::Debug for Netrc {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Netrc")
            .field("machine_count", &self.parsed.hosts.len())
            .finish_non_exhaustive()
    }
}

impl Netrc {
    /// Read, permission-check, and parse `path`.
    ///
    /// # Errors
    /// Returns [`NetrcError`] when the file cannot be read, is not a regular file, has unsafe Unix
    /// ownership or permissions, or contains invalid netrc syntax.
    pub fn from_path(path: &Path) -> Result<Self, NetrcError> {
        #[cfg(windows)]
        if !std::fs::metadata(path).map_err(read_error(path))?.is_file() {
            return Err(NetrcError::NotRegular {
                path: path.to_path_buf(),
            });
        }
        let mut file = File::open(path).map_err(read_error(path))?;
        let metadata = file.metadata().map_err(read_error(path))?;
        if !metadata.is_file() {
            return Err(NetrcError::NotRegular {
                path: path.to_path_buf(),
            });
        }
        check_permissions(path, &metadata)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents).map_err(read_error(path))?;
        let parsed = contents.parse().map_err(|_source| NetrcError::Parse {
            path: path.to_path_buf(),
        })?;
        Ok(Self { parsed })
    }

    /// Resolve Basic credentials for a URL string.
    ///
    /// # Errors
    /// Returns [`url::ParseError`] when `url` is invalid.
    pub fn auth_for_str(&self, url: &str) -> Result<Auth, url::ParseError> {
        Url::parse(url).map(|url| self.auth_for(&url))
    }

    /// Resolve Basic credentials for `url`.
    ///
    /// Origin-form entries have the highest precedence, followed by `host:port`, a pip-compatible
    /// bare host, and `default`. Empty entries do not authenticate.
    #[must_use]
    pub fn auth_for(&self, url: &Url) -> Auth {
        let Some(host) = url.host() else {
            return Auth::None;
        };
        let host = match host {
            Host::Ipv6(address) => format!("[{address}]"),
            Host::Domain(domain) => domain.to_owned(),
            Host::Ipv4(address) => address.to_string(),
        };
        let effective_port = url.port_or_known_default();
        let origin = url.origin().ascii_serialization();
        let mut candidates = Vec::with_capacity(6);
        if let Some(port) = effective_port {
            candidates.push(format!("{}://{host}:{port}", url.scheme()));
        }
        candidates.push(origin);
        if let Some(port) = effective_port {
            candidates.push(format!("{host}:{port}"));
        }
        if effective_port.is_some() && url.port().is_none() {
            candidates.push(host.clone());
            if let Some(unbracketed) = host.strip_prefix('[').and_then(|value| value.strip_suffix(']')) {
                candidates.push(unbracketed.to_owned());
            }
        }
        candidates.push("default".to_owned());
        candidates
            .into_iter()
            .find_map(|candidate| self.parsed.hosts.get(&candidate))
            .filter(|entry| !entry.login.is_empty() || !entry.password.is_empty())
            .map_or(Auth::None, |entry| Auth::Basic {
                username: entry.login.clone(),
                password: entry.password.clone(),
            })
    }
}

/// A redacted netrc startup error.
#[derive(Debug, thiserror::Error)]
pub enum NetrcError {
    #[error("cannot read netrc file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("netrc path {path} is not a regular file")]
    NotRegular { path: PathBuf },
    #[cfg(unix)]
    #[error("netrc file {path} must be owned by the effective user")]
    WrongOwner { path: PathBuf },
    #[cfg(unix)]
    #[error("netrc file {path} must not grant group or other permissions")]
    UnsafePermissions { path: PathBuf },
    #[error("netrc file {path} has invalid syntax")]
    Parse { path: PathBuf },
}

fn read_error(path: &Path) -> impl FnOnce(std::io::Error) -> NetrcError + '_ {
    |source| NetrcError::Read {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(unix)]
fn check_permissions(path: &Path, metadata: &std::fs::Metadata) -> Result<(), NetrcError> {
    use std::os::unix::fs::MetadataExt as _;

    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(NetrcError::WrongOwner {
            path: path.to_path_buf(),
        });
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(NetrcError::UnsafePermissions {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path, _metadata: &std::fs::Metadata) -> Result<(), NetrcError> {
    Ok(())
}
