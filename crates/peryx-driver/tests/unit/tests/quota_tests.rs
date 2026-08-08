use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use peryx_storage::meta::{QuotaUsage, QuotaValue};

use crate::quota::{QuotaMeter, RepositoryLimits, repository_quota};

fn index(policy: Policy) -> Index {
    Index {
        name: "hosted".to_owned(),
        route: "root/example".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy,
        acl: IndexAcl {
            anonymous_read: false,
            tokens: Vec::new(),
        },
    }
}

fn policy(config: &PolicyConfig) -> Policy {
    Policy::compile(config, str::to_owned)
}

#[test]
fn quota_repository_quota_pairs_counters_with_configured_limits() {
    let usage = QuotaUsage {
        file_bytes: QuotaValue {
            committed: 4096,
            reserved: 512,
        },
        accounted_bytes: QuotaValue {
            committed: 3000,
            reserved: 500,
        },
        projects: QuotaValue {
            committed: 2,
            reserved: 1,
        },
    };
    let status = repository_quota(
        &index(policy(&PolicyConfig {
            max_file_size_bytes: Some(1024),
            max_project_size_bytes: Some(8192),
            max_accounted_bytes: Some(10_000),
            max_projects: Some(5),
            max_versions_per_project: Some(20),
            quota_audit: true,
            ..PolicyConfig::default()
        })),
        &usage,
    );
    assert_eq!(status.repository, "root/example");
    assert_eq!(status.ecosystem, "example");
    assert_eq!(
        status.limits,
        RepositoryLimits {
            max_file_bytes: Some(1024),
            max_project_bytes: Some(8192),
            max_accounted_bytes: Some(10_000),
            max_projects: Some(5),
            max_versions_per_project: Some(20),
            audit: true,
        }
    );
    assert_eq!(
        status.file_bytes,
        QuotaMeter {
            committed: 4096,
            reserved: 512,
            limit: None,
            remaining: None,
        }
    );
    assert_eq!(
        status.accounted_bytes,
        QuotaMeter {
            committed: 3000,
            reserved: 500,
            limit: Some(10_000),
            remaining: Some(6500),
        }
    );
    assert_eq!(
        status.projects,
        QuotaMeter {
            committed: 2,
            reserved: 1,
            limit: Some(5),
            remaining: Some(2),
        }
    );
}

#[test]
fn quota_meter_floors_headroom_when_audited_use_exceeds_its_limit() {
    let usage = QuotaUsage {
        accounted_bytes: QuotaValue {
            committed: 12_000,
            reserved: 0,
        },
        ..QuotaUsage::default()
    };
    let status = repository_quota(
        &index(policy(&PolicyConfig {
            max_accounted_bytes: Some(10_000),
            quota_audit: true,
            ..PolicyConfig::default()
        })),
        &usage,
    );
    assert_eq!(status.accounted_bytes.remaining, Some(0));
}

#[test]
fn quota_unlimited_repository_leaves_every_headroom_null() {
    let status = repository_quota(&index(Policy::default()), &QuotaUsage::default());
    assert_eq!(status.accounted_bytes.limit, None);
    assert_eq!(status.accounted_bytes.remaining, None);
    assert_eq!(status.projects.remaining, None);
    assert!(!status.limits.audit);
}
