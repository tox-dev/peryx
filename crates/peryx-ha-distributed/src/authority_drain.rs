//! The authority-drain job: finalize a failed home's retained intents at the new home.
//!
//! When a home fails and the control quorum transfers its authority, the ingress DCs still hold the
//! writes the old home never finalized as retained intents. This job drains them at the new home: it
//! reads the still-pending intents in stable key order, plans the pass through the pure
//! [`plan_drain`](peryx_ha_distributed::plan_drain), and finalizes each replayable intent into local
//! metadata by advancing it to [`Admitted`](peryx_storage::meta::IntentPhase::Admitted).
//!
//! The pass is bounded, ordered, and resumable. Each finalize is idempotent - advancing an intent only
//! moves it forward - so a re-run after an interruption resumes at the first intent still pending rather
//! than re-finalizing settled ones. The job names its authority as its repository and persists a durable
//! run, so the scheduler leases the authority's epoch as the run's fence: a drain whose authority
//! transfers again mid-run wrote under a superseded epoch and its success is fenced out.
//!
//! Finalizing into the new home's local metadata is the acceptance the in-process drain proves.
//! Publishing a finalized intent onward to a remote home has no producer surface yet and is deferred.

use std::num::NonZeroUsize;

use crate::{DrainIntent, OldEpochOp, plan_drain};
use async_trait::async_trait;
use peryx_driver::jobs::{JobContext, JobFailure, JobReport, NodeJob};
use peryx_storage::meta::{IntentPhase, IntentTransition, JobKind, MetaError, MetaStore};

const AUTHORITY_DRAIN: &str = "authority_drain";

/// How many retained intents one drain pass finalizes before it reads the next batch, so a large backlog
/// drains in bounded transactions rather than one unbounded scan.
const DRAIN_BATCH: NonZeroUsize = NonZeroUsize::new(128).expect("literal is non-zero");

/// Finalize the ingress intents an authority's former home left retained, into the new home's local
/// metadata, in stable key order and one bounded batch per pass.
pub struct AuthorityDrainJob {
    authority: String,
    batch: NonZeroUsize,
}

impl AuthorityDrainJob {
    /// Drain the intents retained for `authority`, finalizing at most [`DRAIN_BATCH`] per pass.
    #[must_use]
    pub fn new(authority: impl Into<String>) -> Self {
        Self {
            authority: authority.into(),
            batch: DRAIN_BATCH,
        }
    }
}

#[async_trait]
impl NodeJob for AuthorityDrainJob {
    fn kind(&self) -> &'static str {
        AUTHORITY_DRAIN
    }

    fn scope(&self) -> &str {
        &self.authority
    }

    fn repository(&self) -> Option<&str> {
        Some(&self.authority)
    }

    fn persist_as(&self) -> Option<JobKind> {
        Some(JobKind::AuthorityDrain)
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        drain_pending(&ctx.state().meta, self.batch, (ctx.state().clock)(), || {
            ctx.is_cancelled()
        })
        .map_err(|error| JobFailure::new("storage", error.to_string()))
    }
}

/// Drain the retained pending intents into local metadata in bounded batches, stopping between batches
/// when `cancelled` reports shutdown.
///
/// Each batch reads at most `batch` pending intents in key order, plans the pass through the pure
/// [`plan_drain`], and finalizes each replayable intent to [`Admitted`](IntentPhase::Admitted) with
/// timestamp `now`. A finalize that finds the intent already advanced counts as processed but not
/// changed, so a re-run resumes without double-counting.
fn drain_pending(
    meta: &MetaStore,
    batch: NonZeroUsize,
    now: i64,
    cancelled: impl Fn() -> bool,
) -> Result<JobReport, MetaError> {
    let mut report = JobReport::default();
    loop {
        if cancelled() {
            return Ok(report);
        }
        let pending = meta.list_pending_intents(batch.get())?;
        let count = pending.len();
        if count == 0 {
            return Ok(report);
        }
        // A durably staged, unfinalized intent is replayable; the pure plan orders the batch and
        // classifies each into the one outcome it reaches.
        let plan = plan_drain(
            pending
                .into_iter()
                .map(|(key, _)| DrainIntent {
                    key,
                    op: OldEpochOp {
                        durably_committed: true,
                        already_applied: false,
                        superseded: false,
                    },
                })
                .collect(),
        );
        for key in plan.finalize {
            if meta.advance_intent(&key, IntentPhase::Admitted, now)? == IntentTransition::Advanced {
                report.changed += 1;
            }
        }
        report.processed += count as u64;
        if count < batch.get() {
            return Ok(report);
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/authority_drain_tests.rs"]
mod drain_tests;
