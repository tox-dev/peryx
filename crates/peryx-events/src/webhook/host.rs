use std::collections::HashSet;

use peryx_storage::meta::{MetaError, MetaStore, WebhookDeliveryAttempt, WebhookDeliveryRecord};

use super::runtime::WebhookRuntime;

/// Keeps delivery independent of the process composition root.
pub trait WebhookHost: Send + Sync + 'static {
    fn webhooks(&self) -> &WebhookRuntime;

    fn meta(&self) -> &MetaStore;

    fn now(&self) -> i64;

    /// # Errors
    /// Returns a metadata error when the due queue cannot be scanned.
    fn list_due_webhook_deliveries(
        &self,
        now_unix: i64,
        limit: usize,
        excluded: &HashSet<(String, String)>,
    ) -> Result<Vec<WebhookDeliveryRecord>, MetaError> {
        self.meta().list_due_webhook_deliveries(now_unix, limit, excluded)
    }

    /// # Errors
    /// Returns a metadata error when the next delivery deadline cannot be read.
    fn next_webhook_delivery_at(&self) -> Result<Option<i64>, MetaError> {
        self.meta().next_webhook_delivery_at()
    }

    /// # Errors
    /// Returns a metadata error when the delivery result cannot be stored.
    fn update_webhook_delivery(
        &self,
        id: &str,
        attempt: WebhookDeliveryAttempt<'_>,
    ) -> Result<Option<WebhookDeliveryRecord>, MetaError> {
        self.meta().update_webhook_delivery(id, attempt)
    }
}
