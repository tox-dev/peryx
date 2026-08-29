use std::sync::Arc;

use async_trait::async_trait;
use peryx_driver::jobs::{JobContext, JobFailure, JobRunOutcome, LeaseScope, NodeJob, NodeJobMetadata};
use peryx_ha::AuthorityDrainer;
use peryx_storage::meta::JobKind;

const AUTHORITY_DRAIN: &str = "authority_drain";

pub struct AuthorityDrainJob {
    authority: String,
    drainer: Arc<dyn AuthorityDrainer>,
}

impl AuthorityDrainJob {
    #[must_use]
    pub fn new(authority: impl Into<String>, drainer: Arc<dyn AuthorityDrainer>) -> Self {
        Self {
            authority: authority.into(),
            drainer,
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
        self.drainer
            .drain(context.now(), &|| cancellation.is_cancelled())
            .await
            .map(|report| cancellation.outcome(report))
            .map_err(|error| super::task_failure(&error))
    }
}
