use std::fmt::Write as _;
use std::sync::{Mutex, PoisonError};

use peryx_core::PrometheusSource;
use peryx_ha::{ByteAckDecision, ByteEvidence, DcAck, DurabilityPolicy, MetadataAckObservation, WriteAckDecision};
use peryx_storage::blob::BlobDurability;

// Fixed labels cap Prometheus series cardinality.
const SCOPES: [BlobDurability; 2] = [BlobDurability::Filesystem, BlobDurability::ObjectStore];
const POLICIES: [DurabilityPolicy; 4] = [
    DurabilityPolicy::Local,
    DurabilityPolicy::Majority,
    DurabilityPolicy::Everywhere,
    DurabilityPolicy::AtLeast(std::num::NonZeroUsize::MIN),
];
const DECISIONS: [WriteAckDecision; 3] = [
    WriteAckDecision::Confirmed,
    WriteAckDecision::Pending,
    WriteAckDecision::Unavailable,
];

const fn scope_index(scope: BlobDurability) -> usize {
    match scope {
        BlobDurability::Filesystem => 0,
        BlobDurability::ObjectStore => 1,
    }
}

const fn policy_index(policy: DurabilityPolicy) -> usize {
    match policy {
        DurabilityPolicy::Local => 0,
        DurabilityPolicy::Majority => 1,
        DurabilityPolicy::Everywhere => 2,
        DurabilityPolicy::AtLeast(_) => 3,
    }
}

const fn decision_index(decision: WriteAckDecision) -> usize {
    match decision {
        WriteAckDecision::Confirmed => 0,
        WriteAckDecision::Pending => 1,
        WriteAckDecision::Unavailable => 2,
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct DcDurabilityState {
    durable: [u64; SCOPES.len()],
    pending: u64,
    unknown: u64,
    quorum_acknowledged: u64,
    quorum_required: u64,
    quorum_remaining: u64,
    metadata: [[u64; DECISIONS.len()]; POLICIES.len()],
    metadata_wait_seconds: [f64; POLICIES.len()],
    metadata_timeouts: [u64; POLICIES.len()],
}

#[derive(Debug, Default)]
pub struct DcDurabilityMetrics {
    state: Mutex<DcDurabilityState>,
}

impl DcDurabilityMetrics {
    fn with<R>(&self, edit: impl FnOnce(&mut DcDurabilityState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        edit(&mut state)
    }

    pub fn record(&self, ack: DcAck) {
        self.with(|state| match ack {
            DcAck::Durable { scope } => state.durable[scope_index(scope)] += 1,
            DcAck::Pending => state.pending += 1,
            DcAck::Unknown => state.unknown += 1,
        });
    }

    pub fn record_quorum(&self, decision: &ByteAckDecision) {
        let (acknowledged, required, remaining) = match decision {
            ByteAckDecision::Acknowledged { nodes, required } => (nodes.len(), *required, 0),
            ByteAckDecision::Pending {
                nodes,
                required,
                remaining,
            } => (nodes.len(), *required, *remaining),
        };
        self.with(|state| {
            state.quorum_acknowledged = acknowledged as u64;
            state.quorum_remaining = remaining as u64;
            state.quorum_required = required as u64;
        });
    }
}

impl peryx_ha::WriteAckObserver for DcDurabilityMetrics {
    /// The quorum gauges count node receipts, which only a filesystem write earns; an object-store write
    /// leaves the last filesystem write's gauges standing rather than reporting a quorum it never ran.
    fn record(&self, outcome: DcAck, evidence: &ByteEvidence) {
        self.record(outcome);
        if let ByteEvidence::Filesystem(decision) = evidence {
            self.record_quorum(decision);
        }
    }

    fn record_metadata(&self, observation: MetadataAckObservation) {
        let policy = policy_index(observation.policy);
        self.with(|state| {
            state.metadata[policy][decision_index(observation.decision)] += 1;
            state.metadata_wait_seconds[policy] += observation.waited.as_secs_f64();
            state.metadata_timeouts[policy] += u64::from(observation.timed_out);
        });
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
        body.push_str(
            "# HELP peryx_metadata_ack_total Metadata writes by configured policy and acknowledgement decision.\n\
             # TYPE peryx_metadata_ack_total counter\n",
        );
        for (policy_index, policy) in POLICIES.into_iter().enumerate() {
            for (decision_index, decision) in DECISIONS.into_iter().enumerate() {
                let _ = writeln!(
                    body,
                    "peryx_metadata_ack_total{{policy=\"{}\",evidence=\"journal-frontier\",decision=\"{}\"}} {}",
                    policy.as_str(),
                    decision.as_str(),
                    state.metadata[policy_index][decision_index]
                );
            }
        }
        body.push_str(
            "# HELP peryx_metadata_ack_wait_seconds_total Time spent waiting for metadata acknowledgement.\n\
             # TYPE peryx_metadata_ack_wait_seconds_total counter\n\
             # HELP peryx_metadata_ack_timeout_total Metadata acknowledgement deadlines that expired.\n\
             # TYPE peryx_metadata_ack_timeout_total counter\n",
        );
        for (index, policy) in POLICIES.into_iter().enumerate() {
            let _ = writeln!(
                body,
                "peryx_metadata_ack_wait_seconds_total{{policy=\"{}\",evidence=\"journal-frontier\"}} {}",
                policy.as_str(),
                state.metadata_wait_seconds[index]
            );
            let _ = writeln!(
                body,
                "peryx_metadata_ack_timeout_total{{policy=\"{}\",evidence=\"journal-frontier\"}} {}",
                policy.as_str(),
                state.metadata_timeouts[index]
            );
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/dc_durability_tests.rs"]
mod tests;
