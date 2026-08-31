use std::collections::HashSet;

use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};

use super::error::MetaError;
use super::{MetaStore, SERIAL, WEBHOOK_DELIVERY, WEBHOOK_DUE, WEBHOOK_EVENT, WEBHOOK_SERIAL_KEY};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookDeliveryStatus {
    Pending,
    Delivered,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookDeliveryRecord {
    pub id: String,
    pub index: String,
    pub target: String,
    pub event: String,
    pub payload: String,
    pub status: WebhookDeliveryStatus,
    pub attempts: u16,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
    pub next_attempt_at_unix: Option<i64>,
    pub response_status: Option<u16>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct NewWebhookDelivery<'a> {
    pub index: &'a str,
    pub target: &'a str,
    pub event: &'a str,
    pub payload: &'a str,
    pub created_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEventIntent {
    pub index: String,
    pub targets: Vec<String>,
    pub event: String,
    pub payload: String,
    pub created_at_unix: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct WebhookDeliveryAttempt<'a> {
    pub status: WebhookDeliveryStatus,
    pub updated_at_unix: i64,
    pub next_attempt_at_unix: Option<i64>,
    pub response_status: Option<u16>,
    pub last_error: Option<&'a str>,
}

impl MetaStore {
    /// # Errors
    /// Returns a store error if the write fails or the payload cannot be encoded.
    pub fn enqueue_webhook_delivery(&self, delivery: NewWebhookDelivery<'_>) -> Result<String, MetaError> {
        let txn = self.db.begin_write()?;
        let id = {
            let mut serials = txn.open_table(SERIAL)?;
            let next = serials.get(WEBHOOK_SERIAL_KEY)?.map_or(0, |value| value.value()) + 1;
            serials.insert(WEBHOOK_SERIAL_KEY, next)?;
            format!("wd_{next:016x}")
        };
        let record = WebhookDeliveryRecord {
            id: id.clone(),
            index: delivery.index.to_owned(),
            target: delivery.target.to_owned(),
            event: delivery.event.to_owned(),
            payload: delivery.payload.to_owned(),
            status: WebhookDeliveryStatus::Pending,
            attempts: 0,
            created_at_unix: delivery.created_at_unix,
            updated_at_unix: delivery.created_at_unix,
            next_attempt_at_unix: Some(delivery.created_at_unix),
            response_status: None,
            last_error: None,
        };
        {
            let bytes = serde_json::to_vec(&record)?;
            txn.open_table(WEBHOOK_DELIVERY)?
                .insert(id.as_str(), bytes.as_slice())?;
            txn.open_table(WEBHOOK_DUE)?
                .insert(due_key(delivery.created_at_unix, &id).as_str(), id.as_str())?;
        }
        txn.commit()?;
        Ok(id)
    }

    pub(super) fn enqueue_webhook_events(
        txn: &redb::WriteTransaction,
        events: &[WebhookEventIntent],
    ) -> Result<(), MetaError> {
        for event in events {
            insert_webhook_event(txn, event)?;
        }
        Ok(())
    }

    /// Materializes an event's target deliveries without changing identities already committed by an
    /// earlier attempt, and reports whether the event existed.
    ///
    /// # Errors
    /// Returns a store error if a queue write fails or a stored event cannot be decoded.
    pub fn fan_out_webhook_event(&self, id: &str) -> Result<bool, MetaError> {
        let mut materialized = false;
        while self.enqueue_next_webhook_event_delivery(id)? {
            materialized = true;
        }
        Ok(materialized)
    }

    /// Returns the oldest event whose target fan-out has not completed.
    ///
    /// # Errors
    /// Returns a store error if the event table cannot be read.
    pub fn next_webhook_event_id(&self) -> Result<Option<String>, MetaError> {
        let txn = self.db.begin_read()?;
        let events = txn.open_table(WEBHOOK_EVENT)?;
        Ok(events.first()?.map(|(id, _)| id.value().to_owned()))
    }

    /// Moves one target out of the outbox record and into the queue, so a fan-out replayed after a
    /// crash sees only the targets that never committed and terminal cleanup cannot resurrect one.
    fn enqueue_next_webhook_event_delivery(&self, event_id: &str) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        let delivery = {
            let mut events = txn.open_table(WEBHOOK_EVENT)?;
            let stored = events
                .get(event_id)?
                .map(|value| serde_json::from_slice::<WebhookEventRecord>(value.value()))
                .transpose()?;
            let Some(mut event) = stored else {
                return Ok(false);
            };
            let delivery = event.deliveries.remove(0);
            if event.deliveries.is_empty() {
                events.remove(event_id)?;
            } else {
                let bytes = serde_json::to_vec(&event)?;
                events.insert(event_id, bytes.as_slice())?;
            }
            delivery
        };
        {
            let bytes = serde_json::to_vec(&delivery)?;
            txn.open_table(WEBHOOK_DELIVERY)?
                .insert(delivery.id.as_str(), bytes.as_slice())?;
            txn.open_table(WEBHOOK_DUE)?.insert(
                due_key(delivery.created_at_unix, &delivery.id).as_str(),
                delivery.id.as_str(),
            )?;
        }
        txn.commit()?;
        Ok(true)
    }

    /// Returns due deliveries in due-time order, at most one per `(index, target)`, excluding active
    /// targets so a slow endpoint cannot starve others.
    ///
    /// # Errors
    /// Returns a store error if the scan or damaged-row cleanup fails.
    pub fn list_due_webhook_deliveries(
        &self,
        now_unix: i64,
        limit: usize,
        excluded: &HashSet<(String, String)>,
    ) -> Result<Vec<WebhookDeliveryRecord>, MetaError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let txn = self.db.begin_write()?;
        let mut records = Vec::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut cleanup = WebhookQueueCleanup::default();
        let mut damaged_due_keys = Vec::new();
        let mut damaged_delivery_ids = HashSet::new();
        {
            let due = txn.open_table(WEBHOOK_DUE)?;
            let deliveries = txn.open_table(WEBHOOK_DELIVERY)?;
            for entry in due.iter()? {
                let (key, id) = entry?;
                let Some(due_at) = due_key_time(key.value()) else {
                    cleanup.malformed_due_keys += 1;
                    damaged_due_keys.push(key.value().to_owned());
                    continue;
                };
                if due_at > now_unix {
                    break;
                }
                let Some(value) = deliveries.get(id.value())? else {
                    cleanup.dangling_due_rows += 1;
                    damaged_due_keys.push(key.value().to_owned());
                    continue;
                };
                let Ok(record) = serde_json::from_slice::<WebhookDeliveryRecord>(value.value()) else {
                    if damaged_delivery_ids.insert(id.value().to_owned()) {
                        cleanup.malformed_delivery_records += 1;
                    }
                    damaged_due_keys.push(key.value().to_owned());
                    continue;
                };
                if record.next_attempt_at_unix != Some(due_at) {
                    cleanup.dangling_due_rows += 1;
                    damaged_due_keys.push(key.value().to_owned());
                    continue;
                }
                let target = (record.index.clone(), record.target.clone());
                if excluded.contains(&target) || !seen.insert(target) {
                    continue;
                }
                records.push(record);
                if records.len() == limit {
                    break;
                }
            }
        }
        {
            let mut due = txn.open_table(WEBHOOK_DUE)?;
            for key in damaged_due_keys {
                due.remove(key.as_str())?;
            }
            let mut deliveries = txn.open_table(WEBHOOK_DELIVERY)?;
            for id in damaged_delivery_ids {
                deliveries.remove(id.as_str())?;
            }
        }
        txn.commit()?;
        cleanup.log();
        Ok(records)
    }

    /// # Errors
    /// Returns a store error if the read fails.
    pub fn next_webhook_delivery_at(&self) -> Result<Option<i64>, MetaError> {
        let txn = self.db.begin_read()?;
        let due = txn.open_table(WEBHOOK_DUE)?;
        let deliveries = txn.open_table(WEBHOOK_DELIVERY)?;
        for entry in due.iter()? {
            let (key, id) = entry?;
            let Some(due_at) = due_key_time(key.value()) else {
                continue;
            };
            let Some(value) = deliveries.get(id.value())? else {
                continue;
            };
            let Ok(record) = serde_json::from_slice::<WebhookDeliveryRecord>(value.value()) else {
                continue;
            };
            if record.next_attempt_at_unix == Some(due_at) {
                return Ok(Some(due_at));
            }
        }
        Ok(None)
    }

    /// Returns the updated record or `None` when the delivery no longer exists.
    ///
    /// A terminal attempt drops the row: only the returned record carries the outcome, so the queue
    /// stays proportional to outstanding work rather than to lifetime event volume.
    ///
    /// # Errors
    /// Returns a store error if the write fails or the record cannot be decoded or encoded.
    pub fn update_webhook_delivery(
        &self,
        id: &str,
        attempt: WebhookDeliveryAttempt<'_>,
    ) -> Result<Option<WebhookDeliveryRecord>, MetaError> {
        let txn = self.db.begin_write()?;
        let Some(mut record) = ({
            let table = txn.open_table(WEBHOOK_DELIVERY)?;
            table
                .get(id)?
                .map(|value| serde_json::from_slice::<WebhookDeliveryRecord>(value.value()))
                .transpose()?
        }) else {
            return Ok(None);
        };
        if let Some(next) = record.next_attempt_at_unix {
            let key = due_key(next, &record.id);
            txn.open_table(WEBHOOK_DUE)?.remove(key.as_str())?;
        }
        record.status = attempt.status;
        record.attempts += 1;
        record.updated_at_unix = attempt.updated_at_unix;
        record.next_attempt_at_unix = attempt.next_attempt_at_unix;
        record.response_status = attempt.response_status;
        record.last_error = attempt.last_error.map(str::to_owned);
        let pending = record.status == WebhookDeliveryStatus::Pending;
        {
            let mut deliveries = txn.open_table(WEBHOOK_DELIVERY)?;
            if pending {
                let bytes = serde_json::to_vec(&record)?;
                deliveries.insert(id, bytes.as_slice())?;
            } else {
                deliveries.remove(id)?;
            }
        }
        if pending && let Some(next) = record.next_attempt_at_unix {
            txn.open_table(WEBHOOK_DUE)?.insert(due_key(next, id).as_str(), id)?;
        }
        txn.commit()?;
        Ok(Some(record))
    }

    /// # Errors
    /// Returns a store error if the read fails or the record cannot be decoded.
    pub fn get_webhook_delivery(&self, id: &str) -> Result<Option<WebhookDeliveryRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(WEBHOOK_DELIVERY)?;
        Ok(table
            .get(id)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// Returns the deliveries still awaiting an outcome, in ID order.
    ///
    /// # Errors
    /// Returns a store error if the read fails or a record cannot be decoded.
    pub fn list_webhook_deliveries(&self) -> Result<Vec<WebhookDeliveryRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(WEBHOOK_DELIVERY)?;
        let mut deliveries = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            deliveries.push(serde_json::from_slice(value.value())?);
        }
        Ok(deliveries)
    }
}

fn insert_webhook_event(txn: &redb::WriteTransaction, event: &WebhookEventIntent) -> Result<String, MetaError> {
    if event.targets.is_empty() {
        return Err(MetaError::DriverPrecondition(
            "webhook event requires at least one target".to_owned(),
        ));
    }
    let first_delivery = {
        let mut serials = txn.open_table(SERIAL)?;
        let first_delivery = serials.get(WEBHOOK_SERIAL_KEY)?.map_or(0, |value| value.value()) + 1;
        serials.insert(
            WEBHOOK_SERIAL_KEY,
            first_delivery + u64::try_from(event.targets.len()).expect("target count fits u64") - 1,
        )?;
        first_delivery
    };
    let id = format!("we_{first_delivery:016x}");
    let deliveries = event
        .targets
        .iter()
        .enumerate()
        .map(|(offset, target)| WebhookDeliveryRecord {
            id: format!(
                "wd_{:016x}",
                first_delivery + u64::try_from(offset).expect("target count fits u64")
            ),
            index: event.index.clone(),
            target: target.clone(),
            event: event.event.clone(),
            payload: event.payload.clone(),
            status: WebhookDeliveryStatus::Pending,
            attempts: 0,
            created_at_unix: event.created_at_unix,
            updated_at_unix: event.created_at_unix,
            next_attempt_at_unix: Some(event.created_at_unix),
            response_status: None,
            last_error: None,
        })
        .collect();
    let bytes = serde_json::to_vec(&WebhookEventRecord { deliveries })?;
    txn.open_table(WEBHOOK_EVENT)?.insert(id.as_str(), bytes.as_slice())?;
    Ok(id)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WebhookEventRecord {
    deliveries: Vec<WebhookDeliveryRecord>,
}

#[derive(Default, PartialEq, Eq)]
struct WebhookQueueCleanup {
    malformed_due_keys: usize,
    dangling_due_rows: usize,
    malformed_delivery_records: usize,
}

impl WebhookQueueCleanup {
    fn log(&self) {
        if self == &Self::default() {
            return;
        }
        tracing::warn!(
            target: "peryx::webhook",
            malformed_due_keys = self.malformed_due_keys,
            dangling_due_rows = self.dangling_due_rows,
            malformed_delivery_records = self.malformed_delivery_records,
            "discarding damaged webhook queue rows"
        );
    }
}

fn due_key(timestamp: i64, id: &str) -> String {
    let sortable = u64::from_be_bytes(timestamp.to_be_bytes()) ^ (1_u64 << 63);
    format!("{sortable:020}/{id}")
}

fn due_key_time(key: &str) -> Option<i64> {
    let raw = key.split_once('/')?.0.parse::<u64>().ok()?;
    Some(i64::from_be_bytes((raw ^ (1_u64 << 63)).to_be_bytes()))
}

#[cfg(test)]
#[path = "../../tests/unit/meta/webhook_fault_tests.rs"]
mod fault_tests;
