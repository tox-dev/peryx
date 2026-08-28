use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use peryx_storage::meta::{
    AccountingClass, MetadataMigration, MetadataRecord, MetadataRecordSet, NewQuotaReservation, QuotaLimits, QuotaUsage,
};

use super::{open, open_existing, open_existing_copy, open_existing_read_only};

#[test]
fn writable_open_applies_metadata_migrations() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let plugins = crate::tests::support::plugins_with_metadata_migration(migration(MigrationResult::Move));
    let store = open(&path, &plugins).unwrap();
    seed(&store);
    drop(store);

    let migrated = open_existing(&path, &plugins).unwrap();
    assert_eq!(migrated.quota_usage("source").unwrap(), QuotaUsage::default());
    assert_eq!(migrated.quota_usage("target").unwrap().accounted_bytes.reserved, 1);
}

#[test]
fn immutable_open_accepts_current_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let store = peryx_storage::meta::MetaStore::open(&path).unwrap();
    seed(&store);
    drop(store);
    let plugins = crate::tests::support::plugins_with_metadata_migration(migration(MigrationResult::Keep));

    assert_eq!(
        open_existing_read_only(&path, &plugins)
            .unwrap()
            .quota_usage("source")
            .unwrap()
            .accounted_bytes
            .reserved,
        1
    );
}

#[test]
fn immutable_open_does_not_copy_current_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let store = peryx_storage::meta::MetaStore::open(&path).unwrap();
    seed(&store);
    drop(store);
    let (entered_sender, entered_receiver) = std::sync::mpsc::sync_channel(0);
    let (resume_sender, resume_receiver) = std::sync::mpsc::sync_channel(0);
    let plugins = crate::tests::support::plugins_with_metadata_migration(Arc::new(TestMigration {
        result: MigrationResult::Keep,
        gate: Some(MigrationGate {
            entered: entered_sender,
            resume: Mutex::new(resume_receiver),
        }),
    }));
    let before = directory_entries(directory.path());

    std::thread::scope(|scope| {
        let opener = scope.spawn(|| open_existing_read_only(&path, &plugins));
        entered_receiver.recv().unwrap();
        let during = directory_entries(directory.path());
        resume_sender.send(()).unwrap();
        drop(opener.join().unwrap().unwrap());

        assert_eq!(during, before);
    });
}

#[test]
fn immutable_open_rejects_a_required_upgrade_without_mutating_source() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let store = peryx_storage::meta::MetaStore::open(&path).unwrap();
    seed(&store);
    drop(store);
    let plugins = crate::tests::support::plugins_with_metadata_migration(migration(MigrationResult::Move));
    let before = std::fs::read(&path).unwrap();

    assert_eq!(
        open_existing_read_only(&path, &plugins).unwrap_err().to_string(),
        format!(
            "metadata store {} requires a schema upgrade; open it with a writable peryx command before retrying",
            path.display()
        )
    );
    let source = peryx_storage::meta::MetaStore::open_existing_read_only(&path).unwrap();
    assert_eq!(source.quota_usage("source").unwrap().accounted_bytes.reserved, 1);
    assert_eq!(source.quota_usage("target").unwrap(), QuotaUsage::default());
    drop(source);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn copied_open_migrates_the_copy_without_mutating_source() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let store = peryx_storage::meta::MetaStore::open(&path).unwrap();
    seed(&store);
    drop(store);
    let plugins = crate::tests::support::plugins_with_metadata_migration(migration(MigrationResult::Move));

    let copy = open_existing_copy(&path, &plugins).unwrap();
    assert_eq!(copy.quota_usage("source").unwrap(), QuotaUsage::default());
    assert_eq!(copy.quota_usage("target").unwrap().accounted_bytes.reserved, 1);
    drop(copy);
    let source = peryx_storage::meta::MetaStore::open_existing_read_only(&path).unwrap();
    assert_eq!(source.quota_usage("source").unwrap().accounted_bytes.reserved, 1);
}

#[test]
fn copied_open_keeps_the_probe_beside_the_source_until_drop() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    drop(peryx_storage::meta::MetaStore::open(&path).unwrap());
    let before = directory_entries(directory.path());

    let copy = open_existing_copy(
        &path,
        &crate::tests::support::plugins_with_metadata_migration(migration(MigrationResult::Keep)),
    )
    .unwrap();
    assert_eq!(directory_entries(directory.path()).len(), before.len() + 1);
    drop(copy);
    assert_eq!(directory_entries(directory.path()), before);
}

#[test]
fn metadata_open_without_migration_capability_reads_the_source() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    drop(peryx_storage::meta::MetaStore::open(&path).unwrap());

    assert_eq!(
        open_existing_read_only(&path, &crate::tests::support::plugins())
            .unwrap()
            .quota_usage("source")
            .unwrap(),
        QuotaUsage::default()
    );
    assert_eq!(
        open_existing_copy(&path, &crate::tests::support::plugins())
            .unwrap()
            .quota_usage("source")
            .unwrap(),
        QuotaUsage::default()
    );
}

#[derive(Clone, Copy)]
enum MigrationResult {
    Keep,
    Move,
}

struct TestMigration {
    result: MigrationResult,
    gate: Option<MigrationGate>,
}

struct MigrationGate {
    entered: SyncSender<()>,
    resume: Mutex<Receiver<()>>,
}

impl MetadataMigration for TestMigration {
    fn name(&self) -> &'static str {
        "test"
    }

    fn record_sets(&self) -> &[MetadataRecordSet] {
        &[MetadataRecordSet::QuotaUsage]
    }

    fn legacy_sources(&self) -> &[peryx_storage::meta::LegacyMetadataSource] {
        &[]
    }

    fn rewrite(&self, _: MetadataRecordSet, record: &MetadataRecord) -> Result<Option<MetadataRecord>, String> {
        if let Some(gate) = &self.gate {
            gate.entered.send(()).unwrap();
            gate.resume.lock().unwrap().recv().unwrap();
        }
        Ok(match self.result {
            MigrationResult::Keep => None,
            MigrationResult::Move => Some(MetadataRecord {
                key: "target".to_owned(),
                value: record.value.clone(),
            }),
        })
    }
}

fn migration(result: MigrationResult) -> Arc<dyn MetadataMigration> {
    Arc::new(TestMigration { result, gate: None })
}

fn seed(store: &peryx_storage::meta::MetaStore) {
    store
        .reserve_quota(
            NewQuotaReservation {
                repository: "source",
                resource: Some("resource"),
                group: Some("group"),
                digest: "digest",
                bytes: 1,
                class: AccountingClass::Hosted,
                created_at_unix: 1,
            },
            QuotaLimits::default(),
        )
        .unwrap();
}

fn directory_entries(path: &Path) -> Vec<PathBuf> {
    let mut entries = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}
