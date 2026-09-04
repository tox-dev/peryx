use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use rstest::rstest;

use super::store;
use crate::meta::{MetaStore, NewWebhookDelivery, WebhookDeliveryAttempt, WebhookDeliveryStatus, WebhookEventIntent};

thread_local! {
    /// Where this thread's events go while it holds a [`Captured`], and nowhere otherwise.
    static CAPTURE: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

/// A writer that hands each event to the capture belonging to the thread that raised it, so one
/// subscriber serves every test at once.
struct ThreadCapture;

impl std::io::Write for ThreadCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURE.with_borrow(|sink| {
            if let Some(sink) = sink {
                sink.lock().unwrap().extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for ThreadCapture {
    type Writer = Self;

    fn make_writer(&self) -> Self {
        Self
    }
}

/// The events this thread raises, until it drops.
///
/// The subscriber is installed once for the whole binary rather than per test, because a callsite
/// decides its interest the first time any thread executes it and every thread reads that decision
/// afterwards. A per-test subscriber leaves that decision to whichever thread arrives first: one
/// holding no subscriber at all resolves the callsite against nothing, caches `never`, and silences
/// the callsite for the tests that are asserting on it. A subscriber that outlives every test cannot
/// be the one missing when that question is asked.
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn install() -> Self {
        static SUBSCRIBER: OnceLock<()> = OnceLock::new();
        SUBSCRIBER.get_or_init(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(ThreadCapture)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("the test binary installs one global subscriber");
        });
        let sink = Arc::new(Mutex::new(Vec::new()));
        CAPTURE.with_borrow_mut(|slot| *slot = Some(Arc::clone(&sink)));
        Self(sink)
    }

    fn output(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).expect("the fmt subscriber writes utf-8")
    }
}

impl Drop for Captured {
    fn drop(&mut self) {
        CAPTURE.with_borrow_mut(|slot| *slot = None);
    }
}

fn none() -> HashSet<(String, String)> {
    HashSet::new()
}

fn enqueue_event(store: &MetaStore, event: WebhookEventIntent) -> Result<String, crate::meta::MetaError> {
    store.commit_driver_txn(|txn| {
        txn.enqueue_webhook_event(event);
        Ok::<_, crate::meta::MetaError>(((), Vec::new()))
    })?;
    Ok(store.next_webhook_event_id()?.unwrap())
}

#[test]
fn test_webhook_delivery_queue_orders_due_records() {
    let (_dir, store) = store();
    let later = store
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index: "hosted",
            target: "ci",
            event: "upload",
            payload: r#"{"event":"upload"}"#,
            created_at_unix: 20,
        })
        .unwrap();
    let earlier = store
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index: "hosted",
            target: "ci",
            event: "delete",
            payload: r#"{"event":"delete"}"#,
            created_at_unix: 10,
        })
        .unwrap();

    assert_eq!(store.next_webhook_delivery_at().unwrap(), Some(10));
    assert_eq!(store.list_due_webhook_deliveries(9, 10, &none()).unwrap(), Vec::new());
    let due = store.list_due_webhook_deliveries(20, 1, &none()).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, earlier);
    assert_eq!(store.get_webhook_delivery(&later).unwrap().unwrap().event, "upload");
    assert_eq!(
        store
            .list_webhook_deliveries()
            .unwrap()
            .into_iter()
            .map(|delivery| delivery.id)
            .collect::<HashSet<_>>(),
        HashSet::from([earlier, later])
    );
}

#[test]
fn test_webhook_event_fan_out_uses_stable_delivery_identities() {
    let (_dir, store) = store();
    let targets = vec!["audit".to_owned(), "deploy".to_owned()];
    let event_id = enqueue_event(
        &store,
        WebhookEventIntent {
            index: "hosted".to_owned(),
            targets,
            event: "upload".to_owned(),
            payload: r#"{"event":"upload"}"#.to_owned(),
            created_at_unix: 10,
        },
    )
    .unwrap();

    assert_eq!(
        store.next_webhook_event_id().unwrap().as_deref(),
        Some(event_id.as_str())
    );
    assert!(store.fan_out_webhook_event(&event_id).unwrap());
    assert!(!store.fan_out_webhook_event(&event_id).unwrap());
    assert_eq!(store.next_webhook_event_id().unwrap(), None);
    let deliveries = store.list_webhook_deliveries().unwrap();
    assert_eq!(deliveries.len(), 2);
    assert_ne!(deliveries[0].id, deliveries[1].id);
    assert_eq!(
        deliveries
            .iter()
            .map(|delivery| delivery.target.as_str())
            .collect::<HashSet<_>>(),
        HashSet::from(["audit", "deploy"])
    );
}

#[test]
fn test_webhook_event_rejects_an_empty_target_snapshot() {
    let (_dir, store) = store();

    let error = enqueue_event(
        &store,
        WebhookEventIntent {
            index: "hosted".to_owned(),
            targets: Vec::new(),
            event: "upload".to_owned(),
            payload: "{}".to_owned(),
            created_at_unix: 10,
        },
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "driver precondition failed: webhook event requires at least one target"
    );
}

#[test]
fn test_webhook_delivery_update_reschedules_and_finishes() {
    let (_dir, store) = store();
    let id = store
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index: "hosted",
            target: "ci",
            event: "upload",
            payload: r#"{"event":"upload"}"#,
            created_at_unix: 10,
        })
        .unwrap();

    let pending = store
        .update_webhook_delivery(
            &id,
            WebhookDeliveryAttempt {
                status: WebhookDeliveryStatus::Pending,
                updated_at_unix: 11,
                next_attempt_at_unix: Some(16),
                response_status: Some(500),
                last_error: Some("http status 500"),
            },
        )
        .unwrap()
        .unwrap();

    assert_eq!(pending.attempts, 1);
    assert_eq!(pending.next_attempt_at_unix, Some(16));
    assert_eq!(store.next_webhook_delivery_at().unwrap(), Some(16));
    assert!(store.list_due_webhook_deliveries(15, 10, &none()).unwrap().is_empty());
    assert_eq!(store.list_due_webhook_deliveries(16, 10, &none()).unwrap()[0].id, id);

    let delivered = store
        .update_webhook_delivery(
            &id,
            WebhookDeliveryAttempt {
                status: WebhookDeliveryStatus::Delivered,
                updated_at_unix: 16,
                next_attempt_at_unix: None,
                response_status: Some(204),
                last_error: None,
            },
        )
        .unwrap()
        .unwrap();

    assert_eq!(delivered.attempts, 2);
    assert_eq!(delivered.status, WebhookDeliveryStatus::Delivered);
    assert_eq!(store.get_webhook_delivery(&id).unwrap(), None);
    assert_eq!(store.next_webhook_delivery_at().unwrap(), None);
    assert!(store.list_due_webhook_deliveries(100, 10, &none()).unwrap().is_empty());
}

#[test]
fn test_webhook_delivery_update_handles_record_without_due_key() {
    let (_dir, store) = store();
    let id = enqueue(&store, "hosted", "ci", 10);
    store
        .update_webhook_delivery(
            &id,
            WebhookDeliveryAttempt {
                status: WebhookDeliveryStatus::Pending,
                updated_at_unix: 11,
                next_attempt_at_unix: None,
                response_status: Some(500),
                last_error: Some("http status 500"),
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(store.next_webhook_delivery_at().unwrap(), None);

    let failed = store
        .update_webhook_delivery(
            &id,
            WebhookDeliveryAttempt {
                status: WebhookDeliveryStatus::Failed,
                updated_at_unix: 12,
                next_attempt_at_unix: None,
                response_status: None,
                last_error: Some("manual terminal update"),
            },
        )
        .unwrap()
        .unwrap();

    assert_eq!(failed.attempts, 2);
    assert_eq!(failed.status, WebhookDeliveryStatus::Failed);
    assert_eq!(failed.next_attempt_at_unix, None);
    assert_eq!(failed.last_error.as_deref(), Some("manual terminal update"));
    assert_eq!(store.get_webhook_delivery(&id).unwrap(), None);
}

#[test]
fn test_completed_deliveries_never_return_to_the_fan_out() {
    let (_dir, store) = store();
    let event_id = enqueue_event(
        &store,
        WebhookEventIntent {
            index: "hosted".to_owned(),
            targets: vec!["audit".to_owned(), "deploy".to_owned()],
            event: "upload".to_owned(),
            payload: r#"{"event":"upload"}"#.to_owned(),
            created_at_unix: 10,
        },
    )
    .unwrap();
    assert!(store.fan_out_webhook_event(&event_id).unwrap());
    for delivery in store.list_webhook_deliveries().unwrap() {
        finish(&store, &delivery.id);
    }

    assert!(!store.fan_out_webhook_event(&event_id).unwrap());

    assert!(store.list_webhook_deliveries().unwrap().is_empty());
    assert_eq!(store.next_webhook_delivery_at().unwrap(), None);
}

#[test]
fn test_completed_deliveries_leave_only_outstanding_work_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    let outstanding = enqueue(&store, "hosted", "slow", 10);
    for round in 0..64 {
        finish(&store, &enqueue(&store, "hosted", "fast", 11 + round));
    }
    drop(store);

    let store = MetaStore::open_existing(&path).unwrap();

    assert_eq!(
        store
            .list_webhook_deliveries()
            .unwrap()
            .into_iter()
            .map(|delivery| delivery.id)
            .collect::<Vec<_>>(),
        vec![outstanding.clone()]
    );
    assert_eq!(store.next_webhook_delivery_at().unwrap(), Some(10));
    assert_eq!(
        store.list_due_webhook_deliveries(100, 10, &none()).unwrap()[0].id,
        outstanding
    );
}

fn finish(store: &MetaStore, id: &str) {
    store
        .update_webhook_delivery(
            id,
            WebhookDeliveryAttempt {
                status: WebhookDeliveryStatus::Delivered,
                updated_at_unix: 20,
                next_attempt_at_unix: None,
                response_status: Some(204),
                last_error: None,
            },
        )
        .unwrap()
        .unwrap();
}

#[test]
fn test_webhook_delivery_ignores_empty_limit_and_missing_updates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    assert!(store.list_due_webhook_deliveries(10, 0, &none()).unwrap().is_empty());
    assert!(
        store
            .update_webhook_delivery(
                "missing",
                WebhookDeliveryAttempt {
                    status: WebhookDeliveryStatus::Delivered,
                    updated_at_unix: 10,
                    next_attempt_at_unix: None,
                    response_status: Some(204),
                    last_error: None,
                },
            )
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_webhook_queue_ignores_a_stale_schedule_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    let id = enqueue(&store, "hosted", "ci", 20);
    drop(store);
    let database = redb::Database::open(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, &str>::new("webhook_due"))
        .unwrap()
        .insert("09223372036854775818/stale", id.as_str())
        .unwrap();
    txn.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();

    assert_eq!(store.next_webhook_delivery_at().unwrap(), Some(20));
    assert!(store.list_due_webhook_deliveries(10, 10, &none()).unwrap().is_empty());
    assert_eq!(store.next_webhook_delivery_at().unwrap(), Some(20));
}

#[derive(Clone, Copy)]
enum QueueDamage {
    MalformedTimestamp,
    MissingRecord,
    InvalidJson,
}

#[rstest]
#[case::malformed_timestamp(QueueDamage::MalformedTimestamp, "malformed_due_keys")]
#[case::missing_record(QueueDamage::MissingRecord, "dangling_due_rows")]
#[case::invalid_json(QueueDamage::InvalidJson, "malformed_delivery_records")]
fn test_webhook_queue_scan_skips_and_cleans_damaged_rows(#[case] damage: QueueDamage, #[case] count: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    let damaged = matches!(damage, QueueDamage::InvalidJson).then(|| enqueue(&store, "hosted", "broken", 10));
    let healthy = enqueue(&store, "hosted", "healthy", 20);
    drop(store);
    damage_queue(&path, damage, damaged.as_deref());

    let store = MetaStore::open_existing(&path).unwrap();
    assert_eq!(store.next_webhook_delivery_at().unwrap(), Some(20));
    let captured = Captured::install();
    for _ in 0..2 {
        assert_eq!(
            store
                .list_due_webhook_deliveries(100, 10, &none())
                .unwrap()
                .into_iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            std::slice::from_ref(&healthy)
        );
    }
    if matches!(damage, QueueDamage::InvalidJson) {
        assert_eq!(store.get_webhook_delivery(damaged.as_deref().unwrap()).unwrap(), None);
    }
    let output = captured.output();
    assert_eq!(output.matches("discarding damaged webhook queue rows").count(), 1);
    assert!(output.contains(&format!("{count}=1")), "{output}");
    assert!(!output.contains("unbounded-corrupt-identifier"));
}

fn damage_queue(path: &std::path::Path, damage: QueueDamage, damaged: Option<&str>) {
    let db = redb::Database::open(path).unwrap();
    let txn = db.begin_write().unwrap();
    match damage {
        QueueDamage::MalformedTimestamp => {
            txn.open_table(redb::TableDefinition::<&str, &str>::new("webhook_due"))
                .unwrap()
                .insert("!unparseable", "unbounded-corrupt-identifier")
                .unwrap();
        }
        QueueDamage::MissingRecord => {
            txn.open_table(redb::TableDefinition::<&str, &str>::new("webhook_due"))
                .unwrap()
                .insert(
                    "09223372036854775818/unbounded-corrupt-identifier",
                    "unbounded-corrupt-identifier",
                )
                .unwrap();
        }
        QueueDamage::InvalidJson => {
            txn.open_table(redb::TableDefinition::<&str, &[u8]>::new("webhook_delivery"))
                .unwrap()
                .insert(damaged.unwrap(), b"{".as_slice())
                .unwrap();
        }
    }
    txn.commit().unwrap();
}

fn enqueue(store: &MetaStore, index: &str, target: &str, created_at_unix: i64) -> String {
    store
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index,
            target,
            event: "upload",
            payload: r#"{"event":"upload"}"#,
            created_at_unix,
        })
        .unwrap()
}

#[test]
fn test_list_due_returns_one_record_per_target_in_due_order() {
    let (_dir, store) = store();
    let slow_first = enqueue(&store, "hosted", "slow", 10);
    enqueue(&store, "hosted", "slow", 11);
    let healthy = enqueue(&store, "hosted", "healthy", 12);

    let due = store.list_due_webhook_deliveries(100, 10, &none()).unwrap();

    let ids: Vec<&str> = due.iter().map(|record| record.id.as_str()).collect();
    assert_eq!(ids, [slow_first.as_str(), healthy.as_str()]);
}

#[test]
fn test_list_due_skips_excluded_targets_to_reach_a_later_one() {
    let (_dir, store) = store();
    for created_at in 10..14 {
        enqueue(&store, "hosted", "slow", created_at);
    }
    let healthy = enqueue(&store, "hosted", "healthy", 20);
    let excluded = HashSet::from([("hosted".to_owned(), "slow".to_owned())]);

    let due = store.list_due_webhook_deliveries(100, 10, &excluded).unwrap();

    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, healthy);
    assert_eq!(due[0].target, "healthy");
}

#[test]
fn test_list_due_separates_same_target_name_across_indexes() {
    let (_dir, store) = store();
    let first = enqueue(&store, "one", "ci", 10);
    let second = enqueue(&store, "two", "ci", 11);

    let due = store.list_due_webhook_deliveries(100, 10, &none()).unwrap();

    let ids: Vec<&str> = due.iter().map(|record| record.id.as_str()).collect();
    assert_eq!(ids, [first.as_str(), second.as_str()]);
}

/// The interleaving that silenced this file's captures: a thread holding no subscriber is the first to
/// execute a callsite, which is when its interest is decided for every thread that follows. A capture
/// that only exists for the length of one test is absent at that moment; the binary's subscriber is
/// not, so the event still reaches the thread asserting on it.
#[test]
fn test_a_capture_survives_a_neighbour_reaching_the_callsite_first() {
    let captured = Captured::install();
    std::thread::spawn(unsubscribed_first_event).join().unwrap();
    unsubscribed_first_event();

    assert!(
        captured.output().contains("callsite registered by a bare thread"),
        "a thread with no subscriber decided this callsite for everyone: {:?}",
        captured.output()
    );
}

/// A callsite of its own, so the test asserts on a first execution rather than on one some earlier test
/// already resolved.
fn unsubscribed_first_event() {
    tracing::warn!(target: "peryx::webhook", "callsite registered by a bare thread");
}
