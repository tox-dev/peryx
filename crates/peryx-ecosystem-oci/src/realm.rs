//! Which token realms may be handed the credentials an index is configured with.
//!
//! A registry's `401` names the realm its client must trade a challenge at, so the registry chooses
//! where the index's `username`/`password` pair travels. Reaching a realm and disclosing a
//! credential to it are separate permissions: the guarded transport rules on the first, this rules
//! on the second. The distribution token flow puts the authorization service on its own host often
//! enough — Docker Hub answers from `registry-1.docker.io` and authenticates at `auth.docker.io` —
//! that a same-origin rule would break real deployments, so the operator names the extra origins.

use std::sync::Arc;

use toml::Value;
use url::Url;

/// The `[index.settings]` key [`TokenRealms`] is read from.
pub const TOKEN_REALMS: &str = "token_realms";

/// The token-realm origins an index presents its configured Basic credentials to.
///
/// The upstream's own origin is trusted on top of these. Each entry is a canonical
/// `scheme://host[:port]` origin, so a realm differing only in scheme or port is a different origin
/// and receives nothing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TokenRealms(Arc<[String]>);

impl TokenRealms {
    /// Compile the configured entries, each an absolute `http`/`https` origin carrying no userinfo,
    /// path, query, or fragment. Trust is only ever configured, never learned from a challenge.
    ///
    /// # Errors
    /// Returns a user-visible message when the value is not an array of well-formed origins.
    pub fn parse(value: &Value) -> Result<Self, String> {
        let Some(entries) = value.as_array() else {
            return Err(format!("`{TOKEN_REALMS}` must be an array of origins, not {value}"));
        };
        entries
            .iter()
            .map(|entry| {
                entry.as_str().map_or_else(
                    || Err(format!("`{TOKEN_REALMS}` entries must be strings, not {entry}")),
                    canonical_origin,
                )
            })
            .collect::<Result<Arc<[String]>, String>>()
            .map(Self)
    }

    /// Whether a realm at `realm` may receive the credentials configured for the upstream at
    /// `base`. The upstream origin is trusted because the operator configured it and gave peryx the
    /// credentials for it; every other origin has to be named.
    #[must_use]
    pub fn allows(&self, base: &Url, realm: &Url) -> bool {
        let realm = realm.origin().ascii_serialization();
        base.origin().ascii_serialization() == realm || self.0.contains(&realm)
    }
}

fn canonical_origin(entry: &str) -> Result<String, String> {
    let reject = |reason: &str| Err(format!("`{TOKEN_REALMS}` entry {entry:?} {reason}"));
    let Ok(url) = Url::parse(entry) else {
        return reject("is not an absolute URL");
    };
    if !matches!(url.scheme(), "http" | "https") {
        return reject("must use http or https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return reject("must not carry userinfo");
    }
    if url.path() != "/" {
        return reject("must not carry a path");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return reject("must not carry a query or fragment");
    }
    Ok(url.origin().ascii_serialization())
}

#[cfg(test)]
#[path = "../tests/unit/realm/tests.rs"]
mod tests;
