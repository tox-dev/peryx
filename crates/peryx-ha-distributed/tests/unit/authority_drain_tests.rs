use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use peryx_ha::{AuthorityDrainer, AvailabilityTaskReport, RetainedWriteFinalizer};
use peryx_storage::meta::{IntentPhase, IntentTransition, MetaStore};

use super::DistributedAuthorityDrainer;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn stage(store: &MetaStore, authority: &str, key: &str) {
    let limits = peryx_storage::meta::IntentLimits {
        max_records: 256,
        max_bytes: 1 << 20,
        backpressure_percent: 80,
    };
    let admission = peryx_storage::meta::IntentAdmission {
        authority,
        key,
        digest: "digest",
        size: 1,
        payload: b"payload",
    };
    store.stage_intent(admission, limits, 1).unwrap();
}

fn pending(store: &MetaStore) -> Vec<String> {
    store
        .list_pending_intents(256, u32::MAX)
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

fn phases(store: &MetaStore, keys: &[&str]) -> Vec<IntentPhase> {
    keys.iter()
        .map(|key| store.staged_intent(key).unwrap().unwrap().phase)
        .collect()
}

/// Stands in for the ecosystem finalizer at the authority's home: it publishes by settling the intent in
/// the store, exactly as the real finalize transaction does, and refuses the keys a home cannot yet
/// publish so their intents stay pending.
struct Home {
    meta: MetaStore,
    refused: BTreeSet<String>,
    offered: Mutex<Vec<(String, String)>>,
}

impl Home {
    fn new(meta: &MetaStore, refused: &[&str]) -> Self {
        Self {
            meta: meta.clone(),
            refused: refused.iter().map(|key| (*key).to_owned()).collect(),
            offered: Mutex::new(Vec::new()),
        }
    }

    fn offered(&self) -> Vec<(String, String)> {
        self.offered.lock().unwrap().clone()
    }
}

#[async_trait]
impl RetainedWriteFinalizer for Home {
    async fn finalize_retained(&self, authority: &str, intent_key: &str) -> bool {
        self.offered
            .lock()
            .unwrap()
            .push((authority.to_owned(), intent_key.to_owned()));
        !self.refused.contains(intent_key)
            && self.meta.advance_intent(intent_key, IntentPhase::Admitted, 9).unwrap() == IntentTransition::Advanced
    }
}

#[tokio::test]
async fn test_drain_publishes_every_retained_write_of_the_authority_across_batches() {
    let (_dir, store) = store();
    for serial in 0..130 {
        stage(&store, "auth", &format!("key-{serial}"));
    }
    let home = Home::new(&store, &[]);

    let report = DistributedAuthorityDrainer::new(store.clone())
        .drain("auth", &home, &|| false)
        .await
        .unwrap();

    assert_eq!(
        report,
        AvailabilityTaskReport {
            processed: 130,
            changed: 130
        }
    );
    assert_eq!(
        home.offered(),
        (0..130)
            .map(|serial| ("auth".to_owned(), format!("key-{serial}")))
            .collect::<Vec<_>>()
    );
    assert_eq!(pending(&store), Vec::<String>::new());
}

#[tokio::test]
async fn test_drain_leaves_the_writes_retained_for_another_authority_pending() {
    let (_dir, store) = store();
    stage(&store, "other", "other-first");
    stage(&store, "auth", "auth-only");
    stage(&store, "other", "other-last");
    let home = Home::new(&store, &[]);

    let report = DistributedAuthorityDrainer::new(store.clone())
        .drain("auth", &home, &|| false)
        .await
        .unwrap();

    assert_eq!(
        report,
        AvailabilityTaskReport {
            processed: 1,
            changed: 1
        }
    );
    assert_eq!(home.offered(), vec![("auth".to_owned(), "auth-only".to_owned())]);
    assert_eq!(pending(&store), vec!["other-first".to_owned(), "other-last".to_owned()]);
    assert_eq!(
        phases(&store, &["auth-only", "other-first", "other-last"]),
        vec![IntentPhase::Admitted, IntentPhase::Pending, IntentPhase::Pending]
    );
}

#[tokio::test]
async fn test_drain_leaves_a_write_its_home_refused_pending_and_still_finishes() {
    let (_dir, store) = store();
    for serial in 0..130 {
        stage(&store, "auth", &format!("key-{serial}"));
    }
    let refused = (0..130).map(|serial| format!("key-{serial}")).collect::<Vec<_>>();
    let home = Home::new(&store, &refused.iter().map(String::as_str).collect::<Vec<_>>());

    let report = DistributedAuthorityDrainer::new(store.clone())
        .drain("auth", &home, &|| false)
        .await
        .unwrap();

    assert_eq!(
        report,
        AvailabilityTaskReport {
            processed: 130,
            changed: 0
        }
    );
    assert_eq!(home.offered().len(), 130);
    assert_eq!(pending(&store), refused);
}

#[tokio::test]
async fn test_drain_settles_only_the_writes_their_home_published() {
    let (_dir, store) = store();
    for key in ["key-0", "key-1", "key-2"] {
        stage(&store, "auth", key);
    }
    let home = Home::new(&store, &["key-1"]);

    let report = DistributedAuthorityDrainer::new(store.clone())
        .drain("auth", &home, &|| false)
        .await
        .unwrap();

    assert_eq!(
        report,
        AvailabilityTaskReport {
            processed: 3,
            changed: 2
        }
    );
    assert_eq!(
        phases(&store, &["key-0", "key-1", "key-2"]),
        vec![IntentPhase::Admitted, IntentPhase::Pending, IntentPhase::Admitted]
    );
}

#[tokio::test]
async fn test_drain_resumes_past_settled_intents_and_is_idempotent() {
    let (_dir, store) = store();
    for serial in 0..5 {
        stage(&store, "auth", &format!("key-{serial}"));
    }
    store.advance_intent("key-1", IntentPhase::Admitted, 2).unwrap();
    store.advance_intent("key-3", IntentPhase::Admitted, 2).unwrap();
    let drainer = DistributedAuthorityDrainer::new(store.clone());
    let home = Home::new(&store, &[]);

    let report = drainer.drain("auth", &home, &|| false).await.unwrap();

    assert_eq!(
        report,
        AvailabilityTaskReport {
            processed: 3,
            changed: 3
        }
    );
    assert_eq!(
        drainer.drain("auth", &home, &|| false).await.unwrap(),
        AvailabilityTaskReport::default()
    );
    assert_eq!(
        home.offered(),
        ["key-0", "key-2", "key-4"]
            .map(|key| ("auth".to_owned(), key.to_owned()))
            .to_vec()
    );
}

#[tokio::test]
async fn test_drain_stops_between_batches_when_cancelled() {
    let (_dir, store) = store();
    for serial in 0..130 {
        stage(&store, "auth", &format!("key-{serial}"));
    }
    let home = Home::new(&store, &[]);
    let calls = AtomicUsize::new(0);

    let report = DistributedAuthorityDrainer::new(store.clone())
        .drain("auth", &home, &|| calls.fetch_add(1, Ordering::Relaxed) >= 1)
        .await
        .unwrap();

    assert_eq!(
        report,
        AvailabilityTaskReport {
            processed: 128,
            changed: 128
        }
    );
    assert_eq!(pending(&store).len(), 2);
}

#[tokio::test]
async fn test_distributed_drainer_maps_storage_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    stage(&meta, "auth", "key-1");
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
    let store = MetaStore::open_existing(&path).unwrap();
    let home = Home::new(&store, &[]);

    let error = DistributedAuthorityDrainer::new(store)
        .drain("auth", &home, &|| false)
        .await
        .unwrap_err();

    assert_eq!(error.code(), "storage");
    assert_eq!(home.offered(), Vec::<(String, String)>::new());
}
