use peryx_storage::meta::MetaStore;

use super::runtime::WebhookRuntime;

/// Keeps delivery independent of the process composition root.
pub trait WebhookHost: Send + Sync + 'static {
    fn webhooks(&self) -> &WebhookRuntime;

    fn meta(&self) -> &MetaStore;

    fn now(&self) -> i64;
}
