use std::sync::Arc;

use peryx_driver::ServingState;
use peryx_events::webhook::{WebhookEnvelope, WebhookEvent};
use peryx_index::Index;
use serde::Serialize;

pub const BLOB_DELETE: &str = "blob-delete";
pub const MANIFEST_DELETE: &str = "manifest-delete";
pub const MANIFEST_PUSH: &str = "manifest-push";
pub const MANIFEST_RESTORE: &str = "manifest-restore";
pub const EVENTS: &[&str] = &[BLOB_DELETE, MANIFEST_DELETE, MANIFEST_PUSH, MANIFEST_RESTORE];

pub struct OciWebhook<'a> {
    pub event: &'static str,
    pub index: &'a Index,
    pub repository: &'a str,
    pub reference: Option<&'a str>,
    pub digest: Option<&'a str>,
    pub actor: Option<String>,
    pub request_id: Option<String>,
}

pub fn emit(state: &Arc<ServingState>, webhook: &OciWebhook<'_>) {
    let created_at_unix = (state.clock)();
    peryx_events::webhook::emit(
        state.as_ref(),
        &WebhookEvent {
            created_at_unix,
            index: webhook.index.name.clone(),
            envelope: WebhookEnvelope::new(
                "oci.v1",
                webhook.event,
                serde_json::to_value(OciPayload {
                    schema: "oci.v1",
                    event: webhook.event,
                    created_at: created_at_unix,
                    index: &webhook.index.name,
                    route: &webhook.index.route,
                    actor: webhook.actor.as_deref(),
                    request_id: webhook.request_id.as_deref(),
                    data: OciData {
                        repository: webhook.repository,
                        reference: webhook.reference,
                        digest: webhook.digest,
                    },
                })
                .expect("OCI webhook payload is serializable"),
            ),
        },
    );
}

#[derive(Serialize)]
struct OciPayload<'a> {
    schema: &'static str,
    event: &'static str,
    created_at: i64,
    index: &'a str,
    route: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<&'a str>,
    data: OciData<'a>,
}

#[derive(Serialize)]
struct OciData<'a> {
    repository: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reference: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<&'a str>,
}
