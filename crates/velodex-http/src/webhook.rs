//! Signed webhook delivery for repository mutations.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use url::Url;
use velodex_storage::meta::{
    MetaError, NewWebhookDelivery, WebhookDeliveryAttempt, WebhookDeliveryRecord, WebhookDeliveryStatus,
};

use crate::state::AppState;

const DELIVERY_BATCH: usize = 32;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);
const INITIAL_BACKOFF_SECS: i64 = 5;
const MAX_BACKOFF_SECS: i64 = 300;
const MAX_ATTEMPTS: u16 = 5;

pub struct WebhookRuntime {
    client: reqwest::Client,
    targets: HashMap<String, Vec<WebhookTarget>>,
    running: AtomicBool,
    notify: tokio::sync::Notify,
}

impl WebhookRuntime {
    /// Runtime with no configured targets.
    #[must_use]
    pub fn disabled() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Self {
            client: reqwest::Client::new(),
            targets: HashMap::new(),
            running: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Build a runtime from resolved configuration.
    ///
    /// # Errors
    /// Returns an error for duplicate target names, invalid URLs, empty secrets, or unknown events.
    pub fn new(configs: Vec<WebhookTargetConfig>) -> Result<Self, WebhookConfigError> {
        let mut seen = HashSet::new();
        let mut targets: HashMap<String, Vec<WebhookTarget>> = HashMap::new();
        for config in configs {
            if config.name.is_empty() {
                return Err(WebhookConfigError::EmptyName { index: config.index });
            }
            if config.secret.is_empty() {
                return Err(WebhookConfigError::EmptySecret {
                    index: config.index,
                    target: config.name,
                });
            }
            if !seen.insert((config.index.clone(), config.name.clone())) {
                return Err(WebhookConfigError::Duplicate {
                    index: config.index,
                    target: config.name,
                });
            }
            targets.entry(config.index).or_default().push(WebhookTarget {
                name: config.name,
                url: target_url(&config.url)?,
                secret: config.secret,
                events: WebhookEvents::new(config.events)?,
            });
        }
        Ok(Self {
            targets,
            ..Self::disabled()
        })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    fn target_names(&self, index: &str, event: WebhookEventKind) -> Vec<String> {
        self.targets.get(index).map_or_else(Vec::new, |targets| {
            targets
                .iter()
                .filter(|target| target.events.matches(event))
                .map(|target| target.name.clone())
                .collect()
        })
    }

    fn target(&self, index: &str, name: &str) -> Option<WebhookTarget> {
        self.targets
            .get(index)?
            .iter()
            .find(|target| target.name == name)
            .cloned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookTargetConfig {
    pub index: String,
    pub name: String,
    pub url: String,
    pub secret: String,
    pub events: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WebhookConfigError {
    #[error("webhook target name is empty on index {index}")]
    EmptyName { index: String },
    #[error("webhook target {target} on index {index} has an empty secret")]
    EmptySecret { index: String, target: String },
    #[error("duplicate webhook target {target} on index {index}")]
    Duplicate { index: String, target: String },
    #[error("webhook target URL {url:?} is invalid: {source}")]
    InvalidUrl { url: String, source: url::ParseError },
    #[error("webhook target URL {url:?} must use http or https")]
    InvalidScheme { url: String },
    #[error("webhook target URL {url:?} must not include credentials, query, or fragment")]
    SensitiveUrlParts { url: String },
    #[error("unknown webhook event {0:?}")]
    UnknownEvent(String),
}

#[derive(Debug, Clone)]
struct WebhookTarget {
    name: String,
    url: Url,
    secret: String,
    events: WebhookEvents,
}

#[derive(Debug, Clone)]
struct WebhookEvents {
    all: bool,
    events: HashSet<WebhookEventKind>,
}

impl WebhookEvents {
    fn new(names: Vec<String>) -> Result<Self, WebhookConfigError> {
        if names.is_empty() {
            return Ok(Self {
                all: true,
                events: HashSet::new(),
            });
        }
        Ok(Self {
            all: false,
            events: names
                .into_iter()
                .map(|name| WebhookEventKind::parse(&name).ok_or(WebhookConfigError::UnknownEvent(name)))
                .collect::<Result<_, _>>()?,
        })
    }

    fn matches(&self, event: WebhookEventKind) -> bool {
        self.all || self.events.contains(&event)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WebhookEventKind {
    Upload,
    Yank,
    Unyank,
    Delete,
    Restore,
    Promote,
    ProjectStatus,
    Management,
}

impl WebhookEventKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Yank => "yank",
            Self::Unyank => "unyank",
            Self::Delete => "delete",
            Self::Restore => "restore",
            Self::Promote => "promote",
            Self::ProjectStatus => "project-status",
            Self::Management => "management",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "upload" => Some(Self::Upload),
            "yank" => Some(Self::Yank),
            "unyank" => Some(Self::Unyank),
            "delete" => Some(Self::Delete),
            "restore" => Some(Self::Restore),
            "promote" => Some(Self::Promote),
            "project-status" => Some(Self::ProjectStatus),
            "management" => Some(Self::Management),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEvent {
    pub kind: WebhookEventKind,
    pub created_at_unix: i64,
    pub index: String,
    pub route: String,
    pub local_index: String,
    pub project: String,
    pub version: Option<String>,
    pub filename: Option<String>,
    pub digest: Option<String>,
    pub count: usize,
    pub actor: Option<String>,
    pub request_id: Option<String>,
}

impl WebhookEvent {
    fn payload(&self) -> WebhookPayload<'_> {
        WebhookPayload {
            event: self.kind.as_str(),
            created_at: self.created_at_unix,
            index: &self.index,
            route: &self.route,
            local_index: &self.local_index,
            project: &self.project,
            version: self.version.as_deref(),
            file: self.filename.as_deref().map(|filename| WebhookFile {
                filename,
                sha256: self.digest.as_deref(),
            }),
            count: self.count,
            actor: self.actor.as_deref(),
            request_id: self.request_id.as_deref(),
        }
    }
}

#[derive(Serialize)]
struct WebhookPayload<'a> {
    event: &'static str,
    created_at: i64,
    index: &'a str,
    route: &'a str,
    local_index: &'a str,
    project: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<WebhookFile<'a>>,
    count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<&'a str>,
}

#[derive(Serialize)]
struct WebhookFile<'a> {
    filename: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<&'a str>,
}

/// Enqueue signed webhook deliveries for `event` to every configured target subscribed to its kind.
///
/// A no-op when no webhooks are configured or none subscribe to the event's kind.
///
/// # Panics
/// Panics only if the aggregation lock is poisoned; the payload is all JSON primitives and cannot
/// fail to serialize.
pub fn emit(state: Arc<AppState>, event: &WebhookEvent) {
    if state.webhooks.is_empty() {
        return;
    }
    let targets = state.webhooks.target_names(&event.index, event.kind);
    if targets.is_empty() {
        return;
    }
    let payload = serde_json::to_string(&event.payload()).expect("webhook payload contains JSON primitives");
    let event_name = event.kind.as_str();
    let mut enqueued = 0;
    for target in targets {
        let result = state.meta.enqueue_webhook_delivery(NewWebhookDelivery {
            index: &event.index,
            target: &target,
            event: event_name,
            payload: &payload,
            created_at_unix: event.created_at_unix,
        });
        log_enqueue_error(result.as_ref().err(), event, &target);
        if result.is_ok() {
            enqueued += 1;
        }
    }
    if enqueued > 0 {
        kick(state);
    }
}

pub fn kick(state: Arc<AppState>) {
    if state.webhooks.running.swap(true, Ordering::AcqRel) {
        state.webhooks.notify.notify_one();
        return;
    }
    tokio::spawn(delivery_loop(state));
}

async fn delivery_loop(state: Arc<AppState>) {
    loop {
        deliver_due(&state).await;
        let result = state.meta.next_webhook_delivery_at();
        log_next_delivery_error(result.as_ref().err());
        let Some(next) = result.ok().flatten() else {
            state.webhooks.notify.notified().await;
            continue;
        };
        let now = (state.clock)();
        let sleep_secs = u64::try_from(next - now).unwrap_or(0);
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(sleep_secs)) => {}
            () = state.webhooks.notify.notified() => {}
        }
    }
}

async fn deliver_due(state: &Arc<AppState>) {
    loop {
        let now = (state.clock)();
        let result = state.meta.list_due_webhook_deliveries(now, DELIVERY_BATCH);
        log_queue_scan_error(result.as_ref().err());
        let deliveries = result.unwrap_or_default();
        if deliveries.is_empty() {
            return;
        }
        for delivery in deliveries {
            deliver_one(state, delivery).await;
        }
    }
}

async fn deliver_one(state: &Arc<AppState>, delivery: WebhookDeliveryRecord) {
    let now = (state.clock)();
    let Some(target) = state.webhooks.target(&delivery.index, &delivery.target) else {
        record_failure(state, &delivery, now, None, "webhook target is not configured");
        return;
    };
    let signature = signature(&target.secret, now, &delivery.id, delivery.payload.as_bytes());
    let result = state
        .webhooks
        .client
        .post(target.url)
        .timeout(DELIVERY_TIMEOUT)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::USER_AGENT,
            concat!("velodex/", env!("CARGO_PKG_VERSION")),
        )
        .header("x-velodex-event", delivery.event.as_str())
        .header("x-velodex-delivery", delivery.id.as_str())
        .header("x-velodex-timestamp", now.to_string())
        .header("x-velodex-signature", signature)
        .body(delivery.payload.clone())
        .send()
        .await;
    match result {
        Ok(response) if response.status().is_success() => {
            record_success(state, &delivery, now, response.status().as_u16());
        }
        Ok(response) => {
            let status = response.status().as_u16();
            record_failure(state, &delivery, now, Some(status), &format!("http status {status}"));
        }
        Err(err) => {
            record_failure(state, &delivery, now, None, &err.without_url().to_string());
        }
    }
}

fn record_success(state: &AppState, delivery: &WebhookDeliveryRecord, now: i64, status: u16) {
    let result = state.meta.update_webhook_delivery(
        &delivery.id,
        WebhookDeliveryAttempt {
            status: WebhookDeliveryStatus::Delivered,
            updated_at_unix: now,
            next_attempt_at_unix: None,
            response_status: Some(status),
            last_error: None,
        },
    );
    log_update_error(result.as_ref().err());
    log_delivery_success(result.as_ref().ok().and_then(Option::as_ref), status);
}

fn log_delivery_success(record: Option<&WebhookDeliveryRecord>, status: u16) {
    if let Some(record) = record {
        tracing::info!(
            target: "velodex::webhook",
            delivery = %record.id,
            index = %record.index,
            target = %record.target,
            event = %record.event,
            attempts = record.attempts,
            status,
            "webhook delivery succeeded"
        );
    }
}

fn record_failure(
    state: &AppState,
    delivery: &WebhookDeliveryRecord,
    now: i64,
    response_status: Option<u16>,
    error: &str,
) {
    let attempts = delivery.attempts + 1;
    let (status, next_attempt_at_unix) = if attempts >= MAX_ATTEMPTS {
        (WebhookDeliveryStatus::Failed, None)
    } else {
        (WebhookDeliveryStatus::Pending, Some(now + backoff_secs(attempts)))
    };
    let result = state.meta.update_webhook_delivery(
        &delivery.id,
        WebhookDeliveryAttempt {
            status,
            updated_at_unix: now,
            next_attempt_at_unix,
            response_status,
            last_error: Some(error),
        },
    );
    log_update_error(result.as_ref().err());
    log_delivery_failure(result.as_ref().ok().and_then(Option::as_ref));
}

fn log_delivery_failure(record: Option<&WebhookDeliveryRecord>) {
    if let Some(record) = record {
        tracing::warn!(
            target: "velodex::webhook",
            delivery = %record.id,
            index = %record.index,
            target = %record.target,
            event = %record.event,
            attempts = record.attempts,
            response_status = ?record.response_status,
            next_attempt_at_unix = ?record.next_attempt_at_unix,
            status = ?record.status,
            "webhook delivery failed"
        );
    }
}

fn log_enqueue_error(err: Option<&MetaError>, event: &WebhookEvent, target: &str) {
    if let Some(err) = err {
        let event_name = event.kind.as_str();
        tracing::error!(
            target: "velodex::webhook",
            error = ?err,
            index = %event.index,
            target = %target,
            event = event_name,
            "webhook delivery could not be queued"
        );
    }
}

fn log_next_delivery_error(err: Option<&MetaError>) {
    if let Some(err) = err {
        tracing::error!(target: "velodex::webhook", error = ?err, "webhook queue scheduling failed");
    }
}

fn log_queue_scan_error(err: Option<&MetaError>) {
    if let Some(err) = err {
        tracing::error!(target: "velodex::webhook", error = ?err, "webhook queue scan failed");
    }
}

fn log_update_error(err: Option<&MetaError>) {
    if let Some(err) = err {
        tracing::error!(target: "velodex::webhook", error = ?err, "webhook result update failed");
    }
}

fn backoff_secs(attempts: u16) -> i64 {
    let mut secs = INITIAL_BACKOFF_SECS;
    for _ in 1..attempts {
        secs = (secs * 3).min(MAX_BACKOFF_SECS);
    }
    secs
}

#[must_use]
pub fn signature(secret: &str, timestamp: i64, delivery: &str, body: &[u8]) -> String {
    let mut message = timestamp.to_string();
    message.push('.');
    message.push_str(delivery);
    message.push('.');
    let mut mac = HmacSha256::new(secret.as_bytes());
    mac.update(message.as_bytes());
    mac.update(body);
    format!("sha256={}", hex(&mac.finalize()))
}

struct HmacSha256 {
    inner: Sha256,
    outer_key: [u8; 64],
}

impl HmacSha256 {
    fn new(key: &[u8]) -> Self {
        let mut block = [0_u8; 64];
        if key.len() > block.len() {
            block[..32].copy_from_slice(&Sha256::digest(key));
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        let mut inner_key = [0x36_u8; 64];
        let mut outer_key = [0x5c_u8; 64];
        for (index, byte) in block.iter().enumerate() {
            inner_key[index] ^= byte;
            outer_key[index] ^= byte;
        }
        let mut inner = Sha256::new();
        inner.update(inner_key);
        Self { inner, outer_key }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    fn finalize(self) -> [u8; 32] {
        let mut outer = Sha256::new();
        outer.update(self.outer_key);
        outer.update(self.inner.finalize());
        let digest = outer.finalize();
        let mut out = [0_u8; 32];
        out.copy_from_slice(&digest);
        out
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn target_url(raw: &str) -> Result<Url, WebhookConfigError> {
    let url = Url::parse(raw).map_err(|source| WebhookConfigError::InvalidUrl {
        url: raw.to_owned(),
        source,
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(WebhookConfigError::InvalidScheme { url: raw.to_owned() });
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err(WebhookConfigError::SensitiveUrlParts { url: raw.to_owned() });
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signature_matches_hmac_sha256_vector() {
        assert_eq!(
            signature("key", 123, "wd_1", b"body"),
            "sha256=1c3e3ab3893bda6e5538c2f6f4dfaecb81b85dd27ea9243206d7237a65a33355"
        );
    }

    #[test]
    fn test_hmac_hashes_long_keys() {
        let mut mac = HmacSha256::new(&[0xaa; 131]);
        mac.update(b"Test Using Larger Than Block-Size Key - Hash Key First");

        assert_eq!(
            hex(&mac.finalize()),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn test_backoff_caps() {
        assert_eq!(backoff_secs(1), 5);
        assert_eq!(backoff_secs(3), 45);
        assert_eq!(backoff_secs(10), 300);
    }

    #[test]
    fn test_event_names_roundtrip() {
        for (kind, name) in [
            (WebhookEventKind::Upload, "upload"),
            (WebhookEventKind::Yank, "yank"),
            (WebhookEventKind::Unyank, "unyank"),
            (WebhookEventKind::Delete, "delete"),
            (WebhookEventKind::Restore, "restore"),
            (WebhookEventKind::Promote, "promote"),
            (WebhookEventKind::ProjectStatus, "project-status"),
            (WebhookEventKind::Management, "management"),
        ] {
            assert_eq!(kind.as_str(), name);
            assert_eq!(WebhookEventKind::parse(name), Some(kind));
        }
    }

    #[test]
    fn test_runtime_matches_all_events_when_no_filter_is_set() {
        let runtime = WebhookRuntime::new(vec![WebhookTargetConfig {
            index: "local".to_owned(),
            name: "ci".to_owned(),
            url: "https://ci.example/hook".to_owned(),
            secret: "secret".to_owned(),
            events: Vec::new(),
        }])
        .unwrap();

        assert_eq!(runtime.target_names("local", WebhookEventKind::Upload), ["ci"]);
        assert_eq!(runtime.target_names("local", WebhookEventKind::Management), ["ci"]);
        assert!(runtime.target_names("other", WebhookEventKind::Upload).is_empty());
    }

    #[test]
    fn test_runtime_rejects_invalid_target_config() {
        assert!(matches!(
            WebhookRuntime::new(vec![WebhookTargetConfig {
                index: "local".to_owned(),
                name: String::new(),
                url: "https://ci.example/hook".to_owned(),
                secret: "secret".to_owned(),
                events: Vec::new(),
            }]),
            Err(WebhookConfigError::EmptyName { .. })
        ));
        assert!(matches!(
            WebhookRuntime::new(vec![WebhookTargetConfig {
                index: "local".to_owned(),
                name: "ci".to_owned(),
                url: "https://ci.example/hook".to_owned(),
                secret: String::new(),
                events: Vec::new(),
            }]),
            Err(WebhookConfigError::EmptySecret { .. })
        ));
        assert!(matches!(
            WebhookRuntime::new(vec![
                WebhookTargetConfig {
                    index: "local".to_owned(),
                    name: "ci".to_owned(),
                    url: "https://ci.example/hook".to_owned(),
                    secret: "secret".to_owned(),
                    events: Vec::new(),
                },
                WebhookTargetConfig {
                    index: "local".to_owned(),
                    name: "ci".to_owned(),
                    url: "https://ci.example/other".to_owned(),
                    secret: "secret".to_owned(),
                    events: Vec::new(),
                },
            ]),
            Err(WebhookConfigError::Duplicate { .. })
        ));
        assert!(matches!(
            WebhookRuntime::new(vec![WebhookTargetConfig {
                index: "local".to_owned(),
                name: "ci".to_owned(),
                url: "not a url".to_owned(),
                secret: "secret".to_owned(),
                events: Vec::new(),
            }]),
            Err(WebhookConfigError::InvalidUrl { .. })
        ));
        assert!(matches!(
            WebhookRuntime::new(vec![WebhookTargetConfig {
                index: "local".to_owned(),
                name: "ci".to_owned(),
                url: "https://ci.example/hook?token=secret".to_owned(),
                secret: "secret".to_owned(),
                events: Vec::new(),
            }]),
            Err(WebhookConfigError::SensitiveUrlParts { .. })
        ));
    }

    #[test]
    fn test_runtime_rejects_unknown_event() {
        assert!(matches!(
            WebhookRuntime::new(vec![WebhookTargetConfig {
                index: "local".to_owned(),
                name: "ci".to_owned(),
                url: "https://ci.example/hook".to_owned(),
                secret: "secret".to_owned(),
                events: vec!["bogus".to_owned()],
            }]),
            Err(WebhookConfigError::UnknownEvent(event)) if event == "bogus"
        ));
    }

    #[test]
    fn test_error_log_helpers_accept_store_errors() {
        let err = MetaError::Decode(serde_json::from_str::<serde_json::Value>("{").unwrap_err());
        let event = WebhookEvent {
            kind: WebhookEventKind::Upload,
            created_at_unix: 1,
            index: "local".to_owned(),
            route: "local".to_owned(),
            local_index: "local".to_owned(),
            project: "demo".to_owned(),
            version: None,
            filename: None,
            digest: None,
            count: 1,
            actor: None,
            request_id: None,
        };

        log_enqueue_error(Some(&err), &event, "ci");
        log_next_delivery_error(Some(&err));
        log_queue_scan_error(Some(&err));
        log_update_error(Some(&err));
        log_enqueue_error(None, &event, "ci");
        log_next_delivery_error(None);
        log_queue_scan_error(None);
        log_update_error(None);

        let record = WebhookDeliveryRecord {
            id: "wd_1".to_owned(),
            index: "local".to_owned(),
            target: "ci".to_owned(),
            event: "upload".to_owned(),
            payload: "{}".to_owned(),
            status: WebhookDeliveryStatus::Delivered,
            attempts: 1,
            created_at_unix: 1,
            updated_at_unix: 2,
            next_attempt_at_unix: None,
            response_status: Some(204),
            last_error: None,
        };
        log_delivery_success(Some(&record), 204);
        log_delivery_success(None, 204);
        log_delivery_failure(Some(&WebhookDeliveryRecord {
            status: WebhookDeliveryStatus::Pending,
            response_status: Some(500),
            last_error: Some("http status 500".to_owned()),
            ..record
        }));
        log_delivery_failure(None);
    }
}
