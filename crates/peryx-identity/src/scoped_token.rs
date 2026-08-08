//! Scoped API tokens: a named, expiring credential carrying a fixed reach and action set, minted once
//! and thereafter proven by a stored verifier the server checks without a database write.
//!
//! The secret is shown once, at creation or rotation, and never again; only its [`TokenVerifier`] - a
//! SHA-256 digest - is persisted, so a leaked store discloses no usable credential. Verification hashes
//! the presented secret and resolves the one token whose stored digest keys it, a single indexed read
//! and no write. The secret carries 256 bits of entropy, so a fast digest is a sound verifier and a
//! per-request check need not pay a memory-hard password derivation; the digest, not the secret, is
//! what a lookup compares, so the comparison discloses nothing about the credential.
//!
//! The reach ([`GrantScope`]) and [`Action`] set are the same types the per-index ACL and the role
//! model already speak, so a scoped token bridges the config-only `NamedToken` grants to a persisted,
//! authorized lifecycle without a parallel vocabulary.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Random bytes behind a token secret. 256 bits makes a guess infeasible and a plain digest a sound
/// verifier, so verification need not run a memory-hard derivation on the request path.
const SECRET_BYTES: usize = 32;

/// The prefix every token secret carries, so a leaked credential is recognizable to a human and to a
/// secret scanner without borrowing a client ecosystem's token prefix.
const SECRET_PREFIX: &str = "peryx_";

/// The longest a token name may be, in bytes; long enough to describe a use, short enough to bound a row.
const MAX_NAME_BYTES: usize = 256;

/// An opaque scoped-token identifier, stable across rotation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenId(String);

impl TokenId {
    /// Generate a random token identifier.
    #[must_use]
    pub fn random() -> Self {
        Self(format!("tok_{}", uuid::Uuid::new_v4().simple()))
    }

    /// Adopt a client-supplied identifier verbatim for a lookup; an unknown value resolves to no
    /// token, so the value is never trusted beyond being a key.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TokenId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A validated token name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenName(String);

impl TokenName {
    /// Preserve the trimmed spelling of a token name.
    ///
    /// # Errors
    /// Returns [`TokenNameError::Empty`] when `value` is only whitespace and [`TokenNameError::TooLong`]
    /// when it exceeds [`MAX_NAME_BYTES`] after trimming.
    pub fn new(value: &str) -> Result<Self, TokenNameError> {
        let name = value.trim();
        if name.is_empty() {
            return Err(TokenNameError::Empty);
        }
        if name.len() > MAX_NAME_BYTES {
            return Err(TokenNameError::TooLong);
        }
        Ok(Self(name.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A name that cannot label a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenNameError {
    #[error("token name cannot be empty")]
    Empty,
    #[error("token name cannot exceed {MAX_NAME_BYTES} bytes")]
    TooLong,
}

/// A token's plaintext secret, held only long enough to show a client once.
///
/// It is never persisted and never serialized: the store keeps only its [`TokenVerifier`], and this
/// type's [`Debug`] redacts, so a secret cannot reach a log or an API view except through the one
/// deliberate [`expose`](Self::expose) at mint time.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenSecret(String);

impl TokenSecret {
    /// Mint a fresh high-entropy secret carrying the peryx prefix.
    ///
    /// # Panics
    /// Never in practice: the platform CSPRNG is the same source password salts draw from, and a
    /// failure to read it is unrecoverable rather than a case a caller handles.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; SECRET_BYTES];
        getrandom::fill(&mut bytes).expect("the platform CSPRNG is available");
        Self(format!("{SECRET_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
    }

    /// Adopt a credential a client presented, so it can be checked against a stored verifier.
    #[must_use]
    pub fn presented(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The one deliberate disclosure of the secret: the value a client must store now, as it is never
    /// shown again.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Derive the persisted verifier for this secret.
    #[must_use]
    pub fn verifier(&self) -> TokenVerifier {
        TokenVerifier(URL_SAFE_NO_PAD.encode(Sha256::digest(self.0.as_bytes())))
    }
}

impl fmt::Debug for TokenSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TokenSecret(<redacted>)")
    }
}

/// The persisted proof of a token secret: a SHA-256 digest a presented secret must reproduce.
///
/// It is a credential secret in its own right - a preimage is the token - so its [`Debug`] redacts and
/// it never appears in an API view.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenVerifier(String);

impl TokenVerifier {
    /// The digest as a lookup key; the store indexes tokens by it to resolve a presented secret with one
    /// read and no write.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for TokenVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TokenVerifier(<redacted>)")
    }
}
