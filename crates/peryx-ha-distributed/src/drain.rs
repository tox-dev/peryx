//! Stable key ordering makes reruns resumable. Replayable intents finalize at the new home; all other
//! terminal dispositions retire without another finalize.

use crate::reconcile::{Disposition, OldEpochOp, classify};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainIntent {
    pub key: String,
    pub op: OldEpochOp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainPlan {
    pub finalize: Vec<String>,
    pub retired: Vec<(String, Disposition)>,
}

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
