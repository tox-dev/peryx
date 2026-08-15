use std::collections::HashMap;
use std::path::Path;

use redb::{ReadableDatabase as _, ReadableTable as _, TableDefinition, TableHandle as _};
use rstest::rstest;
use tempfile::TempDir;

use crate::meta::{
    LegacyMetadataSource, MetaError, MetaStore, MetadataMigration, MetadataMigrationError, MetadataMigrationReport,
    MetadataRecord, MetadataRecordSet, MetadataValueKind,
};

#[derive(Clone, Copy)]
enum Rewrite {
    Append,
    Rename,
    Reject,
    Skip,
}

struct Migration {
    record_sets: &'static [MetadataRecordSet],
    legacy_sources: &'static [LegacyMetadataSource],
    rewrite: Rewrite,
}

struct DefaultSourcesMigration;

impl MetadataMigration for DefaultSourcesMigration {
    fn name(&self) -> &'static str {
        "default-sources"
    }

    fn record_sets(&self) -> &'static [MetadataRecordSet] {
        &[MetadataRecordSet::QuotaUsage]
    }

    fn rewrite(
        &self,
        _record_set: MetadataRecordSet,
        record: &MetadataRecord,
    ) -> Result<Option<MetadataRecord>, String> {
        Ok(Some(MetadataRecord {
            key: record.key.clone(),
            value: [record.value.as_slice(), b"!"].concat(),
        }))
    }
}

impl MetadataMigration for Migration {
    fn name(&self) -> &'static str {
        "neutral-test"
    }

    fn record_sets(&self) -> &'static [MetadataRecordSet] {
        self.record_sets
    }

    fn legacy_sources(&self) -> &'static [LegacyMetadataSource] {
        self.legacy_sources
    }

    fn rewrite(
        &self,
        _record_set: MetadataRecordSet,
        record: &MetadataRecord,
    ) -> Result<Option<MetadataRecord>, String> {
        if record.key == "fail" {
            return Err("owner rejected record".to_owned());
        }
        if record.key == "unchanged" {
            return Ok(None);
        }
        Ok(Some(match self.rewrite {
            Rewrite::Append => MetadataRecord {
                key: record.key.clone(),
                value: [record.value.as_slice(), b"!"].concat(),
            },
            Rewrite::Rename => MetadataRecord {
                key: "renamed".to_owned(),
                value: record.value.clone(),
            },
            Rewrite::Reject => MetadataRecord {
                key: record.key.clone(),
                value: vec![0xff],
            },
            Rewrite::Skip => return Ok(None),
        }))
    }
}

#[test]
fn test_metadata_migration_empty_sources_change_nothing() {
    let (directory, store) = store();
    let report = store
        .migrate_metadata(&Migration {
            record_sets: &[MetadataRecordSet::QuotaUsage],
            legacy_sources: &[LegacyMetadataSource {
                table: "absent_source",
                value_kind: MetadataValueKind::Bytes,
                target: MetadataRecordSet::QuotaResource,
            }],
            rewrite: Rewrite::Append,
        })
        .unwrap();
    drop(store);

    assert_eq!(report, MetadataMigrationReport::default());
    assert!(!table_names(&database_path(&directory)).contains(&"absent_source".to_owned()));
}

#[test]
fn test_metadata_migration_uses_the_default_empty_legacy_sources() {
    let (directory, store) = store();
    drop(store);
    write_bytes(
        &database_path(&directory),
        "quota_usage",
        &[("key".to_owned(), b"old".to_vec())],
    );
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();

    assert_eq!(
        store.migrate_metadata(&DefaultSourcesMigration).unwrap(),
        MetadataMigrationReport {
            scanned: 1,
            rewritten: 1,
        }
    );
    drop(store);
    assert_eq!(read_bytes(&database_path(&directory), "quota_usage")["key"], b"old!");
}

#[rstest]
#[case::quota_usage(MetadataRecordSet::QuotaUsage, "quota_usage")]
#[case::quota_resource(MetadataRecordSet::QuotaResource, "quota_resource")]
#[case::quota_group(MetadataRecordSet::QuotaGroup, "quota_group")]
#[case::quota_reservation(MetadataRecordSet::QuotaReservation, "quota_reservation")]
#[case::decision_history(MetadataRecordSet::PolicyDecisionHistory, "policy_decision")]
#[case::decision_current(MetadataRecordSet::PolicyDecisionCurrent, "policy_decision_current")]
#[case::decision_by_id(MetadataRecordSet::PolicyDecisionCurrentById, "policy_decision_current_id")]
#[case::analytics(MetadataRecordSet::Analytics, "analytics")]
fn test_metadata_migration_recognizes_current_tables(
    #[case] record_set: MetadataRecordSet,
    #[case] table: &'static str,
) {
    let (_directory, store) = store();

    assert_eq!(
        store
            .migrate_metadata(&Migration {
                record_sets: &[],
                legacy_sources: Box::leak(
                    vec![LegacyMetadataSource {
                        table,
                        value_kind: match record_set {
                            MetadataRecordSet::PolicyDecisionCurrent | MetadataRecordSet::PolicyDecisionCurrentById =>
                                MetadataValueKind::Text,
                            _ => MetadataValueKind::Bytes,
                        },
                        target: record_set,
                    }]
                    .into_boxed_slice(),
                ),
                rewrite: Rewrite::Skip,
            })
            .unwrap(),
        MetadataMigrationReport::default()
    );
}

#[test]
fn test_metadata_migration_moves_more_than_one_batch_and_deletes_source() {
    let (directory, store) = store();
    drop(store);
    write_bytes(
        &database_path(&directory),
        "retired_batch",
        &(0..257)
            .map(|index| (format!("key-{index:03}"), format!("value-{index}").into_bytes()))
            .collect::<Vec<_>>(),
    );
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();
    let report = store
        .migrate_metadata(&Migration {
            record_sets: &[],
            legacy_sources: &[LegacyMetadataSource {
                table: "retired_batch",
                value_kind: MetadataValueKind::Bytes,
                target: MetadataRecordSet::QuotaGroup,
            }],
            rewrite: Rewrite::Append,
        })
        .unwrap();
    drop(store);

    assert_eq!(
        report,
        MetadataMigrationReport {
            scanned: 257,
            rewritten: 257
        }
    );
    assert_eq!(read_bytes(&database_path(&directory), "quota_group").len(), 257);
    assert!(!table_names(&database_path(&directory)).contains(&"retired_batch".to_owned()));
}

#[test]
fn test_metadata_migration_deletes_an_exact_full_legacy_batch() {
    let (directory, store) = store();
    drop(store);
    write_bytes(
        &database_path(&directory),
        "retired_exact_batch",
        &(0..256)
            .map(|index| (format!("key-{index:03}"), format!("value-{index}").into_bytes()))
            .collect::<Vec<_>>(),
    );
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();
    let report = store
        .migrate_metadata(&Migration {
            record_sets: &[],
            legacy_sources: &[LegacyMetadataSource {
                table: "retired_exact_batch",
                value_kind: MetadataValueKind::Bytes,
                target: MetadataRecordSet::QuotaGroup,
            }],
            rewrite: Rewrite::Append,
        })
        .unwrap();
    drop(store);

    assert_eq!(
        (
            report,
            read_bytes(&database_path(&directory), "quota_group").len(),
            table_names(&database_path(&directory)).contains(&"retired_exact_batch".to_owned()),
        ),
        (
            MetadataMigrationReport {
                scanned: 256,
                rewritten: 256,
            },
            256,
            false,
        )
    );
}

#[test]
fn test_metadata_migration_preserves_an_exact_full_unchanged_legacy_batch() {
    let (directory, store) = store();
    drop(store);
    write_bytes(
        &database_path(&directory),
        "retired_unchanged_batch",
        &(0..256)
            .map(|index| (format!("key-{index:03}"), format!("value-{index}").into_bytes()))
            .collect::<Vec<_>>(),
    );
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();
    let report = store
        .migrate_metadata(&Migration {
            record_sets: &[],
            legacy_sources: &[LegacyMetadataSource {
                table: "retired_unchanged_batch",
                value_kind: MetadataValueKind::Bytes,
                target: MetadataRecordSet::QuotaGroup,
            }],
            rewrite: Rewrite::Skip,
        })
        .unwrap();
    drop(store);

    assert_eq!(
        (
            report,
            read_bytes(&database_path(&directory), "retired_unchanged_batch").len(),
            read_bytes(&database_path(&directory), "quota_group"),
        ),
        (
            MetadataMigrationReport {
                scanned: 256,
                rewritten: 0
            },
            256,
            HashMap::new()
        )
    );
}

#[test]
fn test_metadata_migration_does_not_rescan_created_current_keys() {
    let (directory, store) = store();
    drop(store);
    write_bytes(
        &database_path(&directory),
        "quota_resource",
        &(0..257)
            .map(|index| (format!("key-{index:03}"), format!("value-{index}").into_bytes()))
            .collect::<Vec<_>>(),
    );
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();
    let report = store
        .migrate_metadata(&Migration {
            record_sets: &[MetadataRecordSet::QuotaResource],
            legacy_sources: &[],
            rewrite: Rewrite::Rename,
        })
        .unwrap();
    drop(store);

    assert_eq!(
        (report, read_bytes(&database_path(&directory), "quota_resource"),),
        (
            MetadataMigrationReport {
                scanned: 257,
                rewritten: 257,
            },
            HashMap::from([("renamed".to_owned(), b"value-0".to_vec())]),
        )
    );
}

#[rstest]
#[case::quota_usage(MetadataRecordSet::QuotaUsage, "quota_usage", MetadataValueKind::Bytes)]
#[case::quota_resource(MetadataRecordSet::QuotaResource, "quota_resource", MetadataValueKind::Bytes)]
#[case::quota_group(MetadataRecordSet::QuotaGroup, "quota_group", MetadataValueKind::Bytes)]
#[case::quota_reservation(MetadataRecordSet::QuotaReservation, "quota_reservation", MetadataValueKind::Bytes)]
#[case::decision_history(
    MetadataRecordSet::PolicyDecisionHistory,
    "policy_decision",
    MetadataValueKind::Bytes
)]
#[case::decision_current(
    MetadataRecordSet::PolicyDecisionCurrent,
    "policy_decision_current",
    MetadataValueKind::Text
)]
#[case::decision_by_id(
    MetadataRecordSet::PolicyDecisionCurrentById,
    "policy_decision_current_id",
    MetadataValueKind::Text
)]
#[case::analytics(MetadataRecordSet::Analytics, "analytics", MetadataValueKind::Bytes)]
fn test_metadata_migration_rewrites_current_record_sets(
    #[case] record_set: MetadataRecordSet,
    #[case] table: &'static str,
    #[case] value_kind: MetadataValueKind,
) {
    let (directory, store) = store();
    drop(store);
    match value_kind {
        MetadataValueKind::Bytes => write_bytes(
            &database_path(&directory),
            table,
            &[("key".to_owned(), b"old".to_vec())],
        ),
        MetadataValueKind::Text => write_text(&database_path(&directory), table, &[("key", "old")]),
    }
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();

    assert_eq!(
        store
            .migrate_metadata(&Migration {
                record_sets: Box::leak(vec![record_set].into_boxed_slice()),
                legacy_sources: &[],
                rewrite: Rewrite::Append,
            })
            .unwrap(),
        MetadataMigrationReport {
            scanned: 1,
            rewritten: 1
        }
    );
    drop(store);
    match value_kind {
        MetadataValueKind::Bytes => assert_eq!(read_bytes(&database_path(&directory), table)["key"], b"old!"),
        MetadataValueKind::Text => assert_eq!(read_text(&database_path(&directory), table)["key"], "old!"),
    }
}

#[rstest]
#[case::without_collision(false, b"moved")]
#[case::with_collision(true, b"current")]
fn test_metadata_migration_renamed_key_preserves_target_collision(#[case] collision: bool, #[case] expected: &[u8]) {
    let (directory, store) = store();
    drop(store);
    let mut records = vec![("old".to_owned(), b"moved".to_vec())];
    if collision {
        records.push(("renamed".to_owned(), b"current".to_vec()));
    }
    write_bytes(&database_path(&directory), "quota_resource", &records);
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();
    store
        .migrate_metadata(&Migration {
            record_sets: &[MetadataRecordSet::QuotaResource],
            legacy_sources: &[],
            rewrite: Rewrite::Rename,
        })
        .unwrap();
    drop(store);

    assert_eq!(
        read_bytes(&database_path(&directory), "quota_resource"),
        HashMap::from([("renamed".to_owned(), expected.to_vec())])
    );
}

#[rstest]
#[case::bytes(
    LegacyMetadataSource {
        table: "retired_bytes",
        value_kind: MetadataValueKind::Bytes,
        target: MetadataRecordSet::QuotaResource,
    },
    "quota_resource"
)]
#[case::text(
    LegacyMetadataSource {
        table: "retired_text",
        value_kind: MetadataValueKind::Text,
        target: MetadataRecordSet::PolicyDecisionCurrent,
    },
    "policy_decision_current"
)]
#[case::quota_usage(
    LegacyMetadataSource {
        table: "retired_usage",
        value_kind: MetadataValueKind::Bytes,
        target: MetadataRecordSet::QuotaUsage,
    },
    "quota_usage"
)]
#[case::quota_group(
    LegacyMetadataSource {
        table: "retired_group",
        value_kind: MetadataValueKind::Bytes,
        target: MetadataRecordSet::QuotaGroup,
    },
    "quota_group"
)]
#[case::quota_reservation(
    LegacyMetadataSource {
        table: "retired_reservation",
        value_kind: MetadataValueKind::Bytes,
        target: MetadataRecordSet::QuotaReservation,
    },
    "quota_reservation"
)]
#[case::decision_history(
    LegacyMetadataSource {
        table: "retired_decision",
        value_kind: MetadataValueKind::Bytes,
        target: MetadataRecordSet::PolicyDecisionHistory,
    },
    "policy_decision"
)]
#[case::decision_by_id(
    LegacyMetadataSource {
        table: "retired_decision_id",
        value_kind: MetadataValueKind::Text,
        target: MetadataRecordSet::PolicyDecisionCurrentById,
    },
    "policy_decision_current_id"
)]
#[case::analytics(
    LegacyMetadataSource {
        table: "retired_analytics",
        value_kind: MetadataValueKind::Bytes,
        target: MetadataRecordSet::Analytics,
    },
    "analytics"
)]
fn test_metadata_migration_moves_bytes_and_text(#[case] source: LegacyMetadataSource, #[case] target: &'static str) {
    let (directory, store) = store();
    drop(store);
    match source.value_kind {
        MetadataValueKind::Bytes => write_bytes(
            &database_path(&directory),
            source.table,
            &[("key".to_owned(), b"old".to_vec())],
        ),
        MetadataValueKind::Text => write_text(&database_path(&directory), source.table, &[("key", "old")]),
    }
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();
    store
        .migrate_metadata(&Migration {
            record_sets: &[],
            legacy_sources: Box::leak(vec![source].into_boxed_slice()),
            rewrite: Rewrite::Append,
        })
        .unwrap();
    drop(store);

    match source.value_kind {
        MetadataValueKind::Bytes => assert_eq!(read_bytes(&database_path(&directory), target)["key"], b"old!"),
        MetadataValueKind::Text => assert_eq!(read_text(&database_path(&directory), target)["key"], "old!"),
    }
}

#[test]
fn test_metadata_migration_skips_current_records_left_unchanged() {
    let (directory, store) = store();
    drop(store);
    write_bytes(
        &database_path(&directory),
        "quota_resource",
        &[("key".to_owned(), b"old".to_vec())],
    );
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();

    assert_eq!(
        store
            .migrate_metadata(&Migration {
                record_sets: &[MetadataRecordSet::QuotaResource],
                legacy_sources: &[],
                rewrite: Rewrite::Skip,
            })
            .unwrap(),
        MetadataMigrationReport {
            scanned: 1,
            rewritten: 0,
        }
    );
}

#[test]
fn test_metadata_migration_does_not_rescan_renamed_text_keys() {
    let (directory, store) = store();
    drop(store);
    let records = (0..257)
        .map(|index| (format!("key-{index:03}"), format!("value-{index}")))
        .collect::<Vec<_>>();
    let records = records
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    write_text(&database_path(&directory), "policy_decision_current", &records);
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();

    assert_eq!(
        store
            .migrate_metadata(&Migration {
                record_sets: &[MetadataRecordSet::PolicyDecisionCurrent],
                legacy_sources: &[],
                rewrite: Rewrite::Rename,
            })
            .unwrap(),
        MetadataMigrationReport {
            scanned: 257,
            rewritten: 257,
        }
    );
    drop(store);
    assert_eq!(
        read_text(&database_path(&directory), "policy_decision_current"),
        HashMap::from([("renamed".to_owned(), "value-0".to_owned())])
    );
}

#[test]
fn test_metadata_migration_invalid_text_rolls_back_legacy_move() {
    let (directory, store) = store();
    drop(store);
    write_bytes(
        &database_path(&directory),
        "retired_invalid_text",
        &[("key".to_owned(), b"old".to_vec())],
    );
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();
    let error = store
        .migrate_metadata(&Migration {
            record_sets: &[],
            legacy_sources: &[LegacyMetadataSource {
                table: "retired_invalid_text",
                value_kind: MetadataValueKind::Bytes,
                target: MetadataRecordSet::PolicyDecisionCurrent,
            }],
            rewrite: Rewrite::Reject,
        })
        .unwrap_err();
    drop(store);

    assert!(matches!(error, MetadataMigrationError::InvalidText { key, .. } if key == "key"));
    assert_eq!(
        read_bytes(&database_path(&directory), "retired_invalid_text")["key"],
        b"old"
    );
    assert!(read_text(&database_path(&directory), "policy_decision_current").is_empty());
}

#[test]
fn test_metadata_migration_owner_failure_rolls_back_batch() {
    let (directory, store) = store();
    drop(store);
    write_bytes(
        &database_path(&directory),
        "retired_failure",
        &[
            ("first".to_owned(), b"one".to_vec()),
            ("fail".to_owned(), b"two".to_vec()),
        ],
    );
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();
    let error = store
        .migrate_metadata(&Migration {
            record_sets: &[],
            legacy_sources: &[LegacyMetadataSource {
                table: "retired_failure",
                value_kind: MetadataValueKind::Bytes,
                target: MetadataRecordSet::QuotaResource,
            }],
            rewrite: Rewrite::Append,
        })
        .unwrap_err();
    drop(store);

    assert!(
        matches!(error, MetadataMigrationError::Owner { key, message, .. } if key == "fail" && message == "owner rejected record")
    );
    assert_eq!(read_bytes(&database_path(&directory), "retired_failure").len(), 2);
    assert!(read_bytes(&database_path(&directory), "quota_resource").is_empty());
}

#[test]
fn test_metadata_migration_leaves_invalid_records_and_source_table() {
    let (directory, store) = store();
    drop(store);
    write_text(&database_path(&directory), "retired_unchanged", &[("unchanged", "old")]);
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();
    let report = store
        .migrate_metadata(&Migration {
            record_sets: &[],
            legacy_sources: &[LegacyMetadataSource {
                table: "retired_unchanged",
                value_kind: MetadataValueKind::Text,
                target: MetadataRecordSet::PolicyDecisionCurrentById,
            }],
            rewrite: Rewrite::Append,
        })
        .unwrap();
    drop(store);

    assert_eq!(
        report,
        MetadataMigrationReport {
            scanned: 1,
            rewritten: 0
        }
    );
    assert_eq!(
        read_text(&database_path(&directory), "retired_unchanged")["unchanged"],
        "old"
    );
    assert!(table_names(&database_path(&directory)).contains(&"retired_unchanged".to_owned()));
}

#[test]
fn test_metadata_migration_legacy_collision_preserves_current_value() {
    let (directory, store) = store();
    drop(store);
    write_bytes(
        &database_path(&directory),
        "retired_collision",
        &[("old".to_owned(), b"legacy".to_vec())],
    );
    write_bytes(
        &database_path(&directory),
        "quota_usage",
        &[("renamed".to_owned(), b"current".to_vec())],
    );
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();
    store
        .migrate_metadata(&Migration {
            record_sets: &[],
            legacy_sources: &[LegacyMetadataSource {
                table: "retired_collision",
                value_kind: MetadataValueKind::Bytes,
                target: MetadataRecordSet::QuotaUsage,
            }],
            rewrite: Rewrite::Rename,
        })
        .unwrap();
    drop(store);

    assert_eq!(
        read_bytes(&database_path(&directory), "quota_usage")["renamed"],
        b"current"
    );
    assert!(!table_names(&database_path(&directory)).contains(&"retired_collision".to_owned()));
}

#[test]
fn test_metadata_migration_legacy_text_collision_preserves_current_value() {
    let (directory, store) = store();
    drop(store);
    write_text(
        &database_path(&directory),
        "retired_text_collision",
        &[("old", "legacy")],
    );
    write_text(
        &database_path(&directory),
        "policy_decision_current",
        &[("renamed", "current")],
    );
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();
    store
        .migrate_metadata(&Migration {
            record_sets: &[],
            legacy_sources: &[LegacyMetadataSource {
                table: "retired_text_collision",
                value_kind: MetadataValueKind::Text,
                target: MetadataRecordSet::PolicyDecisionCurrent,
            }],
            rewrite: Rewrite::Rename,
        })
        .unwrap();
    drop(store);

    assert_eq!(
        read_text(&database_path(&directory), "policy_decision_current")["renamed"],
        "current"
    );
    assert!(!table_names(&database_path(&directory)).contains(&"retired_text_collision".to_owned()));
}

#[test]
fn test_metadata_migration_same_source_and_target_uses_in_place_path() {
    let (directory, store) = store();
    drop(store);
    write_bytes(
        &database_path(&directory),
        "quota_group",
        &[("key".to_owned(), b"old".to_vec())],
    );
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();

    assert_eq!(
        store
            .migrate_metadata(&Migration {
                record_sets: &[],
                legacy_sources: &[LegacyMetadataSource {
                    table: "quota_group",
                    value_kind: MetadataValueKind::Bytes,
                    target: MetadataRecordSet::QuotaGroup,
                }],
                rewrite: Rewrite::Append,
            })
            .unwrap(),
        MetadataMigrationReport {
            scanned: 1,
            rewritten: 1
        }
    );
}

#[test]
fn test_metadata_migration_reports_legacy_table_type_error() {
    let (directory, store) = store();
    drop(store);
    write_text(&database_path(&directory), "retired_wrong_type", &[("key", "old")]);
    let store = MetaStore::open_existing(database_path(&directory)).unwrap();

    assert!(matches!(
        store.migrate_metadata(&Migration {
            record_sets: &[],
            legacy_sources: &[LegacyMetadataSource {
                table: "retired_wrong_type",
                value_kind: MetadataValueKind::Bytes,
                target: MetadataRecordSet::QuotaUsage,
            }],
            rewrite: Rewrite::Append,
        }),
        Err(MetadataMigrationError::Store(MetaError::Table(
            redb::TableError::TableTypeMismatch { .. }
        )))
    ));
}

#[test]
fn test_metadata_migration_rejects_read_only_store() {
    let (directory, store) = store();
    drop(store);
    let store = MetaStore::open_existing_read_only(database_path(&directory)).unwrap();

    assert!(matches!(
        store.migrate_metadata(&Migration {
            record_sets: &[MetadataRecordSet::QuotaUsage],
            legacy_sources: &[],
            rewrite: Rewrite::Append,
        }),
        Err(MetadataMigrationError::Store(MetaError::Transaction(redb::TransactionError::Storage(
            redb::StorageError::Io(error)
        )))) if error.kind() == std::io::ErrorKind::PermissionDenied
    ));
}

#[test]
fn test_metadata_migration_reports_write_failure() {
    let (store, _backend, fault) = crate::meta::fault::initialized();
    crate::meta::fault::corrupt(&store, TableDefinition::new("quota_usage"), "key", b"old");
    fault.arm(0);

    assert!(matches!(
        store.migrate_metadata(&Migration {
            record_sets: &[MetadataRecordSet::QuotaUsage],
            legacy_sources: &[],
            rewrite: Rewrite::Append,
        }),
        Err(MetadataMigrationError::Store(_))
    ));
}

fn store() -> (TempDir, MetaStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = MetaStore::open(database_path(&directory)).unwrap();
    (directory, store)
}

fn database_path(directory: &TempDir) -> std::path::PathBuf {
    directory.path().join("metadata.redb")
}

fn write_bytes(path: &Path, table: &'static str, records: &[(String, Vec<u8>)]) {
    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_write().unwrap();
    {
        let mut table = txn.open_table(TableDefinition::<&str, &[u8]>::new(table)).unwrap();
        for (key, value) in records {
            table.insert(key.as_str(), value.as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();
}

fn write_text(path: &Path, table: &'static str, records: &[(&str, &str)]) {
    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_write().unwrap();
    {
        let mut table = txn.open_table(TableDefinition::<&str, &str>::new(table)).unwrap();
        for &(key, value) in records {
            table.insert(key, value).unwrap();
        }
    }
    txn.commit().unwrap();
}

fn read_bytes(path: &Path, table: &'static str) -> HashMap<String, Vec<u8>> {
    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_read().unwrap();
    txn.open_table(TableDefinition::<&str, &[u8]>::new(table))
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
    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_read().unwrap();
    txn.open_table(TableDefinition::<&str, &str>::new(table))
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
    redb::Database::open(path)
        .unwrap()
        .begin_read()
        .unwrap()
        .list_tables()
        .unwrap()
        .map(|table| table.name().to_owned())
        .collect()
}
