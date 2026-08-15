//! Creation and rotation expose secrets once; stores retain a [`TokenVerifier`]. A 256-bit secret makes
//! SHA-256 verification sufficient without password-strength key derivation.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const SECRET_BYTES: usize = 32;

/// The prefix lets people and scanners recognize leaked credentials without reusing an ecosystem's
/// token prefix.
const SECRET_PREFIX: &str = "peryx_";

/// Bounds token-name storage while allowing descriptive names.
const MAX_NAME_BYTES: usize = 256;

/// An opaque scoped-token identifier, stable across rotation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenId(String);

impl TokenId {
    #[must_use]
    pub fn random() -> Self {
        Self(format!("tok_{}", uuid::Uuid::new_v4().simple()))
    }

    /// Client-supplied IDs remain untrusted lookup keys; unknown values resolve to no token.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenName(String);

impl TokenName {
    /// # Errors
    /// Returns [`TokenNameError::Empty`] for whitespace and [`TokenNameError::TooLong`] when the trimmed value exceeds
    /// the accepted length.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenNameError {
    #[error("token name cannot be empty")]
    Empty,
    #[error("token name cannot exceed {MAX_NAME_BYTES} bytes")]
    TooLong,
}

/// A plaintext token secret with redacted debug output and no serialization support.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenSecret(String);

impl TokenSecret {
    /// Generates a 256-bit secret with the peryx prefix.
    ///
    /// # Panics
    /// Panics when the operating system CSPRNG fails.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; SECRET_BYTES];
        getrandom::fill(&mut bytes).expect("the platform CSPRNG is available");
        Self(format!("{SECRET_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
    }

    #[must_use]
    pub fn presented(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the plaintext that clients store after creation or rotation.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

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

/// The persisted SHA-256 proof of a token secret, with redacted debug output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenVerifier(String);

impl TokenVerifier {
    /// Stores index this digest to resolve a presented secret with one read and no write.
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
