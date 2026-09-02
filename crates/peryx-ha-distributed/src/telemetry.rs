//! Telemetry includes operation identity, kind, and traceparent, but excludes the change payload. This
//! keeps credentials, artifact bytes, and private paths out of logs.
//!
//! A write-path trace is opened where the acknowledgement resolves, so the record carries the write's
//! own commit receipt and the evidence the decision was made from. Emission is unconditional there: a
//! mutation is rare next to a read, and the record only exists to answer questions about one.
//!
//! Envelope telemetry is the receiving half and keeps its own sampling: a traceparent that arrives from
//! a peer carries the sampling that peer chose. The apply side joins the author trace through
//! [`derive_child`](crate::derive_child).

use crate::envelope::OperationEnvelope;
use peryx_ha::{
    BlobAckObservation, ByteAckDecision, ByteEvidence, MetadataAckObservation, OperationTrace, SourceFailure,
};
use peryx_storage::blob::BlobDurability;

const NO_SCOPE: &str = "none";

/// Records what a blob write's acknowledgement proved, under the trace opened for that write.
///
/// A write that journaled nothing has no serial, and the field is then absent rather than reported as
/// the zero a reader would mistake for the first commit.
pub fn record_blob_ack(trace: &OperationTrace, observation: &BlobAckObservation<'_>) {
    // Bound outside the macro: a field expression runs only while a subscriber is enabled.
    let identity = Identity::of(trace);
    let bytes = ByteFields::of(observation.bytes);
    let nodes = bytes.nodes.as_str();
    let scope = observation.outcome.scope().map_or(NO_SCOPE, BlobDurability::as_str);
    let outcome = observation.outcome.as_str();
    let policy = observation.policy.as_str();
    let waited = observation.waited.as_secs_f64();
    let bytes_retired = retired_field(observation.bytes_retired);
    let metadata_retired = retired_field(observation.metadata_retired);
    tracing::info!(
        operation.traceparent = identity.traceparent,
        operation.source = identity.source,
        operation.authority = identity.authority,
        operation.epoch = identity.epoch,
        operation.serial = identity.serial,
        operation.kind = identity.kind,
        ack.policy = policy,
        ack.outcome = outcome,
        ack.scope = scope,
        ack.evidence = bytes.evidence,
        ack.nodes = nodes,
        ack.required = bytes.required,
        ack.remaining = bytes.remaining,
        ack.bytes_acknowledged = bytes.acknowledged,
        ack.metadata_acknowledged = observation.metadata_acknowledged,
        ack.bytes_expired = observation.bytes_expired,
        ack.metadata_expired = observation.metadata_expired,
        ack.bytes_retired = bytes_retired,
        ack.metadata_retired = metadata_retired,
        ack.waited_seconds = waited,
        "availability blob write acknowledged",
    );
}

/// Records what a metadata-only write's acknowledgement proved, under the trace opened for that write.
///
/// Metadata holds its own bytes, so the journal frontier is the whole proof and there is no second
/// dimension to report.
pub fn record_metadata_ack(trace: &OperationTrace, observation: MetadataAckObservation, retired: &[SourceFailure]) {
    let identity = Identity::of(trace);
    let policy = observation.policy.as_str();
    let waited = observation.waited.as_secs_f64();
    let outcome = observation.decision.as_str();
    let retired = retired_field(retired);
    tracing::info!(
        operation.traceparent = identity.traceparent,
        operation.source = identity.source,
        operation.authority = identity.authority,
        operation.epoch = identity.epoch,
        operation.serial = identity.serial,
        operation.kind = identity.kind,
        ack.policy = policy,
        ack.outcome = outcome,
        ack.evidence = "journal-frontier",
        ack.expired = observation.timed_out,
        ack.retired = retired,
        ack.waited_seconds = waited,
        "availability metadata write acknowledged",
    );
}

/// Joins retired sources as `source=reason` pairs, the way counted node names are joined. `reason` is a
/// bounded token from the transport, so no response body or credential text reaches the record.
///
/// A dimension that stopped short without its budget expiring names the failure that stopped it here,
/// which is the difference between a peer that rejected the credential and a peer that is behind.
fn retired_field(retired: &[SourceFailure]) -> String {
    let mut field = String::new();
    for failure in retired {
        if !field.is_empty() {
            field.push(',');
        }
        field.push_str(&failure.source);
        field.push('=');
        field.push_str(failure.reason);
    }
    field
}

/// The trace fields every acknowledgement record shares, borrowed as log-ready values.
struct Identity<'trace> {
    traceparent: &'trace str,
    source: &'trace str,
    authority: &'trace str,
    epoch: u64,
    serial: Option<u64>,
    kind: &'static str,
}

impl<'trace> Identity<'trace> {
    fn of(trace: &'trace OperationTrace) -> Self {
        Self {
            traceparent: &trace.traceparent,
            source: &trace.operation.source,
            authority: &trace.operation.authority,
            epoch: trace.operation.epoch.0,
            serial: trace.operation.serial,
            kind: trace.operation.kind.as_str(),
        }
    }
}

/// The byte dimension's counted receipts, flattened to log fields. An object store publishes the one
/// copy every reader shares, so it counts no node receipt and has no threshold to report.
struct ByteFields {
    evidence: &'static str,
    nodes: String,
    required: usize,
    remaining: usize,
    acknowledged: bool,
}

impl ByteFields {
    fn of(evidence: &ByteEvidence) -> Self {
        match evidence {
            ByteEvidence::Filesystem(ByteAckDecision::Acknowledged { nodes, required }) => Self {
                evidence: "filesystem",
                nodes: nodes.join(","),
                required: *required,
                remaining: 0,
                acknowledged: true,
            },
            ByteEvidence::Filesystem(ByteAckDecision::Pending {
                nodes,
                required,
                remaining,
            }) => Self {
                evidence: "filesystem",
                nodes: nodes.join(","),
                required: *required,
                remaining: *remaining,
                acknowledged: false,
            },
            ByteEvidence::ObjectStore { acknowledged } => Self {
                evidence: "object-store",
                nodes: String::new(),
                required: 0,
                remaining: 0,
                acknowledged: *acknowledged,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OperationTelemetry {
    pub source: String,
    pub epoch: u64,
    pub serial: u64,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    pub sampled: bool,
}

impl OperationTelemetry {
    pub fn emit(&self) {
        let Some(traceparent) = self.traceparent.as_deref().filter(|_| self.sampled) else {
            return;
        };
        tracing::info!(
            operation.source = %self.source,
            operation.epoch = self.epoch,
            operation.serial = self.serial,
            operation.kind = self.kind,
            operation.traceparent = traceparent,
            "availability operation",
        );
    }
}

#[must_use]
pub fn sampled(traceparent: Option<&str>) -> bool {
    traceparent
        .map(|value| value.rsplit_once('-').map_or(value, |(_, flags)| flags))
        .filter(|flags| flags.len() == 2)
        .and_then(|flags| u8::from_str_radix(flags, 16).ok())
        .is_some_and(|flags| flags & 0x01 != 0)
}

/// Copies log-safe identity fields without exposing the change payload.
#[must_use]
pub fn operation_telemetry(envelope: &OperationEnvelope) -> OperationTelemetry {
    let identity = envelope.identity();
    let traceparent = envelope.trace.as_ref().map(|trace| trace.traceparent.clone());
    OperationTelemetry {
        source: identity.source.to_owned(),
        epoch: identity.epoch.0,
        serial: identity.serial,
        kind: envelope.kind.as_str(),
        sampled: sampled(traceparent.as_deref()),
        traceparent,
    }
}

#[cfg(test)]
#[path = "../tests/unit/telemetry/tests.rs"]
mod tests;
