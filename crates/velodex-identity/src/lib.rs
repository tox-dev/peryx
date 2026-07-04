//! Identity and access for velodex.
//!
//! Today this is a single credential check: a hosted index accepts an upload when the request carries
//! the index's upload token as its HTTP Basic-auth password (the `__token__` convention pip and twine
//! use). The logic is pure — a header string in, a decision out — so it needs no HTTP or storage
//! dependency and is trivial to test.
//!
//! It is the seam the richer access model grows behind: named and scoped tokens with expiry, read
//! ACLs, macaroon-style attenuable credentials, and OIDC trusted-publishing token minting. Those land
//! as an `IdentityProvider`/verifier trait here (the serving layer already routes every upload through
//! this crate), so adding them does not touch the request handlers.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// The user and password carried by an HTTP Basic `Authorization` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicCredentials {
    pub user: String,
    pub password: String,
}

/// Parse an `Authorization` header value as HTTP Basic credentials, or `None` if it is absent, not
/// Basic, not valid base64/UTF-8, or has no `user:password` separator.
#[must_use]
pub fn parse_basic(header_value: &str) -> Option<BasicCredentials> {
    let encoded = header_value.strip_prefix("Basic ")?;
    let decoded = STANDARD.decode(encoded.trim()).ok()?;
    let credentials = String::from_utf8(decoded).ok()?;
    let (user, password) = credentials.split_once(':')?;
    Some(BasicCredentials {
        user: user.to_owned(),
        password: password.to_owned(),
    })
}

/// Whether an `Authorization` header carries the correct upload token as its Basic-auth password.
/// Any username is accepted, matching pypi's `__token__` convention where the password is the token.
#[must_use]
pub fn authorized(header: Option<&str>, token: &str) -> bool {
    header
        .and_then(parse_basic)
        .is_some_and(|credentials| credentials.password == token)
}

#[cfg(test)]
mod tests;
