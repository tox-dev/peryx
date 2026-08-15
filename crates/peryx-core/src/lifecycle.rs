use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{ArtifactKey, Ecosystem, RepositoryKey, ResourceKey};

/// Retention sweeps cannot reclaim artifacts during this recovery window.
pub const TRASH_GRACE_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashInfo {
    pub deleted_at_unix: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrashState {
    Restorable,
    Expired,
}

impl TrashState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Restorable => "restorable",
            Self::Expired => "expired",
        }
    }
}

impl fmt::Display for TrashState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownTrashState(pub String);

impl fmt::Display for UnknownTrashState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown trash state: {}", self.0)
    }
}

impl std::error::Error for UnknownTrashState {}

impl FromStr for TrashState {
    type Err = UnknownTrashState;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "restorable" => Ok(Self::Restorable),
            "expired" => Ok(Self::Expired),
            other => Err(UnknownTrashState(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashRecord {
    pub ecosystem: Ecosystem,
    pub repository: RepositoryKey,
    pub resource: ResourceKey,
    pub artifact: Option<ArtifactKey>,
    pub digest: Option<String>,
    pub reason: Option<String>,
    pub actor: Option<String>,
    pub deleted_at_unix: i64,
    /// False when restore cannot recover both content and its live slot.
    pub retained: bool,
}

impl TrashRecord {
    #[must_use]
    pub const fn deadline_unix(&self) -> i64 {
        self.deleted_at_unix.saturating_add(TRASH_GRACE_SECS)
    }

    #[must_use]
    pub const fn restorable(&self, now_unix: i64) -> bool {
        self.retained && now_unix < self.deadline_unix()
    }

    #[must_use]
    pub const fn state(&self, now_unix: i64) -> TrashState {
        if self.restorable(now_unix) {
            TrashState::Restorable
        } else {
            TrashState::Expired
        }
    }

    /// Uses identity as the tiebreak for equal deletion times.
    #[must_use]
    pub fn cursor(&self) -> String {
        format!(
            "{:019}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            i64::MAX - self.deleted_at_unix,
            self.ecosystem.as_str(),
            self.repository,
            self.resource,
            self.artifact.as_ref().map_or("", ArtifactKey::as_str),
            self.digest.as_deref().unwrap_or_default(),
        )
    }
}

#[cfg(test)]
#[path = "../tests/unit/lifecycle/tests.rs"]
mod tests;
