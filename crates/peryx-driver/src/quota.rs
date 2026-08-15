//! Quota status derives headroom from persisted counters and reports unlimited capacity as `null`.
//! Counters use repository names; responses use caller-facing routes.

use peryx_storage::meta::{QuotaUsage, QuotaValue};
use serde::Serialize;

use crate::Index;

/// One repository's configured limits alongside its committed and reserved counters.
///
/// The `accounted_bytes` and `resources` meters carry the repository-level caps a write is admitted
/// against; `artifact_bytes` is the logical footprint, which no repository-level limit bounds. The per-artifact
/// and per-resource caps that admission also uses appear under `limits`, since neither pairs with a
/// repository-wide counter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositoryQuota {
    pub repository: String,
    pub ecosystem: String,
    pub limits: RepositoryLimits,
    pub artifact_bytes: QuotaMeter,
    pub accounted_bytes: QuotaMeter,
    pub resources: QuotaMeter,
}

#[must_use]
pub fn repository_quota(index: &Index, usage: &QuotaUsage) -> RepositoryQuota {
    let policy = &index.policy;
    RepositoryQuota {
        repository: index.route.clone(),
        ecosystem: index.ecosystem.as_str().to_owned(),
        limits: RepositoryLimits {
            max_artifact_bytes: policy.max_artifact_size(),
            max_resource_bytes: policy.max_resource_size(),
            max_accounted_bytes: policy.max_accounted_bytes(),
            max_resources: policy.max_resources(),
            max_groups_per_resource: policy.max_groups_per_resource(),
            audit: policy.quota_audit(),
        },
        artifact_bytes: QuotaMeter::new(usage.artifact_bytes, None),
        accounted_bytes: QuotaMeter::new(usage.accounted_bytes, policy.max_accounted_bytes()),
        resources: QuotaMeter::new(usage.resources, policy.max_resources()),
    }
}

/// Every limit an index configures, each `null` when it sets none. `audit` reports whether a crossed
/// limit records a violation instead of refusing the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RepositoryLimits {
    pub max_artifact_bytes: Option<u64>,
    pub max_resource_bytes: Option<u64>,
    pub max_accounted_bytes: Option<u64>,
    pub max_resources: Option<u64>,
    pub max_groups_per_resource: Option<u64>,
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
