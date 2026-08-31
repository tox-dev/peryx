use std::num::NonZeroUsize;

use crate::{DrainIntent, OldEpochOp, plan_drain};
use async_trait::async_trait;
use peryx_ha::{AuthorityDrainer, AvailabilityTaskError, AvailabilityTaskReport};
use peryx_storage::meta::{IntentPhase, IntentTransition, MetaError, MetaStore};

const DRAIN_BATCH: NonZeroUsize = NonZeroUsize::new(128).expect("literal is non-zero");

/// An operator drain settles every pending intent, including the ones an ecosystem's finalize sweep has
/// given up on, so it walks the pending order without a refusal ceiling.
const DRAIN_REFUSAL_CEILING: u32 = u32::MAX;

pub struct DistributedAuthorityDrainer {
    meta: MetaStore,
    batch: NonZeroUsize,
}

impl DistributedAuthorityDrainer {
    #[must_use]
    pub const fn new(meta: MetaStore) -> Self {
        Self {
            meta,
            batch: DRAIN_BATCH,
        }
    }
}

#[async_trait]
impl AuthorityDrainer for DistributedAuthorityDrainer {
    async fn drain(
        &self,
        now: i64,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        drain_pending(&self.meta, self.batch, now, cancelled)
            .map_err(|error| AvailabilityTaskError::new("storage", error.to_string()))
    }
}

fn drain_pending(
    meta: &MetaStore,
    batch: NonZeroUsize,
    now: i64,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> Result<AvailabilityTaskReport, MetaError> {
    let mut report = AvailabilityTaskReport::default();
    loop {
        if cancelled() {
            return Ok(report);
        }
        let pending = meta.list_pending_intents(batch.get(), DRAIN_REFUSAL_CEILING)?;
        let count = pending.len();
        if count == 0 {
            return Ok(report);
        }
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
