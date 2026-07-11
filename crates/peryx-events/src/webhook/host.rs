//! What webhook delivery needs from the running process.

use peryx_storage::meta::MetaStore;

use super::runtime::WebhookRuntime;

/// The process state signed webhook delivery borrows: the configured targets and their HTTP client,
/// the store the delivery queue lives in, and the clock that dates an attempt.
///
/// Delivery is spawned as a background task, so it holds its host in an `Arc` for as long as the
/// queue is draining. Taking the host through a trait keeps this crate from naming whichever struct
/// the process happens to assemble those three things on, and keeps [`emit`](super::emit) generic
/// rather than dynamically dispatched.
pub trait WebhookHost: Send + Sync + 'static {
    /// The configured targets, their subscriptions, and the shared HTTP client.
    fn webhooks(&self) -> &WebhookRuntime;

    /// The store holding the durable delivery queue.
    fn meta(&self) -> &MetaStore;

    /// The current unix time, injectable so retry backoff is deterministic in tests.
    fn now(&self) -> i64;
}
