use peryx_events::metrics::ResourceUsage;
use peryx_policy::{PolicyAction, PolicyDecisionState};
use peryx_storage::meta::{MetaStore, NewPolicyDecision};

use super::*;

#[test]
fn test_usage_row_projects_counts_and_saturates_large_values() {
    assert_eq!(
        usage_row(ResourceUsage {
            repository: "catalog".to_owned(),
            resource: "resource".to_owned(),
            reads: u64::MAX,
            bytes: 41,
        }),
        Row::new()
            .with("repository", Value::Str("catalog".to_owned()))
            .with("resource", Value::Str("resource".to_owned()))
            .with("reads", Value::Int(i64::MAX))
            .with("bytes", Value::Int(41))
    );
}

#[test]
fn test_policy_row_projects_present_and_absent_fields() {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let record = meta
        .record_policy_decision(NewPolicyDecision {
            repository: "catalog",
            resource: "resource",
            group: Some("group"),
            artifact: Some("artifact.bin"),
            source: Some("upstream"),
            action: PolicyAction::Serve,
            state: PolicyDecisionState::Allow,
            rule: Some("signed"),
            reason: None,
            evaluated_at_unix: 41,
            next_eligible_at_unix: None,
        })
        .unwrap();
    let item = PolicyDecisionItem {
        record: record.clone(),
        fresh: true,
    };

    assert_eq!(
        policy_row(&item),
        Row::new()
            .with("repository", Value::Str(record.repository))
            .with("resource", Value::Str(record.resource))
            .with("group", Value::Str("group".to_owned()))
            .with("artifact", Value::Str("artifact.bin".to_owned()))
            .with("source", Value::Str("upstream".to_owned()))
            .with("action", Value::Str(PolicyAction::Serve.to_string()))
            .with("state", Value::Str("allow".to_owned()))
            .with("rule", Value::Str("signed".to_owned()))
            .with("reason", Value::Null)
            .with("evaluated_at", Value::Timestamp(41))
            .with("fresh", Value::Bool(true))
    );
}

#[test]
fn test_policy_state_names_cover_each_decision() {
    assert_eq!(
        [
            policy_state_name(PolicyDecisionState::Allow),
            policy_state_name(PolicyDecisionState::Deny),
            policy_state_name(PolicyDecisionState::Wait),
        ],
        ["allow", "deny", "wait"]
    );
    assert_eq!(optional(Some("value")), Value::Str("value".to_owned()));
    assert_eq!(optional(None), Value::Null);
}
