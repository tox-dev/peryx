//! Docker Hub keeps its official images under `library/`: `docker pull ubuntu` pulls
//! `library/ubuntu`. A client pulling through a routed proxy index sends the name it typed, so the
//! proxy is the one that must add the namespace before it asks Hub, or Hub answers `401`. The
//! rewrite is upstream-only: cache keys, tags, referrers, and the name the client sees keep the
//! spelling the client used.
//!
//! The neutral config layer carries an index's `[index.settings]` table raw and the composition root
//! hands this crate its own slice of it, so no neutral crate names a `library/` prefix.

use std::borrow::Cow;

use toml::{Table, Value};

/// The hosts that mean Docker Hub. `docker.io` is what a user writes, `index.docker.io` the name the
/// v1 API answered on, `registry-1.docker.io` the registry the v2 API actually serves from.
const DOCKER_HUB_HOSTS: [&str; 3] = ["docker.io", "index.docker.io", "registry-1.docker.io"];
/// The `[index.settings]` key [`LibraryPrefix`] is read from.
const LIBRARY_PREFIX: &str = "library_prefix";

/// One OCI index's settings, compiled from its `[index.settings]` table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IndexSettings {
    pub library_prefix: LibraryPrefix,
}

impl IndexSettings {
    /// # Errors
    /// Returns a user-visible message when a key is unknown to this ecosystem or a value is invalid.
    pub fn compile(settings: &Table) -> Result<Self, String> {
        if let Some(key) = settings.keys().find(|key| key.as_str() != LIBRARY_PREFIX) {
            return Err(format!("unknown field `{key}` in `[index.settings]`"));
        }
        settings.get(LIBRARY_PREFIX).map_or_else(
            || Ok(Self::default()),
            |value| {
                Ok(Self {
                    library_prefix: LibraryPrefix::parse(value)?,
                })
            },
        )
    }
}

/// Whether a single-segment repository is prefixed with `library/` before the upstream sees it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LibraryPrefix {
    /// Prefix only when the upstream is Docker Hub, which is the only registry that needs it.
    #[default]
    Auto,
    /// Prefix whatever the upstream, for a Hub-compatible mirror on some other host.
    Always,
    /// Never rewrite; pass the name through as the client spelled it.
    Never,
}

impl LibraryPrefix {
    fn parse(value: &Value) -> Result<Self, String> {
        match value {
            Value::Boolean(true) => Ok(Self::Always),
            Value::Boolean(false) => Ok(Self::Never),
            Value::String(mode) if mode == "auto" => Ok(Self::Auto),
            other => Err(format!(
                "`{LIBRARY_PREFIX}` must be true, false, or \"auto\", not {other}"
            )),
        }
    }
}

/// The name `repo` is spelled with in an upstream request to `base`: the URL path and the bearer
/// token scope both carry it, so a rewritten name must reach this before either is built.
///
/// Only a single-segment name is ever rewritten. `user/repo` already names its namespace, and
/// prefixing it would ask for a repository that does not exist.
pub fn upstream_repo<'a>(prefix: LibraryPrefix, base: &str, repo: &'a str) -> Cow<'a, str> {
    let rewrite = !repo.contains('/')
        && match prefix {
            LibraryPrefix::Auto => is_docker_hub(base),
            LibraryPrefix::Always => true,
            LibraryPrefix::Never => false,
        };
    if rewrite {
        Cow::Owned(format!("library/{repo}"))
    } else {
        Cow::Borrowed(repo)
    }
}

fn is_docker_hub(base: &str) -> bool {
    url::Url::parse(base).is_ok_and(|url| url.host_str().is_some_and(|host| DOCKER_HUB_HOSTS.contains(&host)))
}

#[cfg(test)]
#[path = "../tests/unit/settings/tests.rs"]
mod tests;
