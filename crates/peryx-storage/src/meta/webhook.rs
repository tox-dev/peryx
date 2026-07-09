use redb::{ReadableDatabase as _, ReadableTable as _};
use serde::{Deserialize, Serialize};

use super::error::MetaError;
use super::{MetaStore, SERIAL, WEBHOOK_DELIVERY, WEBHOOK_DUE, WEBHOOK_SERIAL_KEY};

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

#[derive(Debug, Clone, Copy)]
pub struct WebhookDeliveryAttempt<'a> {
    pub status: WebhookDeliveryStatus,
    pub updated_at_unix: i64,
    pub next_attempt_at_unix: Option<i64>,
    pub response_status: Option<u16>,
    pub last_error: Option<&'a str>,
}

impl MetaStore {
    /// Insert a pending webhook delivery and return its delivery ID.
    ///
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

    /// Pending webhook deliveries due at or before `now_unix`, ordered by due time.
    ///
    /// # Errors
    /// Returns a store error if the read fails or a record cannot be decoded.
    pub fn list_due_webhook_deliveries(
        &self,
        now_unix: i64,
        limit: usize,
    ) -> Result<Vec<WebhookDeliveryRecord>, MetaError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let txn = self.db.begin_read()?;
        let due = txn.open_table(WEBHOOK_DUE)?;
        let deliveries = txn.open_table(WEBHOOK_DELIVERY)?;
        let mut records = Vec::new();
        for entry in due.iter()? {
            let (key, id) = entry?;
            let Some(due_at) = due_key_time(key.value()) else {
                continue;
            };
            if due_at > now_unix {
                break;
            }
            let Some(record) = deliveries.get(id.value())? else {
                continue;
            };
            records.push(serde_json::from_slice(record.value())?);
            if records.len() == limit {
                break;
            }
        }
        Ok(records)
    }

    /// The next pending webhook retry timestamp.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn next_webhook_delivery_at(&self) -> Result<Option<i64>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(WEBHOOK_DUE)?;
        let mut entries = table.iter()?;
        Ok(match entries.next().transpose()? {
            Some((key, _)) => due_key_time(key.value()),
            None => None,
        })
    }

    /// Apply one delivery attempt result, returning the updated record when it still exists.
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
        {
            let bytes = serde_json::to_vec(&record)?;
            txn.open_table(WEBHOOK_DELIVERY)?.insert(id, bytes.as_slice())?;
            if record.status == WebhookDeliveryStatus::Pending
                && let Some(next) = record.next_attempt_at_unix
            {
                txn.open_table(WEBHOOK_DUE)?.insert(due_key(next, id).as_str(), id)?;
            }
        }
        txn.commit()?;
        Ok(Some(record))
    }

    /// Fetch one webhook delivery by ID.
    ///
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

    /// List webhook delivery records by delivery ID.
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

fn due_key(timestamp: i64, id: &str) -> String {
    let sortable = u64::from_be_bytes(timestamp.to_be_bytes()) ^ (1_u64 << 63);
    format!("{sortable:020}/{id}")
}

fn due_key_time(key: &str) -> Option<i64> {
    let raw = key.split_once('/')?.0.parse::<u64>().ok()?;
    Some(i64::from_be_bytes((raw ^ (1_u64 << 63)).to_be_bytes()))
}
