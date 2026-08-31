//! The canonical peer base URL every distributed transport dials.

use std::fmt;

use url::Url;

/// A scheme peryx never registers, so [`Url`] keeps a port that `http` or `https` would elide as its
/// own default.
const PROBE_SCHEME: &str = "peryx-member";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MemberEndpointError {
    #[error("member address {0:?} is not a valid URL")]
    Malformed(String),
    #[error("member address {0:?} must use the http or https scheme")]
    Scheme(String),
    #[error("member address {0:?} needs an explicit `host:port`")]
    MissingPort(String),
    #[error("member address {0:?} must be a bare scheme://host:port with no path, query, fragment, or credentials")]
    ExtraComponents(String),
}

/// One member address reduced to the single spelling every peer transport, roster, and duplicate check
/// compares.
///
/// Two configured strings that name the same socket under the same scheme produce the same endpoint.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemberEndpoint(String);

impl MemberEndpoint {
    /// # Errors
    /// Returns [`MemberEndpointError`] unless `address` is an http or https URL with an explicit port and
    /// no component the transport would discard.
    pub fn parse(address: &str) -> Result<Self, MemberEndpointError> {
        let url = Url::parse(address).map_err(|_| MemberEndpointError::Malformed(address.to_owned()))?;
        let ("http" | "https", Some(host)) = (url.scheme(), url.host_str()) else {
            return Err(MemberEndpointError::Scheme(address.to_owned()));
        };
        if url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(MemberEndpointError::ExtraComponents(address.to_owned()));
        }
        let Some(port) = explicit_port(address) else {
            return Err(MemberEndpointError::MissingPort(address.to_owned()));
        };
        Ok(Self(format!("{}://{host}:{port}/", url.scheme())))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for MemberEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// [`Url`] drops a port that matches its scheme's default, which would let `https://peer:443` and
/// `https://peer` disagree about whether an operator wrote a port. Re-parsing the authority under a
/// scheme with no default keeps the written port visible.
fn explicit_port(address: &str) -> Option<u16> {
    let (_, authority) = address.split_once("://")?;
    Url::parse(&format!("{PROBE_SCHEME}://{authority}")).ok()?.port()
}
