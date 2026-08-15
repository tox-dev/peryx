use peryx_driver::ServingState;
use peryx_events::webhook::{WebhookEnvelope, WebhookEvent};
use serde::Serialize;

pub const DELETE: &str = "delete";
pub const RESTORE: &str = "restore";
pub const UNYANK: &str = "unyank";
pub const UPLOAD: &str = "upload";
pub const YANK: &str = "yank";

#[derive(Clone, Copy)]
pub struct PypiWebhook<'a> {
    pub event: &'static str,
    pub created_at_unix: i64,
    pub index: &'a str,
    pub route: &'a str,
    pub hosted_index: &'a str,
    pub project: &'a str,
    pub version: Option<&'a str>,
    pub filename: Option<&'a str>,
    pub digest: Option<&'a str>,
    pub count: usize,
    pub actor: Option<&'a str>,
    pub request_id: Option<&'a str>,
}

pub fn emit(state: &ServingState, webhook: PypiWebhook<'_>) {
    peryx_events::webhook::emit(
        state,
        &WebhookEvent {
            created_at_unix: webhook.created_at_unix,
            index: webhook.index.to_owned(),
            envelope: WebhookEnvelope::new(
                "pypi.v1",
                webhook.event,
                serde_json::to_value(PypiPayload::from(&webhook)).expect("PyPI webhook payload is serializable"),
            ),
        },
    );
}

#[derive(Serialize)]
struct PypiPayload<'a> {
    event: &'static str,
    created_at: i64,
    index: &'a str,
    route: &'a str,
    hosted_index: &'a str,
    project: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<PypiFile<'a>>,
    count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<&'a str>,
}

impl<'a> From<&PypiWebhook<'a>> for PypiPayload<'a> {
    fn from(webhook: &PypiWebhook<'a>) -> Self {
        Self {
            event: webhook.event,
            created_at: webhook.created_at_unix,
            index: webhook.index,
            route: webhook.route,
            hosted_index: webhook.hosted_index,
            project: webhook.project,
            version: webhook.version,
            file: webhook.filename.map(|filename| PypiFile {
                filename,
                sha256: webhook.digest,
            }),
            count: webhook.count,
            actor: webhook.actor,
            request_id: webhook.request_id,
        }
    }
}

#[derive(Serialize)]
struct PypiFile<'a> {
    filename: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<&'a str>,
}
