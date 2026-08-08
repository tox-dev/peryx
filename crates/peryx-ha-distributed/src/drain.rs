//! Planning the ordered drain of a failed home's retained intents after authority transfer.
//!
//! Once the control quorum commits the transfer and mints the fencing epoch, the old home's retained
//! write intents still sit staged and have to reach the new home. This is the pure plan for that drain:
//! given the retained intents and what the reconciler derived about each, [`plan_drain`] orders them by
//! their stable key and classifies each into the one terminal [`Disposition`] it reaches, so a
//! [`Replayable`](Disposition::Replayable) intent finalizes at the new home and every other disposition
//! retires without a second finalize. Ordering by key makes the plan deterministic and a re-run
//! resumable: a finalized intent leaves the pending set, so the next pass resumes at the first intent
//! still pending.
//!
//! Planning is pure. Reading the retained intents from durable metadata, advancing each finalized intent
//! into the new home's local metadata, and bounding the pass under the scheduler's authority fence are
//! the drain job's wiring.

use crate::reconcile::{Disposition, OldEpochOp, classify};

/// One retained intent the drain evaluates: its stable drain key and the facts the reconciler derived
/// about the durable operation behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainIntent {
    /// The intent's stable key, which orders the drain so it is deterministic and resumable.
    pub key: String,
    /// What the reconciler derived about the operation, classified into its terminal disposition.
    pub op: OldEpochOp,
}

/// The ordered outcome of one drain pass: which intents finalize at the new home, and which a terminal
/// disposition retires without a finalize.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainPlan {
    /// The keys to finalize into the new home's metadata, in stable order; each classified
    /// [`Replayable`](Disposition::Replayable).
    pub finalize: Vec<String>,
    /// The intents a terminal disposition settled without a finalize, in stable order, each paired with
    /// the disposition that retired it: already applied, superseded, or never durably committed.
    pub retired: Vec<(String, Disposition)>,
}

/// Plan the drain of `intents`, ordered by key so the outcome is deterministic and a re-run resumes at
/// the first still-pending intent.
///
/// Each intent reaches exactly one outcome - finalized when [`classify`] finds it
/// [`Replayable`](Disposition::Replayable), retired under the disposition it reaches otherwise - so a
/// home loss at the transfer boundary yields one outcome per operation.
#[must_use]
pub fn plan_drain(mut intents: Vec<DrainIntent>) -> DrainPlan {
    intents.sort_by(|left, right| left.key.cmp(&right.key));
    let mut plan = DrainPlan::default();
    for intent in intents {
        match classify(&intent.op) {
            Disposition::Replayable => plan.finalize.push(intent.key),
            disposition => plan.retired.push((intent.key, disposition)),
        }
    }
    plan
}
