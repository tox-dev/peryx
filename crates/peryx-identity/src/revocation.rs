use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

const SHA256_HEX_BYTES: usize = 64;
const MAX_REASON_BYTES: usize = 2_048;

/// Serializes as the SHA-256 member of an in-toto `DigestSet`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ArtifactDigest {
    sha256: String,
}

impl ArtifactDigest {
    /// # Errors
    /// Returns [`ArtifactDigestError`] unless `sha256` contains 64 lowercase hexadecimal bytes.
    pub fn from_sha256(sha256: impl Into<String>) -> Result<Self, ArtifactDigestError> {
        let sha256 = sha256.into();
        if sha256.len() != SHA256_HEX_BYTES
            || !sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(ArtifactDigestError);
        }
        Ok(Self { sha256 })
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    #[must_use]
    pub fn canonical(&self) -> String {
        format!("sha256:{}", self.sha256)
    }
}

impl fmt::Display for ArtifactDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "sha256:{}", self.sha256)
    }
}

impl FromStr for ArtifactDigest {
    type Err = ArtifactDigestError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_sha256(value.strip_prefix("sha256:").ok_or(ArtifactDigestError)?)
    }
}

impl<'de> Deserialize<'de> for ArtifactDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct DigestSet {
            sha256: String,
        }

        Self::from_sha256(DigestSet::deserialize(deserializer)?.sha256).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("digest must be sha256:<64 lowercase hexadecimal characters>")]
pub struct ArtifactDigestError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RevocationReason(String);

impl RevocationReason {
    /// # Errors
    /// Returns [`RevocationReasonError`] for blank text or more than 2048 UTF-8 bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, RevocationReasonError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(RevocationReasonError::Empty);
        }
        if value.len() > MAX_REASON_BYTES {
            return Err(RevocationReasonError::TooLong);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RevocationReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RevocationReasonError {
    #[error("revocation reason must not be blank")]
    Empty,
    #[error("revocation reason exceeds the 2048-byte limit")]
    TooLong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestDecision {
    Clear,
    Revoked,
}
