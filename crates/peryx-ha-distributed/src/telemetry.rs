//! Telemetry includes operation identity, kind, and traceparent, but excludes the change payload. This
//! keeps credentials, artifact bytes, and private paths out of logs.
//!
//! Emission requires the trace's sampled flag. The apply side joins the author trace through
//! [`derive_child`](crate::derive_child).

use crate::envelope::OperationEnvelope;
use peryx_ha::{OperationObservation, OperationObserver};

pub struct DistributedOperationObserver;

impl OperationObserver for DistributedOperationObserver {
    fn record(&self, operation: OperationObservation) {
        let entropy = trace_entropy(&operation);
        let traceparent = root_traceparent(entropy, samples(entropy));
        OperationTelemetry {
            source: operation.source,
            epoch: operation.epoch.0,
            serial: operation.serial,
            kind: operation.kind.as_str(),
            sampled: sampled(Some(&traceparent)),
            traceparent: Some(traceparent),
        }
        .emit();
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

fn trace_entropy(operation: &OperationObservation) -> u128 {
    use std::hash::{Hash as _, Hasher as _};

    fn fold(salt: u8, operation: &OperationObservation) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        salt.hash(&mut hasher);
        operation.source.hash(&mut hasher);
        operation.epoch.0.hash(&mut hasher);
        operation.serial.hash(&mut hasher);
        operation.kind.as_str().hash(&mut hasher);
        hasher.finish()
    }

    (u128::from(fold(0xA5, operation)) << 64) | u128::from(fold(0x5A, operation))
}

fn root_traceparent(entropy: u128, sampled: bool) -> String {
    let trace_id = entropy | 1;
    let span_id = (entropy >> 64) as u64 | 1;
    let flags = if sampled { "01" } else { "00" };
    format!("00-{trace_id:032x}-{span_id:016x}-{flags}")
}

const fn samples(entropy: u128) -> bool {
    entropy % 16 < 1
}

#[cfg(test)]
#[path = "../tests/unit/telemetry/tests.rs"]
mod tests;
