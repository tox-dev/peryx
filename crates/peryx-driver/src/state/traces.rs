//! Opening and recording the availability operation trace from the live write path.
//!
//! The [`OperationEnvelope`](peryx_ha_distributed::OperationEnvelope) carries a W3C trace context and the
//! [`OperationTelemetry`](peryx_ha_distributed::OperationTelemetry) surface renders it as a redacted
//! `availability operation` event, but nothing opened a trace or recorded the event from the live path.
//! This is that producer: at the write-path commit chokepoint a home publish opens a root W3C span keyed
//! to the operation's identity and records the redacted telemetry through the shared surface.
//!
//! The root trace-id is folded from the operation's `(source, epoch, serial, kind)` identity rather than
//! drawn from an entropy source, so the same operation opens the same trace across a re-push or an
//! epoch-replay while distinct operations stay distinct - a follower that later derives a child span joins
//! one stable trace. The sampled flag is a head decision under a fixed ratio that bounds exporter cost,
//! and [`OperationTelemetry::emit`](peryx_ha_distributed::OperationTelemetry::emit) records nothing for an
//! unsampled operation, so a publish that is not traced adds no log volume. Only the identity, kind, and
//! traceparent reach the event; the change payload never does, so no credential, artifact byte, or private
//! path is exposed.

use std::hash::{Hash as _, Hasher as _};

use peryx_ha::{AuthorityEpoch, OperationKind, OperationTelemetry, sampled};

use crate::state::ServingState;

/// The source identity a rosterless single node stamps on the operation it authors, since it names no
/// roster member of its own to identify as.
const STANDALONE_SOURCE: &str = "standalone";

/// The head-sampling ratio's numerator and denominator: one operation in [`SAMPLE_DENOMINATOR`] opens a
/// sampled trace. Availability operations track publish volume, not request volume, so a modest fraction
/// keeps the correlated traces an operator needs while bounding exporter cost far below tracing every
/// publish. The decision is a deterministic function of the operation identity, so a re-push or a replay
/// of one operation samples the same way and stays consistent across the trace.
const SAMPLE_NUMERATOR: u128 = 1;
const SAMPLE_DENOMINATOR: u128 = 16;

impl ServingState {
    /// Open and record the availability operation trace for a home publish that just committed at the
    /// write path, following the same best-effort, off-the-critical-path contract as the placement
    /// producer: it opens a root W3C span keyed to the operation's identity, sets the sampled flag under a
    /// ratio that bounds exporter cost, and records the redacted `availability operation` event through the
    /// shared telemetry surface. An unsampled operation records nothing, so a durable publish never gains
    /// log volume or a client error from tracing.
    ///
    /// `kind` is the mutation the publish performed and `fence` its authority epoch. The operation's source
    /// is this node's roster identity, or [`STANDALONE_SOURCE`] for a rosterless single node, and its
    /// serial is the committed journal head, so the trace names the operation the same way a replica does.
    pub fn record_operation_trace(&self, kind: OperationKind, fence: u64) {
        let source = self
            .availability_topology()
            .local_node
            .as_deref()
            .unwrap_or(STANDALONE_SOURCE);
        let serial = self.meta.current_serial().unwrap_or(0);
        open_ingress_trace(source, AuthorityEpoch(fence), serial, kind).emit();
    }
}

/// The redacted ingress telemetry for the operation `source` authored at `serial` under `epoch`, its
/// traceparent the freshly opened root span. The `sampled` field reads back the traceparent's own flag
/// through the shared surface, so the recorded decision is the one the wire carries, not a second copy.
fn open_ingress_trace(source: &str, epoch: AuthorityEpoch, serial: u64, kind: OperationKind) -> OperationTelemetry {
    let entropy = trace_entropy(source, epoch, serial, kind);
    let traceparent = root_traceparent(entropy, samples(entropy));
    OperationTelemetry {
        source: source.to_owned(),
        epoch: epoch.0,
        serial,
        kind: kind.as_str(),
        sampled: sampled(Some(&traceparent)),
        traceparent: Some(traceparent),
    }
}

/// Fold the operation identity into 128 bits of trace entropy across two independently salted rounds, so
/// the trace-id spans the full width rather than one 64-bit hash zero-extended.
fn trace_entropy(source: &str, epoch: AuthorityEpoch, serial: u64, kind: OperationKind) -> u128 {
    let high = fold(0xA5, source, epoch, serial, kind);
    let low = fold(0x5A, source, epoch, serial, kind);
    (u128::from(high) << 64) | u128::from(low)
}

/// One salted deterministic fold of the operation identity. [`DefaultHasher`](std::hash::DefaultHasher)
/// keys are fixed, so the digest is reproducible across processes and a replica derives the same trace-id
/// the ingress opened.
fn fold(salt: u8, source: &str, epoch: AuthorityEpoch, serial: u64, kind: OperationKind) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    salt.hash(&mut hasher);
    source.hash(&mut hasher);
    epoch.0.hash(&mut hasher);
    serial.hash(&mut hasher);
    kind.as_str().hash(&mut hasher);
    hasher.finish()
}

/// Format the root W3C traceparent for `entropy`: the trace-id is the full 128 bits and the span-id its
/// high half, each forced odd so neither is the all-zero value the spec rejects, with the sampled flag set
/// per `sampled`.
fn root_traceparent(entropy: u128, sampled: bool) -> String {
    let trace_id = entropy | 1;
    let span_id = (entropy >> 64) as u64 | 1;
    let flags = if sampled { "01" } else { "00" };
    format!("00-{trace_id:032x}-{span_id:016x}-{flags}")
}

/// Whether `entropy` opens a sampled trace under the head ratio, deterministic per operation so the whole
/// trace shares one verdict.
const fn samples(entropy: u128) -> bool {
    entropy % SAMPLE_DENOMINATOR < SAMPLE_NUMERATOR
}

#[cfg(test)]
#[path = "../../tests/unit/state/traces/tests.rs"]
mod tests;
