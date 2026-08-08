//! Structured, redacted telemetry for one replication operation.
//!
//! An [`OperationEnvelope`] carries who authored an operation, under which authority epoch, at what
//! serial, its kind, and the W3C trace it belongs to. This turns that into the log-safe fields a span or
//! event records: the operation identity, the kind, and the traceparent, and nothing from the change
//! payload. A credential, artifact bytes, or a private path travels in the change, never in these fields,
//! so it cannot reach a log through this surface.
//!
//! Emitting is gated on the trace's own sampled flag, so an operation a producer chose not to trace adds
//! no event volume here either. The apply side joins the author's trace through
//! [`derive_child`](crate::derive_child); this records the correlated identity a joined span carries.

pub use peryx_ha::{OperationTelemetry, sampled};

use crate::envelope::OperationEnvelope;

/// Build log-safe telemetry from a replicated operation.
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
