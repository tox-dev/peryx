//! Identity and access for peryx.
//!
//! A request arrives with a credential, or with none; an index declares who may do what. This crate
//! holds both halves and the one decision between them: [`IndexAcl::identify`] turns a credential into
//! a [`Principal`], and [`authorize`] says whether that principal may take an [`Action`] on a project
//! in an index. Every ecosystem calls the same entry point, so the access rules live in one place
//! instead of once per wire protocol.
//!
//! The model is neutral by construction. It knows a principal, an index ACL, a project name and an
//! action, and nothing about how a client presented itself: an OCI scope string, a `PyPI` Basic header
//! and a bearer token all reduce to those four before they reach [`authorize`]. A grant carries project
//! globs (`team/*`), which match `PyPI` project names and OCI repository names alike.
//!
//! [`Signer`] mints and verifies the JWTs a token realm hands out. It lives here because the signing
//! key is identity state, not protocol state: an ecosystem's token endpoint calls `mint` with the
//! grants [`authorize`] approved and never sees the key.

mod acl;
mod token;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

pub use acl::{Action, Denial, Glob, Grant, Identity, IndexAcl, NamedToken, Principal, UPLOAD_TOKEN_NAME, authorize};
pub use token::{Signer, TokenError};

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

/// Compare a presented secret to a configured one without an early-out on the first differing byte, so
/// the server does not leak how much of the secret a guess got right through its response time. The
/// length is not secret, so a length mismatch may short-circuit. `black_box` keeps the optimizer from
/// reintroducing the short-circuit.
fn secrets_match(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    let mut difference = 0u8;
    for (presented, expected) in presented.iter().zip(expected) {
        difference |= presented ^ expected;
    }
    std::hint::black_box(difference) == 0
}

#[cfg(test)]
mod tests;
