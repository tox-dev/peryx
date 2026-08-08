//! Ecosystem-neutral artifact lifecycle records.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::Ecosystem;

/// How long a soft-deleted artifact stays restorable before a retention sweep may reclaim it.
///
/// Peryx keeps trashed content for this window so an operator can inspect and undo a deletion; past it
/// a record reads as expired and restore is no longer guaranteed.
pub const TRASH_GRACE_SECS: i64 = 30 * 24 * 60 * 60;

/// Provenance retained when an artifact is soft-deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashInfo {
    /// When the artifact was trashed, as a Unix timestamp.
    pub deleted_at_unix: i64,
    /// The token or actor that deleted it, when the request carried an identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// The operator's stated reason, when the delete request supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Whether a trashed artifact can still be restored, or its recovery window has closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrashState {
    /// The content and the live slot a restore reclaims are available, within the recovery window.
    Restorable,
    /// The recovery window closed, or the content or slot a restore needs is gone.
    Expired,
}

impl TrashState {
    /// The stable lowercase identifier used in the API and the UI filter.
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

/// A string did not name a known [`TrashState`].
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

/// One soft-deleted artifact in ecosystem-neutral form, produced by each ecosystem's trash scan.
///
/// The scans merge into one inspection view. Restorability is derived, not stored:
/// [`retained`](Self::retained) is the driver's read of whether the content and the live slot a restore
/// reclaims are still there, and the recovery deadline follows [`deleted_at_unix`](Self::deleted_at_unix)
/// plus [`TRASH_GRACE_SECS`], so a query computes the same answer everywhere without a second store call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashRecord {
    /// The ecosystem that owns the deleted artifact.
    pub ecosystem: Ecosystem,
    /// The peryx index the artifact was deleted from.
    pub repository: String,
    /// The artifact's project or repository path.
    pub name: String,
    /// The distribution filename or reference; absent when the trashed record has neither.
    pub reference: Option<String>,
    /// The content digest, when the ecosystem addresses the artifact by one.
    pub digest: Option<String>,
    /// The operator's stated deletion reason, when one was supplied.
    pub reason: Option<String>,
    /// The actor that deleted it, when the request carried an identity. Role-filtered in responses.
    pub actor: Option<String>,
    /// When the artifact was trashed, as Unix seconds.
    pub deleted_at_unix: i64,
    /// Whether the retained content and the live slot a restore reclaims are both still present.
    pub retained: bool,
}

impl TrashRecord {
    /// When the recovery window closes and a retention sweep may reclaim this record.
    #[must_use]
    pub const fn deadline_unix(&self) -> i64 {
        self.deleted_at_unix.saturating_add(TRASH_GRACE_SECS)
    }

    /// Whether the artifact can still be restored at `now_unix`: its content and slot are retained and
    /// the recovery window is still open.
    #[must_use]
    pub const fn restorable(&self, now_unix: i64) -> bool {
        self.retained && now_unix < self.deadline_unix()
    }

    /// The record's derived recovery state at `now_unix`.
    #[must_use]
    pub const fn state(&self, now_unix: i64) -> TrashState {
        if self.restorable(now_unix) {
            TrashState::Restorable
        } else {
            TrashState::Expired
        }
    }

    /// A total-order pagination key: newest deletion first, then a stable identity tiebreak, so a page
    /// boundary holds even as another artifact enters trash. A cursor is one record's key; the next
    /// page returns records whose key sorts strictly after it.
    #[must_use]
    pub fn cursor(&self) -> String {
        format!(
            "{:019}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            i64::MAX - self.deleted_at_unix,
            self.ecosystem.as_str(),
            self.repository,
            self.name,
            self.reference.as_deref().unwrap_or_default(),
            self.digest.as_deref().unwrap_or_default(),
        )
    }
}

#[cfg(test)]
#[path = "../tests/unit/lifecycle/tests.rs"]
mod tests;
