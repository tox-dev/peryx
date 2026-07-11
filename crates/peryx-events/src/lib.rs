//! What a peryx process reports about itself.
//!
//! Three subsystems that observe serving without being part of it: [`metrics`] aggregates usage
//! counters off the request path, [`security`] logs structured events at mutation points, and
//! [`webhook`] signs and delivers those mutations to configured targets.
//!
//! None of them knows about HTTP routing or the process's serving state. Webhook delivery needs a
//! runtime, a metadata store and a clock, and takes them through the [`WebhookHost`](webhook::WebhookHost)
//! trait rather than reaching into whatever struct happens to hold them.

pub mod metrics;
pub mod security;
pub mod webhook;
