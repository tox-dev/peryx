//! The neutral read model for a repository's quota status.
//!
//! Reservation admission and counter upkeep live in `peryx-storage`; this reads the counters that
//! substrate already maintains and the limits an index configures, and pairs them into a status a
//! caller can render. It derives the remaining headroom every client would otherwise compute, and
//! leaves it `null` when a counter is unlimited so "no limit" never reads as an enormous number.
//!
//! The model accounts quota the same way for every format. Counters use an index's name, the identity a writer
//! reserves against, while the status reports the caller-facing route.

use peryx_storage::meta::{QuotaUsage, QuotaValue};
use serde::Serialize;

use crate::Index;

/// One repository's configured limits alongside its committed and reserved counters.
///
/// The `accounted_bytes` and `projects` meters carry the repository-level caps a write is admitted
/// against; `file_bytes` is the logical footprint, which no repository-level limit bounds. The per-file
/// and per-project caps that admission also uses appear under `limits`, since neither pairs with a
/// repository-wide counter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositoryQuota {
    pub repository: String,
    pub ecosystem: &'static str,
    pub limits: RepositoryLimits,
    pub file_bytes: QuotaMeter,
    pub accounted_bytes: QuotaMeter,
    pub projects: QuotaMeter,
}

/// Read the status for one index from its usage counters. The `usage` a caller passes is read from the
/// store under the index's name; the reported `repository` is the index's route.
#[must_use]
pub fn repository_quota(index: &Index, usage: &QuotaUsage) -> RepositoryQuota {
    let policy = &index.policy;
    RepositoryQuota {
        repository: index.route.clone(),
        ecosystem: index.ecosystem.as_str(),
        limits: RepositoryLimits {
            max_file_bytes: policy.max_file_size(),
            max_project_bytes: policy.max_project_size(),
            max_accounted_bytes: policy.max_accounted_bytes(),
            max_projects: policy.max_projects(),
            max_versions_per_project: policy.max_versions_per_project(),
            audit: policy.quota_audit(),
        },
        file_bytes: QuotaMeter::new(usage.file_bytes, None),
        accounted_bytes: QuotaMeter::new(usage.accounted_bytes, policy.max_accounted_bytes()),
        projects: QuotaMeter::new(usage.projects, policy.max_projects()),
    }
}

/// Every limit an index configures, each `null` when it sets none. `audit` reports whether a crossed
/// limit records a violation instead of refusing the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RepositoryLimits {
    pub max_file_bytes: Option<u64>,
    pub max_project_bytes: Option<u64>,
    pub max_accounted_bytes: Option<u64>,
    pub max_projects: Option<u64>,
    pub max_versions_per_project: Option<u64>,
    pub audit: bool,
}

/// One counter against its limit. `committed` is settled use, `reserved` is capacity held by in-flight
/// writes, and `remaining` is the headroom left once both are charged, or `null` when unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct QuotaMeter {
    pub committed: u64,
    pub reserved: u64,
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
}

impl QuotaMeter {
    #[must_use]
    fn new(value: QuotaValue, limit: Option<u64>) -> Self {
        let used = value.committed.saturating_add(value.reserved);
        Self {
            committed: value.committed,
            reserved: value.reserved,
            limit,
            remaining: limit.map(|limit| limit.saturating_sub(used)),
        }
    }
}
