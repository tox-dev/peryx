use std::collections::HashMap;
use std::path::{Path, PathBuf};

use redb::{Database, ReadableDatabase as _, ReadableTable as _, TableDefinition, TableHandle as _};
use serde_json::{Value, json};
use tempfile::TempDir;

use peryx_storage::meta::{MetaError, MetaStore, MetadataMigrationError, MetadataMigrationReport};

use super::*;
use crate::registration;

fn record(key: &str, value: impl serde::Serialize) -> MetadataRecord {
    MetadataRecord {
        key: key.to_owned(),
        value: serde_json::to_vec(&value).unwrap(),
    }
}

fn rewritten(record_set: MetadataRecordSet, record: &MetadataRecord) -> MetadataRecord {
    PypiPlugin.rewrite(record_set, record).unwrap().unwrap()
}

fn value(record: &MetadataRecord) -> Value {
    serde_json::from_slice(&record.value).unwrap()
}

#[test]
fn test_migration_identifies_its_owned_record_sets() {
    assert_eq!(PypiPlugin.name(), "pypi-v1-metadata");
    assert_eq!(
        PypiPlugin.record_sets(),
        &[
            MetadataRecordSet::QuotaUsage,
            MetadataRecordSet::QuotaResource,
            MetadataRecordSet::QuotaReservation,
            MetadataRecordSet::PolicyDecisionHistory,
            MetadataRecordSet::PolicyDecisionCurrent,
            MetadataRecordSet::PolicyDecisionCurrentById,
            MetadataRecordSet::Analytics,
        ]
    );
    assert_eq!(
        PypiPlugin.legacy_sources(),
        &[
            LegacyMetadataSource {
                table: "quota_project",
                value_kind: MetadataValueKind::Bytes,
                target: MetadataRecordSet::QuotaResource,
            },
            LegacyMetadataSource {
                table: "quota_version",
                value_kind: MetadataValueKind::Bytes,
                target: MetadataRecordSet::QuotaGroup,
            },
        ]
    );
    assert_eq!(registration().metadata_migration.unwrap().name(), PypiPlugin.name());
}

#[test]
fn test_migration_copies_legacy_group_usage() {
    let original = record("repo/resource/group", json!({"committed": 1, "reserved": 2}));
    assert_eq!(rewritten(MetadataRecordSet::QuotaGroup, &original), original);
}

#[test]
fn test_migration_rewrites_legacy_quota_usage() {
    let migrated = rewritten(
        MetadataRecordSet::QuotaUsage,
        &record(
            "repo",
            json!({
                "file_bytes": {"committed": 1, "reserved": 2},
                "accounted_bytes": {"committed": 3, "reserved": 4},
                "projects": {"committed": 5, "reserved": 6}
            }),
        ),
    );

    assert_eq!(
        value(&migrated),
        json!({
            "artifact_bytes": {"committed": 1, "reserved": 2},
            "accounted_bytes": {"committed": 3, "reserved": 4},
            "resources": {"committed": 5, "reserved": 6}
        })
    );
}

#[test]
fn test_migration_rewrites_legacy_resource_usage() {
    let migrated = rewritten(
        MetadataRecordSet::QuotaResource,
        &record(
            "repo/resource",
            json!({
                "references": {"committed": 1, "reserved": 2},
                "file_bytes": {"committed": 3, "reserved": 4},
                "versions": {"committed": 5, "reserved": 6}
            }),
        ),
    );

    assert_eq!(
        value(&migrated),
        json!({
            "references": {"committed": 1, "reserved": 2},
            "artifact_bytes": {"committed": 3, "reserved": 4},
            "groups": {"committed": 5, "reserved": 6}
        })
    );
}

#[test]
fn test_migration_rewrites_legacy_quota_reservations() {
    let migrated = rewritten(
        MetadataRecordSet::QuotaReservation,
        &record(
            "allocation",
            json!({
                "id": "00000000-0000-0000-0000-000000000001",
                "repository": "repo",
                "project": "resource",
                "version": "group",
                "digest": "sha256:1",
                "bytes": 7,
                "class": "hosted",
                "state": "reserved",
                "created_at_unix": 8,
                "violations": ["file_bytes", "accounted_bytes", "projects", "versions_per_project"],
                "project_file_bytes": true
            }),
        ),
    );

    assert_eq!(
        value(&migrated),
        json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "repository": "repo",
            "resource": "resource",
            "group": "group",
            "digest": "sha256:1",
            "bytes": 7,
            "class": "hosted",
            "state": "reserved",
            "created_at_unix": 8,
            "violations": ["artifact_bytes", "accounted_bytes", "resources", "groups_per_resource"],
            "resource_artifact_bytes": true
        })
    );
}

#[test]
fn test_migration_rewrites_legacy_policy_history() {
    let migrated = rewritten(
        MetadataRecordSet::PolicyDecisionHistory,
        &record(
            "decision",
            json!({
                "id": "00000000-0000-0000-0000-000000000002",
                "repository": "repo",
                "project": "resource",
                "version": "group",
                "filename": "artifact",
                "source": "upstream",
                "action": "serve",
                "state": "allow",
                "rule": "rule",
                "reason": "reason",
                "evaluated_at_unix": 9,
                "input_generation": {"repository": 1, "catalog": 2, "policy": 3},
                "next_eligible_at_unix": 10
            }),
        ),
    );

    assert_eq!(
        value(&migrated),
        json!({
            "id": "00000000-0000-0000-0000-000000000002",
            "repository": "repo",
            "resource": "resource",
            "group": "group",
            "artifact": "artifact",
            "source": "upstream",
            "action": "serve",
            "state": "allow",
            "rule": "rule",
            "reason": "reason",
            "evaluated_at_unix": 9,
            "input_generation": {"repository": 1, "catalog": 2, "policy": 3},
            "next_eligible_at_unix": 10
        })
    );
}

#[rstest::rstest]
#[case::subject_key(MetadataRecordSet::PolicyDecisionCurrent, true)]
#[case::subject_value(MetadataRecordSet::PolicyDecisionCurrentById, false)]
fn test_migration_rewrites_legacy_policy_subjects(#[case] record_set: MetadataRecordSet, #[case] key: bool) {
    let subject = json!({
        "repository": "repo",
        "project": "resource",
        "version": "group",
        "filename": "artifact",
        "source": "upstream",
        "action": "serve"
    });
    let record = if key {
        MetadataRecord {
            key: subject.to_string(),
            value: b"decision".to_vec(),
        }
    } else {
        MetadataRecord {
            key: "decision".to_owned(),
            value: serde_json::to_vec(&subject).unwrap(),
        }
    };
    let migrated = rewritten(record_set, &record);
    let migrated_subject = if key {
        serde_json::from_str(&migrated.key).unwrap()
    } else {
        value(&migrated)
    };

    assert_eq!(
        migrated_subject,
        json!({
            "repository": "repo",
            "resource": "resource",
            "group": "group",
            "artifact": "artifact",
            "source": "upstream",
            "action": "serve"
        })
    );
}

#[test]
fn test_migration_rewrites_legacy_read_analytics() {
    let migrated = rewritten(
        MetadataRecordSet::Analytics,
        &record(
            "downloads",
            json!({
                "files": [{
                    "route": "repo",
                    "project": "resource",
                    "filename": "artifact",
                    "downloads": 11,
                    "bytes": 12
                }]
            }),
        ),
    );

    assert_eq!(migrated.key, "reads");
    assert_eq!(
        value(&migrated),
        json!({
            "artifacts": [{
                "repository": "repo",
                "resource": "resource",
                "artifact": "artifact",
                "reads": 11,
                "bytes": 12
            }]
        })
    );
}

#[test]
fn test_migration_rewrites_legacy_daily_analytics() {
    let migrated = rewritten(
        MetadataRecordSet::Analytics,
        &record(
            "daily_usage",
            json!({
                "schema": 1,
                "buckets": [{
                    "day": 1,
                    "repository": "repo",
                    "project": "resource",
                    "version": "group",
                    "source": "upstream",
                    "downloads": 13,
                    "bytes": 14
                }]
            }),
        ),
    );

    assert_eq!(
        value(&migrated),
        json!({
            "schema": 1,
            "buckets": [{
                "day": 1,
                "repository": "repo",
                "resource": "resource",
                "group": "group",
                "source": "upstream",
                "reads": 13,
                "bytes": 14
            }]
        })
    );
}

#[rstest::rstest]
#[case::current_quota(
    MetadataRecordSet::QuotaUsage,
    "repo",
    json!({
        "artifact_bytes": {"committed": 1, "reserved": 2},
        "accounted_bytes": {"committed": 3, "reserved": 4},
        "resources": {"committed": 5, "reserved": 6}
    })
)]
#[case::current_resource(
    MetadataRecordSet::QuotaResource,
    "repo/resource",
    json!({
        "references": {"committed": 1, "reserved": 2},
        "artifact_bytes": {"committed": 3, "reserved": 4},
        "groups": {"committed": 5, "reserved": 6}
    })
)]
#[case::current_reservation(
    MetadataRecordSet::QuotaReservation,
    "allocation",
    json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "repository": "repo",
        "resource": "resource",
        "group": "group",
        "digest": "sha256:1",
        "bytes": 7,
        "class": "hosted",
        "state": "reserved",
        "created_at_unix": 8,
        "violations": ["artifact_bytes"],
        "resource_artifact_bytes": true
    })
)]
#[case::current_policy_history(
    MetadataRecordSet::PolicyDecisionHistory,
    "decision",
    json!({
        "id": "00000000-0000-0000-0000-000000000002",
        "repository": "repo",
        "resource": "resource",
        "group": "group",
        "artifact": "artifact",
        "source": "upstream",
        "action": "serve",
        "state": "allow",
        "rule": "rule",
        "reason": "reason",
        "evaluated_at_unix": 9,
        "input_generation": {"repository": 1, "catalog": 2, "policy": 3},
        "next_eligible_at_unix": 10
    })
)]
#[case::current_reads(MetadataRecordSet::Analytics, "reads", json!({"artifacts": []}))]
#[case::current_daily(MetadataRecordSet::Analytics, "daily_usage", json!({"schema": 1, "buckets": []}))]
#[case::unknown_analytics(MetadataRecordSet::Analytics, "other", json!({}))]
#[case::malformed_legacy(MetadataRecordSet::QuotaUsage, "repo", json!({"file_bytes": false}))]
fn test_migration_leaves_current_or_unowned_records_unchanged(
    #[case] record_set: MetadataRecordSet,
    #[case] key: &str,
    #[case] contents: Value,
) {
    assert_eq!(PypiPlugin.rewrite(record_set, &record(key, contents)), Ok(None));
}

#[rstest::rstest]
#[case::subject_key(MetadataRecordSet::PolicyDecisionCurrent, true)]
#[case::subject_value(MetadataRecordSet::PolicyDecisionCurrentById, false)]
fn test_migration_leaves_current_policy_subjects_unchanged(#[case] record_set: MetadataRecordSet, #[case] key: bool) {
    let subject = json!({
        "repository": "repo",
        "resource": "resource",
        "group": "group",
        "artifact": "artifact",
        "source": "upstream",
        "action": "serve"
    });
    let record = if key {
        MetadataRecord {
            key: subject.to_string(),
            value: b"decision".to_vec(),
        }
    } else {
        MetadataRecord {
            key: "decision".to_owned(),
            value: serde_json::to_vec(&subject).unwrap(),
        }
    };

    assert_eq!(PypiPlugin.rewrite(record_set, &record), Ok(None));
}

#[rstest::rstest]
#[case::subject_key(MetadataRecordSet::PolicyDecisionCurrent, true)]
#[case::subject_value(MetadataRecordSet::PolicyDecisionCurrentById, false)]
fn test_migration_leaves_malformed_policy_subjects_unchanged(#[case] record_set: MetadataRecordSet, #[case] key: bool) {
    let record = if key {
        MetadataRecord {
            key: "malformed".to_owned(),
            value: b"decision".to_vec(),
        }
    } else {
        MetadataRecord {
            key: "decision".to_owned(),
            value: b"malformed".to_vec(),
        }
    };

    assert_eq!(PypiPlugin.rewrite(record_set, &record), Ok(None));
}

#[test]
fn test_migration_moves_owned_metadata_and_preserves_collisions() {
    let (directory, store) = store();
    drop(store);
    let path = database_path(&directory);
    write_quota_fixture(&path);
    write_policy_fixture(&path);
    write_analytics_fixture(&path);
    write_legacy_quota_fixture(&path);

    let store = MetaStore::open_existing(&path).unwrap();
    assert_eq!(
        store.migrate_metadata(&PypiPlugin).unwrap(),
        MetadataMigrationReport {
            scanned: 14,
            rewritten: 11,
        }
    );
    drop(store);

    assert_migrated_quota(&path);
    assert_migrated_policy(&path);
    assert_migrated_analytics(&path);
}

fn write_quota_fixture(path: &Path) {
    write_bytes(
        path,
        "quota_usage",
        &[(
            "repo",
            json!({
                "file_bytes": {"committed": 1, "reserved": 2},
                "accounted_bytes": {"committed": 3, "reserved": 4},
                "projects": {"committed": 5, "reserved": 6}
            }),
        )],
    );
    write_bytes(
        path,
        "quota_resource",
        &[(
            "repo/collision",
            json!({
                "references": {"committed": 7, "reserved": 8},
                "artifact_bytes": {"committed": 9, "reserved": 10},
                "groups": {"committed": 11, "reserved": 12}
            }),
        )],
    );
    write_bytes(
        path,
        "quota_reservation",
        &[(
            "allocation",
            json!({
                "id": "00000000-0000-0000-0000-000000000001",
                "repository": "repo",
                "project": "resource",
                "version": "group",
                "digest": "sha256:1",
                "bytes": 13,
                "class": "hosted",
                "state": "reserved",
                "created_at_unix": 14,
                "violations": ["versions_per_project"],
                "project_file_bytes": true
            }),
        )],
    );
}

fn write_policy_fixture(path: &Path) {
    write_bytes(
        path,
        "policy_decision",
        &[(
            "decision",
            json!({
                "id": "00000000-0000-0000-0000-000000000002",
                "repository": "repo",
                "project": "resource",
                "version": "group",
                "filename": "artifact",
                "source": "upstream",
                "action": "serve",
                "state": "allow",
                "rule": null,
                "reason": null,
                "evaluated_at_unix": 15,
                "input_generation": {"repository": 1, "catalog": 2, "policy": 3},
                "next_eligible_at_unix": null
            }),
        )],
    );
    let legacy_subject = json!({
        "repository": "repo",
        "project": "resource",
        "version": "group",
        "filename": "artifact",
        "source": "upstream",
        "action": "serve"
    });
    write_text(
        path,
        "policy_decision_current",
        &[(legacy_subject.to_string(), "decision".to_owned())],
    );
    write_text(
        path,
        "policy_decision_current_id",
        &[("decision".to_owned(), legacy_subject.to_string())],
    );
}

fn write_analytics_fixture(path: &Path) {
    write_bytes(
        path,
        "analytics",
        &[
            (
                "downloads",
                json!({
                    "files": [{
                        "route": "repo",
                        "project": "resource",
                        "filename": "legacy",
                        "downloads": 16,
                        "bytes": 17
                    }]
                }),
            ),
            (
                "reads",
                json!({"artifacts": [{"repository": "repo", "resource": "resource", "artifact": "current", "reads": 18, "bytes": 19}]}),
            ),
            (
                "daily_usage",
                json!({
                    "schema": 1,
                    "buckets": [{
                        "day": 20,
                        "repository": "repo",
                        "project": "resource",
                        "version": "group",
                        "source": "upstream",
                        "downloads": 21,
                        "bytes": 22
                    }]
                }),
            ),
        ],
    );
}

fn write_legacy_quota_fixture(path: &Path) {
    write_bytes(
        path,
        "quota_project",
        &[
            (
                "repo/moved",
                json!({
                    "references": {"committed": 23, "reserved": 24},
                    "versions": {"committed": 25, "reserved": 26}
                }),
            ),
            (
                "repo/collision",
                json!({
                    "references": {"committed": 27, "reserved": 28},
                    "file_bytes": {"committed": 29, "reserved": 30},
                    "versions": {"committed": 31, "reserved": 32}
                }),
            ),
            ("repo/malformed", json!({"references": false})),
        ],
    );
    write_bytes(
        path,
        "quota_version",
        &[
            ("repo/resource/one", json!({"committed": 33, "reserved": 34})),
            ("repo/resource/two", json!({"committed": 35, "reserved": 36})),
        ],
    );
}

fn assert_migrated_quota(path: &Path) {
    assert_eq!(
        value_bytes(&read_bytes(path, "quota_usage")["repo"]),
        json!({
            "artifact_bytes": {"committed": 1, "reserved": 2},
            "accounted_bytes": {"committed": 3, "reserved": 4},
            "resources": {"committed": 5, "reserved": 6}
        })
    );
    let resources = read_bytes(path, "quota_resource");
    assert_eq!(
        value_bytes(&resources["repo/collision"]),
        json!({
            "references": {"committed": 7, "reserved": 8},
            "artifact_bytes": {"committed": 9, "reserved": 10},
            "groups": {"committed": 11, "reserved": 12}
        })
    );
    assert_eq!(
        value_bytes(&resources["repo/moved"]),
        json!({
            "references": {"committed": 23, "reserved": 24},
            "artifact_bytes": {"committed": 0, "reserved": 0},
            "groups": {"committed": 25, "reserved": 26}
        })
    );
    assert_eq!(
        read_bytes(path, "quota_group"),
        HashMap::from([
            (
                "repo/resource/one".to_owned(),
                json_bytes(json!({"committed": 33, "reserved": 34})),
            ),
            (
                "repo/resource/two".to_owned(),
                json_bytes(json!({"committed": 35, "reserved": 36})),
            ),
        ])
    );
    assert_eq!(
        value_bytes(&read_bytes(path, "quota_reservation")["allocation"])["resource"],
        "resource"
    );
    assert_eq!(
        read_bytes(path, "quota_project"),
        HashMap::from([("repo/malformed".to_owned(), json_bytes(json!({"references": false})))])
    );
    assert!(!table_names(path).contains(&"quota_version".to_owned()));
}

fn assert_migrated_policy(path: &Path) {
    assert_eq!(
        value_bytes(&read_bytes(path, "policy_decision")["decision"])["artifact"],
        "artifact"
    );
    let current_subjects = read_text(path, "policy_decision_current");
    let (subject, decision) = current_subjects.iter().next().unwrap();
    assert_eq!(
        (
            current_subjects.len(),
            serde_json::from_str::<Value>(subject).unwrap(),
            decision.as_str(),
        ),
        (
            1,
            json!({
                "repository": "repo",
                "resource": "resource",
                "group": "group",
                "artifact": "artifact",
                "source": "upstream",
                "action": "serve"
            }),
            "decision",
        )
    );
    assert_eq!(
        value_bytes(read_text(path, "policy_decision_current_id")["decision"].as_bytes())["resource"],
        "resource"
    );
}

fn assert_migrated_analytics(path: &Path) {
    let analytics = read_bytes(path, "analytics");
    assert_eq!(value_bytes(&analytics["reads"])["artifacts"][0]["artifact"], "current");
    assert_eq!(value_bytes(&analytics["daily_usage"])["buckets"][0]["reads"], 21);
}

#[test]
fn test_migration_reports_a_legacy_table_type_error_without_modifying_it() {
    let (directory, store) = store();
    drop(store);
    let path = database_path(&directory);
    write_text(
        &path,
        "quota_project",
        &[("repo/resource".to_owned(), "malformed".to_owned())],
    );
    let store = MetaStore::open_existing(&path).unwrap();

    assert!(matches!(
        store.migrate_metadata(&PypiPlugin),
        Err(MetadataMigrationError::Store(MetaError::Table(
            redb::TableError::TableTypeMismatch { .. }
        )))
    ));
    drop(store);
    assert_eq!(
        read_text(&path, "quota_project"),
        HashMap::from([("repo/resource".to_owned(), "malformed".to_owned())])
    );
}

fn store() -> (TempDir, MetaStore) {
    let directory = TempDir::new().unwrap();
    let store = MetaStore::open(database_path(&directory)).unwrap();
    (directory, store)
}

fn database_path(directory: &TempDir) -> PathBuf {
    directory.path().join("metadata.redb")
}

fn write_bytes(path: &Path, table: &'static str, records: &[(&str, Value)]) {
    let database = Database::open(path).unwrap();
    let transaction = database.begin_write().unwrap();
    {
        let mut table = transaction
            .open_table(TableDefinition::<&str, &[u8]>::new(table))
            .unwrap();
        for (key, value) in records {
            table.insert(*key, json_bytes(value.clone()).as_slice()).unwrap();
        }
    }
    transaction.commit().unwrap();
}

fn write_text(path: &Path, table: &'static str, records: &[(String, String)]) {
    let database = Database::open(path).unwrap();
    let transaction = database.begin_write().unwrap();
    {
        let mut table = transaction
            .open_table(TableDefinition::<&str, &str>::new(table))
            .unwrap();
        for (key, value) in records {
            table.insert(key.as_str(), value.as_str()).unwrap();
        }
    }
    transaction.commit().unwrap();
}

fn read_bytes(path: &Path, table: &'static str) -> HashMap<String, Vec<u8>> {
    let database = Database::open(path).unwrap();
    let transaction = database.begin_read().unwrap();
    transaction
        .open_table(TableDefinition::<&str, &[u8]>::new(table))
        .unwrap()
        .iter()
        .unwrap()
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.value().to_owned(), value.value().to_vec())
        })
        .collect()
}

fn read_text(path: &Path, table: &'static str) -> HashMap<String, String> {
    let database = Database::open(path).unwrap();
    let transaction = database.begin_read().unwrap();
    transaction
        .open_table(TableDefinition::<&str, &str>::new(table))
        .unwrap()
        .iter()
        .unwrap()
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.value().to_owned(), value.value().to_owned())
        })
        .collect()
}

fn table_names(path: &Path) -> Vec<String> {
    let database = Database::open(path).unwrap();
    database
        .begin_read()
        .unwrap()
        .list_tables()
        .unwrap()
        .map(|table| table.name().to_owned())
        .collect()
}

fn json_bytes(value: impl serde::Serialize) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

fn value_bytes(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}
