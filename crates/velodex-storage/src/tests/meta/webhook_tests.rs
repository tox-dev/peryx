use super::store;
use crate::meta::{MetaStore, NewWebhookDelivery, WebhookDeliveryAttempt, WebhookDeliveryStatus};

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
    assert_eq!(store.list_due_webhook_deliveries(9, 10).unwrap(), Vec::new());
    let due = store.list_due_webhook_deliveries(20, 1).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, earlier);
    assert_eq!(store.get_webhook_delivery(&later).unwrap().unwrap().event, "upload");
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
    assert!(store.list_due_webhook_deliveries(15, 10).unwrap().is_empty());
    assert_eq!(store.list_due_webhook_deliveries(16, 10).unwrap()[0].id, id);

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
    assert!(store.list_due_webhook_deliveries(100, 10).unwrap().is_empty());
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
fn test_webhook_delivery_ignores_empty_limit_and_stale_due_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("velodex.redb");
    let store = MetaStore::open(&path).unwrap();
    assert!(store.list_due_webhook_deliveries(10, 0).unwrap().is_empty());
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
    drop(store);

    let db = redb::Database::create(&path).unwrap();
    let table: redb::TableDefinition<&str, &str> = redb::TableDefinition::new("webhook_due");
    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(table).unwrap();
        table.insert("not-a-due-key", "missing").unwrap();
        table.insert("09223372036854775808/missing", "missing").unwrap();
    }
    txn.commit().unwrap();
    drop(db);

    let store = MetaStore::open(&path).unwrap();
    assert!(store.list_due_webhook_deliveries(0, 10).unwrap().is_empty());
}
