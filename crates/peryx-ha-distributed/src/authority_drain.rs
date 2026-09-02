use std::num::NonZeroUsize;

use async_trait::async_trait;
use peryx_ha::{AuthorityDrainer, AvailabilityTaskError, AvailabilityTaskReport, RetainedWriteFinalizer};
use peryx_storage::meta::{MetaError, MetaStore};

const DRAIN_BATCH: NonZeroUsize = NonZeroUsize::new(128).expect("literal is non-zero");

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
        authority: &str,
        finalizer: &dyn RetainedWriteFinalizer,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        drain_pending(&self.meta, self.batch, authority, finalizer, cancelled)
            .await
            .map_err(|error| AvailabilityTaskError::new("storage", error.to_string()))
    }
}

/// Offers every write retained for `authority` to its home, in the durable order they were admitted,
/// and counts the ones that published. A write the home cannot publish yet stays pending for a later
/// pass; the pass never advances an intent itself, so nothing settles whose effect no transaction
/// committed. Paging resumes past the last intent offered rather than re-listing from the start, which
/// is what bounds a pass whose intents all stay pending.
async fn drain_pending(
    meta: &MetaStore,
    batch: NonZeroUsize,
    authority: &str,
    finalizer: &dyn RetainedWriteFinalizer,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> Result<AvailabilityTaskReport, MetaError> {
    let mut report = AvailabilityTaskReport::default();
    let mut resume = None;
    loop {
        if cancelled() {
            return Ok(report);
        }
        let pending = meta.list_pending_intents_for(authority, resume, batch.get())?;
        let Some((_, last)) = pending.last() else {
            return Ok(report);
        };
        resume = Some(last.seq);
        let count = pending.len();
        for (key, _) in pending {
            if finalizer.finalize_retained(authority, &key).await {
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
