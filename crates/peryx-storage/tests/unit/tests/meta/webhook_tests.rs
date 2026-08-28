use std::collections::HashSet;
use std::io::{Read as _, Seek as _};
use std::sync::Mutex;

use rstest::rstest;

use super::store;
use crate::meta::{MetaStore, NewWebhookDelivery, WebhookDeliveryAttempt, WebhookDeliveryStatus};

fn none() -> HashSet<(String, String)> {
    HashSet::new()
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
    assert_eq!(store.next_webhook_delivery_at().unwrap(), None);
    assert!(store.list_due_webhook_deliveries(100, 10, &none()).unwrap().is_empty());
}

#[test]
fn test_webhook_delivery_update_handles_record_without_due_key() {
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

    store
        .update_webhook_delivery(
            &id,
            WebhookDeliveryAttempt {
                status: WebhookDeliveryStatus::Delivered,
                updated_at_unix: 11,
                next_attempt_at_unix: None,
                response_status: Some(204),
                last_error: None,
            },
        )
        .unwrap();
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
    assert_eq!(store.next_webhook_delivery_at().unwrap(), None);
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
    FinishedRecord,
}

#[rstest]
#[case::malformed_timestamp(QueueDamage::MalformedTimestamp, "malformed_due_keys")]
#[case::missing_record(QueueDamage::MissingRecord, "dangling_due_rows")]
#[case::invalid_json(QueueDamage::InvalidJson, "malformed_delivery_records")]
#[case::finished_record(QueueDamage::FinishedRecord, "dangling_due_rows")]
fn test_webhook_queue_scan_skips_and_cleans_damaged_rows(#[case] damage: QueueDamage, #[case] count: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    let damaged = matches!(damage, QueueDamage::InvalidJson | QueueDamage::FinishedRecord)
        .then(|| enqueue(&store, "hosted", "broken", 10));
    if matches!(damage, QueueDamage::FinishedRecord) {
        store
            .update_webhook_delivery(
                damaged.as_deref().unwrap(),
                WebhookDeliveryAttempt {
                    status: WebhookDeliveryStatus::Delivered,
                    updated_at_unix: 10,
                    next_attempt_at_unix: None,
                    response_status: Some(200),
                    last_error: None,
                },
            )
            .unwrap();
    }
    let healthy = enqueue(&store, "hosted", "healthy", 20);
    drop(store);
    damage_queue(&path, damage, damaged.as_deref());

    let store = MetaStore::open_existing(&path).unwrap();
    assert_eq!(store.next_webhook_delivery_at().unwrap(), Some(20));
    let mut capture = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(Mutex::new(capture.try_clone().unwrap()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
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
    });
    if matches!(damage, QueueDamage::InvalidJson) {
        assert_eq!(store.get_webhook_delivery(damaged.as_deref().unwrap()).unwrap(), None);
    }
    capture.rewind().unwrap();
    let mut output = String::new();
    capture.read_to_string(&mut output).unwrap();
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
        QueueDamage::FinishedRecord => {
            txn.open_table(redb::TableDefinition::<&str, &str>::new("webhook_due"))
                .unwrap()
                .insert("09223372036854775818/stale", damaged.unwrap())
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
