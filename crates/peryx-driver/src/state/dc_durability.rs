//! Prometheus observability for the datacenter durability acknowledgement decision.
//!
//! A client write in `dc` or `ha` mode resolves to a [`DcAck`] outcome from the shared decision
//! ([`decide_dc_ack`](peryx_ha_distributed::decide_dc_ack)): durable for the backend scope, pending while
//! the deadline is live, or unknown once it expires. This records those outcomes and the quorum progress
//! behind the byte dimension so an operator sees per-write datacenter durability state at `/metrics`.
//!
//! The recorder is read-only over the decision: it takes an already-decided [`DcAck`] or
//! [`ByteAckDecision`] as input and folds it into bounded counters, never recomputing durability. The
//! only label is a closed `scope` vocabulary, and the quorum progress is carried as gauge values rather
//! than labels, so the exposition holds a fixed series count whatever the write volume - no project,
//! digest, or operation identity ever reaches a series.

use std::fmt::Write as _;
use std::sync::{Mutex, PoisonError};

use peryx_ha::{ByteAckDecision, DcAck};
use peryx_storage::blob::BlobDurability;

use crate::state::PrometheusSource;

/// The backend scopes a durable write is attributed to, in render order. Closed so the durable counter
/// stays at one series per scope rather than growing with the writes it counts.
const SCOPES: [BlobDurability; 2] = [BlobDurability::Filesystem, BlobDurability::ObjectStore];

/// The index into [`DcDurabilityState::durable`] for `scope`.
const fn scope_index(scope: BlobDurability) -> usize {
    match scope {
        BlobDurability::Filesystem => 0,
        BlobDurability::ObjectStore => 1,
    }
}

/// The bounded datacenter-durability counters, all `u64` so the whole state copies out from under the
/// lock for rendering.
#[derive(Debug, Default, Clone, Copy)]
struct DcDurabilityState {
    /// Writes proven datacenter-durable, indexed by [`scope_index`].
    durable: [u64; SCOPES.len()],
    /// Writes still pending durability within the client deadline.
    pending: u64,
    /// Writes whose deadline expired before durability proved.
    unknown: u64,
    /// Independent members that acknowledged the most recent filesystem byte decision.
    quorum_acknowledged: u64,
    /// Independent members the policy required for that decision.
    quorum_required: u64,
    /// Independent members still needed for that decision.
    quorum_remaining: u64,
}

/// Records datacenter durability acknowledgement outcomes and renders them as bounded Prometheus series.
///
/// Registered as a [`PrometheusSource`] on a `dc` or `ha` node, fed the [`DcAck`] a write resolves to and
/// the [`ByteAckDecision`] behind its byte dimension. Every process holds one; a single-node `none`
/// process never registers it, so its series appear only where datacenter durability is a real decision.
#[derive(Debug, Default)]
pub struct DcDurabilityMetrics {
    state: Mutex<DcDurabilityState>,
}

impl DcDurabilityMetrics {
    fn with<R>(&self, edit: impl FnOnce(&mut DcDurabilityState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        edit(&mut state)
    }

    /// Record one write's datacenter acknowledgement outcome, counting a durable result under its backend
    /// scope and a pending or unknown result under its own total.
    pub fn record(&self, ack: DcAck) {
        self.with(|state| match ack {
            DcAck::Durable { scope } => state.durable[scope_index(scope)] += 1,
            DcAck::Pending => state.pending += 1,
            DcAck::Unknown => state.unknown += 1,
        });
    }

    /// Record the quorum progress behind a filesystem byte decision: how many independent members have
    /// acknowledged, how many the policy requires, and how many remain. Numbers only, never labels, so an
    /// operator sees convergence without the series count growing.
    pub fn record_quorum(&self, decision: &ByteAckDecision) {
        let (acknowledged, remaining) = match decision {
            ByteAckDecision::Acknowledged { nodes } => (nodes.len(), 0),
            ByteAckDecision::Pending { nodes, remaining } => (nodes.len(), *remaining),
        };
        self.with(|state| {
            state.quorum_acknowledged = acknowledged as u64;
            state.quorum_remaining = remaining as u64;
            state.quorum_required = (acknowledged + remaining) as u64;
        });
    }
}

impl peryx_ha::WriteAckObserver for DcDurabilityMetrics {
    fn record(&self, outcome: DcAck, byte_decision: &ByteAckDecision) {
        self.record(outcome);
        self.record_quorum(byte_decision);
    }
}

impl PrometheusSource for DcDurabilityMetrics {
    fn write_metrics(&self, body: &mut String) {
        let state = *self.state.lock().unwrap_or_else(PoisonError::into_inner);
        body.push_str(
            "# HELP peryx_dc_ack_durable_total Client writes proven datacenter-durable, by backend scope.\n\
             # TYPE peryx_dc_ack_durable_total counter\n",
        );
        for scope in SCOPES {
            let _ = writeln!(
                body,
                "peryx_dc_ack_durable_total{{scope=\"{}\"}} {}",
                scope.as_str(),
                state.durable[scope_index(scope)]
            );
        }
        body.push_str(
            "# HELP peryx_dc_ack_pending_total Client writes still pending datacenter durability within the deadline.\n\
             # TYPE peryx_dc_ack_pending_total counter\n",
        );
        let _ = writeln!(body, "peryx_dc_ack_pending_total {}", state.pending);
        body.push_str(
            "# HELP peryx_dc_ack_unknown_total Client writes whose deadline expired before durability proved.\n\
             # TYPE peryx_dc_ack_unknown_total counter\n",
        );
        let _ = writeln!(body, "peryx_dc_ack_unknown_total {}", state.unknown);
        body.push_str(
            "# HELP peryx_dc_ack_quorum_acknowledged Independent members that acknowledged the most recent filesystem write.\n\
             # TYPE peryx_dc_ack_quorum_acknowledged gauge\n",
        );
        let _ = writeln!(body, "peryx_dc_ack_quorum_acknowledged {}", state.quorum_acknowledged);
        body.push_str(
            "# HELP peryx_dc_ack_quorum_required Independent members the policy requires for the most recent filesystem write.\n\
             # TYPE peryx_dc_ack_quorum_required gauge\n",
        );
        let _ = writeln!(body, "peryx_dc_ack_quorum_required {}", state.quorum_required);
        body.push_str(
            "# HELP peryx_dc_ack_quorum_remaining Independent members still needed for the most recent filesystem write.\n\
             # TYPE peryx_dc_ack_quorum_remaining gauge\n",
        );
        let _ = writeln!(body, "peryx_dc_ack_quorum_remaining {}", state.quorum_remaining);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The number of `peryx_dc_ack_*` series the recorder renders: one durable counter per scope, the
    /// pending and unknown counters, and the three quorum gauges. A constant of the metric shape, not the
    /// load, held by [`test_exposition_stays_within_the_series_budget`].
    const SERIES_BUDGET: usize = SCOPES.len() + 2 + 3;

    fn rendered(metrics: &DcDurabilityMetrics) -> String {
        let mut body = String::new();
        metrics.write_metrics(&mut body);
        body
    }

    #[test]
    fn test_record_counts_a_durable_write_under_its_backend_scope() {
        let metrics = DcDurabilityMetrics::default();
        metrics.record(DcAck::Durable {
            scope: BlobDurability::Filesystem,
        });
        metrics.record(DcAck::Durable {
            scope: BlobDurability::ObjectStore,
        });
        metrics.record(DcAck::Durable {
            scope: BlobDurability::ObjectStore,
        });

        let body = rendered(&metrics);
        assert!(
            body.contains("peryx_dc_ack_durable_total{scope=\"filesystem\"} 1\n"),
            "{body}"
        );
        assert!(
            body.contains("peryx_dc_ack_durable_total{scope=\"object-store\"} 2\n"),
            "{body}"
        );
    }

    #[test]
    fn test_record_counts_pending_and_unknown_outcomes_apart() {
        let metrics = DcDurabilityMetrics::default();
        metrics.record(DcAck::Pending);
        metrics.record(DcAck::Unknown);
        metrics.record(DcAck::Unknown);

        let body = rendered(&metrics);
        assert!(body.contains("peryx_dc_ack_pending_total 1\n"), "{body}");
        assert!(body.contains("peryx_dc_ack_unknown_total 2\n"), "{body}");
        assert!(
            body.contains("peryx_dc_ack_durable_total{scope=\"filesystem\"} 0\n"),
            "an unexercised scope still reports zero: {body}"
        );
    }

    #[test]
    fn test_record_quorum_reports_progress_from_a_pending_decision() {
        let metrics = DcDurabilityMetrics::default();
        metrics.record_quorum(&ByteAckDecision::Pending {
            nodes: vec!["east".to_owned(), "west".to_owned()],
            remaining: 1,
        });

        let body = rendered(&metrics);
        assert!(body.contains("peryx_dc_ack_quorum_acknowledged 2\n"), "{body}");
        assert!(body.contains("peryx_dc_ack_quorum_required 3\n"), "{body}");
        assert!(body.contains("peryx_dc_ack_quorum_remaining 1\n"), "{body}");
    }

    #[test]
    fn test_record_quorum_reports_a_complete_decision_with_nothing_remaining() {
        let metrics = DcDurabilityMetrics::default();
        metrics.record_quorum(&ByteAckDecision::Acknowledged {
            nodes: vec!["east".to_owned(), "west".to_owned(), "south".to_owned()],
        });

        let body = rendered(&metrics);
        assert!(body.contains("peryx_dc_ack_quorum_acknowledged 3\n"), "{body}");
        assert!(body.contains("peryx_dc_ack_quorum_required 3\n"), "{body}");
        assert!(body.contains("peryx_dc_ack_quorum_remaining 0\n"), "{body}");
    }

    #[test]
    fn test_exposition_stays_within_the_series_budget() {
        let metrics = DcDurabilityMetrics::default();
        metrics.record(DcAck::Durable {
            scope: BlobDurability::Filesystem,
        });
        metrics.record(DcAck::Pending);
        metrics.record(DcAck::Unknown);
        metrics.record_quorum(&ByteAckDecision::Pending {
            nodes: vec!["east".to_owned()],
            remaining: 2,
        });

        let body = rendered(&metrics);
        let series = body.lines().filter(|line| line.starts_with("peryx_dc_ack")).count();
        assert_eq!(series, SERIES_BUDGET);
        for forbidden in [
            "tenant",
            "project",
            "repository",
            "digest",
            "operation",
            "trace",
            "node",
        ] {
            assert!(
                !body.contains(forbidden),
                "a series carries a high-cardinality label: {forbidden}\n{body}"
            );
        }
    }
}
