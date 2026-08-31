use peryx_ha::{AuthorityDrainer, AvailabilityTaskReport};
use peryx_storage::meta::{IntentPhase, MetaStore};

use super::DistributedAuthorityDrainer;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn stage(store: &MetaStore, key: &str) {
    let limits = peryx_storage::meta::IntentLimits {
        max_records: 256,
        max_bytes: 1 << 20,
        backpressure_percent: 80,
    };
    let admission = peryx_storage::meta::IntentAdmission {
        authority: "auth",
        key,
        digest: "digest",
        size: 1,
        payload: b"payload",
    };
    store.stage_intent(admission, limits, 1).unwrap();
}

#[tokio::test]
async fn test_drain_finalizes_every_pending_intent_across_batches() {
    let (_dir, store) = store();
    for serial in 0..130 {
        stage(&store, &format!("key-{serial}"));
    }

    let report = DistributedAuthorityDrainer::new(store.clone())
        .drain(9, &|| false)
        .await
        .unwrap();

    assert_eq!(
        report,
        AvailabilityTaskReport {
            processed: 130,
            changed: 130
        }
    );
    assert!(store.list_pending_intents(256, u32::MAX).unwrap().is_empty());
    for serial in 0..130 {
        let record = store.staged_intent(&format!("key-{serial}")).unwrap().unwrap();
        assert_eq!(record.phase, IntentPhase::Admitted);
        assert_eq!(record.updated_at_unix, 9);
    }
}

#[tokio::test]
async fn test_drain_resumes_past_already_finalized_intents_and_is_idempotent() {
    let (_dir, store) = store();
    for serial in 0..5 {
        stage(&store, &format!("key-{serial}"));
    }
    store.advance_intent("key-1", IntentPhase::Admitted, 2).unwrap();
    store.advance_intent("key-3", IntentPhase::Admitted, 2).unwrap();

    let drainer = DistributedAuthorityDrainer::new(store.clone());
    let report = drainer.drain(9, &|| false).await.unwrap();

    assert_eq!(
        report,
        AvailabilityTaskReport {
            processed: 3,
            changed: 3
        }
    );
    assert_eq!(
        drainer.drain(9, &|| false).await.unwrap(),
        AvailabilityTaskReport::default()
    );
    for serial in 0..5 {
        assert_eq!(
            store.staged_intent(&format!("key-{serial}")).unwrap().unwrap().phase,
            IntentPhase::Admitted
        );
    }
}

#[tokio::test]
async fn test_drain_stops_between_batches_when_cancelled() {
    let (_dir, store) = store();
    for serial in 0..130 {
        stage(&store, &format!("key-{serial}"));
    }
    let calls = std::sync::atomic::AtomicUsize::new(0);

    let report = DistributedAuthorityDrainer::new(store.clone())
        .drain(9, &|| calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 1)
        .await
        .unwrap();

    assert_eq!(
        report,
        AvailabilityTaskReport {
            processed: 128,
            changed: 128
        }
    );
    assert_eq!(store.list_pending_intents(256, u32::MAX).unwrap().len(), 2);
}

#[tokio::test]
async fn test_distributed_drainer_maps_storage_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    stage(&meta, "key-1");
    drop(meta);
    let database = redb::Database::open(&path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut table = write
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("ingress_intent"))
            .unwrap();
        table.insert("key-1", b"not json".as_slice()).unwrap();
    }
    write.commit().unwrap();
    drop(database);
    let error = DistributedAuthorityDrainer::new(MetaStore::open_existing(&path).unwrap())
        .drain(1_000, &|| false)
        .await
        .unwrap_err();

    assert_eq!(error.code(), "storage");
}
