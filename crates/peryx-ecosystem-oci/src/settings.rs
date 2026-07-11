//! The OCI-specific settings an operator sets on one index, and the upstream name rewrite they drive.
//!
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
    /// Compile one index's `[index.settings]` table.
    ///
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
    /// Read the setting's TOML value: `true`, `false`, or `"auto"`.
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

/// Whether an upstream base URL points at Docker Hub, which `auto` keys the rewrite on.
fn is_docker_hub(base: &str) -> bool {
    url::Url::parse(base).is_ok_and(|url| url.host_str().is_some_and(|host| DOCKER_HUB_HOSTS.contains(&host)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::absent("", LibraryPrefix::Auto)]
    #[case::auto("library_prefix = \"auto\"", LibraryPrefix::Auto)]
    #[case::always("library_prefix = true", LibraryPrefix::Always)]
    #[case::never("library_prefix = false", LibraryPrefix::Never)]
    fn test_compile_reads_library_prefix(#[case] toml: &str, #[case] expected: LibraryPrefix) {
        let settings = IndexSettings::compile(&toml.parse::<Table>().unwrap()).unwrap();
        assert_eq!(settings.library_prefix, expected);
    }

    #[test]
    fn test_compile_rejects_an_unknown_key() {
        let settings = "libary_prefix = true".parse::<Table>().unwrap();
        assert_eq!(
            IndexSettings::compile(&settings).unwrap_err(),
            "unknown field `libary_prefix` in `[index.settings]`"
        );
    }

    #[rstest]
    #[case::string("library_prefix = \"always\"")]
    #[case::integer("library_prefix = 1")]
    fn test_compile_rejects_an_invalid_library_prefix(#[case] toml: &str) {
        let err = IndexSettings::compile(&toml.parse::<Table>().unwrap()).unwrap_err();
        assert!(
            err.starts_with("`library_prefix` must be true, false, or \"auto\""),
            "{err}"
        );
    }

    #[rstest]
    #[case::auto_hub(LibraryPrefix::Auto, "https://registry-1.docker.io/", "ubuntu", "library/ubuntu")]
    #[case::auto_hub_alias(LibraryPrefix::Auto, "https://index.docker.io/", "ubuntu", "library/ubuntu")]
    #[case::auto_hub_short(LibraryPrefix::Auto, "https://docker.io/", "ubuntu", "library/ubuntu")]
    #[case::auto_other(LibraryPrefix::Auto, "https://ghcr.io/", "ubuntu", "ubuntu")]
    #[case::auto_hub_multi(LibraryPrefix::Auto, "https://registry-1.docker.io/", "acme/app", "acme/app")]
    #[case::always_other(LibraryPrefix::Always, "https://mirror.example/", "ubuntu", "library/ubuntu")]
    #[case::always_multi(LibraryPrefix::Always, "https://mirror.example/", "acme/app", "acme/app")]
    #[case::never_hub(LibraryPrefix::Never, "https://registry-1.docker.io/", "ubuntu", "ubuntu")]
    #[case::auto_unparseable_base(LibraryPrefix::Auto, "not a url", "ubuntu", "ubuntu")]
    fn test_upstream_repo_rewrites_only_a_single_segment_hub_name(
        #[case] prefix: LibraryPrefix,
        #[case] base: &str,
        #[case] repo: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(upstream_repo(prefix, base, repo), expected);
    }
}
