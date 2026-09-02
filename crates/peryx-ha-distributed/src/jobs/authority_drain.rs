use std::sync::Arc;

use async_trait::async_trait;
use peryx_driver::jobs::{JobContext, JobFailure, JobRunOutcome, LeaseScope, NodeJob, NodeJobMetadata};
use peryx_driver::serving::IntentFinalizer;
use peryx_driver::state::ServingState;
use peryx_ha::{AuthorityDrainer, RetainedWriteFinalizer};
use peryx_storage::meta::JobKind;

const AUTHORITY_DRAIN: &str = "authority_drain";

pub struct AuthorityDrainJob {
    authority: String,
    drainer: Arc<dyn AuthorityDrainer>,
    finalizers: Vec<Arc<dyn IntentFinalizer>>,
}

impl AuthorityDrainJob {
    #[must_use]
    pub fn new(
        authority: impl Into<String>,
        drainer: Arc<dyn AuthorityDrainer>,
        finalizers: Vec<Arc<dyn IntentFinalizer>>,
    ) -> Self {
        Self {
            authority: authority.into(),
            drainer,
            finalizers,
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

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: Some(&self.authority),
            persist_as: Some(JobKind::new(AUTHORITY_DRAIN).expect("static job kind is valid")),
        }
    }

    async fn run(&self, context: &JobContext) -> Result<JobRunOutcome, JobFailure> {
        let cancellation = super::CancellationProbe::new(context);
        let finalizer = InstalledFinalizers {
            state: context.state().clone(),
            finalizers: &self.finalizers,
        };
        self.drainer
            .drain(&self.authority, &finalizer, &|| cancellation.is_cancelled())
            .await
            .map(|report| cancellation.outcome(report))
            .map_err(|error| super::task_failure(&error))
    }
}

/// Offers a retained write to each installed ecosystem in turn. An intent key belongs to exactly one
/// ecosystem and the others decline it, so the first to publish is the one that owns it; a write no
/// installed ecosystem can publish stays pending.
struct InstalledFinalizers<'a> {
    state: Arc<ServingState>,
    finalizers: &'a [Arc<dyn IntentFinalizer>],
}

#[async_trait]
impl RetainedWriteFinalizer for InstalledFinalizers<'_> {
    async fn finalize_retained(&self, authority: &str, intent_key: &str) -> bool {
        for finalizer in self.finalizers {
            if finalizer
                .finalize_retained(self.state.clone(), authority, intent_key)
                .await
            {
                return true;
            }
        }
        false
    }
}
