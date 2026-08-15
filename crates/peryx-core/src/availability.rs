use std::time::SystemTime;

use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobDurability {
    Filesystem,
    ObjectStore,
}

impl BlobDurability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Filesystem => "filesystem",
            Self::ObjectStore => "object-store",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobMetadata {
    pub bytes: u64,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest(String);

impl Digest {
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self::from_sha256(Sha256::digest(bytes).into())
    }

    #[must_use]
    pub fn from_sha256(bytes: [u8; 32]) -> Self {
        Self(to_hex(&bytes))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Some(Self(hex.to_owned()))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurabilityRequirement {
    pub conditional_create: bool,
    pub checksum_verified: bool,
}

impl DurabilityRequirement {
    pub const LOCAL: Self = Self {
        conditional_create: false,
        checksum_verified: false,
    };
    pub const REPLICATED: Self = Self {
        conditional_create: true,
        checksum_verified: true,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalCommit(u64);

impl JournalCommit {
    #[must_use]
    pub const fn new(serial: u64) -> Self {
        Self(serial)
    }

    #[must_use]
    pub const fn serial(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedFrontier {
    pub replica: Option<u64>,
    pub backup: Option<u64>,
}

impl ObservedFrontier {
    #[must_use]
    pub const fn covers(&self, required: u64) -> bool {
        (match self.replica {
            Some(frontier) => frontier >= required,
            None => true,
        }) && (match self.backup {
            Some(frontier) => frontier >= required,
            None => true,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct AvailabilityReadError {
    message: String,
}

impl AvailabilityReadError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub trait AnalyticsSnapshotStore: Send + Sync {
    /// # Errors
    /// Returns the persistence failure without exposing its backend type.
    fn load_analytics_snapshot(&self) -> Result<Option<Vec<u8>>, AvailabilityReadError>;
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
#[path = "../tests/unit/availability/tests.rs"]
mod tests;
