use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{StreamExt as _, stream};
use peryx_driver::jobs::{CatalogSyncParameters, JobContext, JobFailure, JobReport, NodeJob, ScheduledJob};
use peryx_events::metrics::{CatalogSyncOutcome as MetricOutcome, Event};
use peryx_index::IndexKind;
use peryx_storage::meta::{JobKind, MetaError};
use peryx_upstream::UpstreamError;

use crate::SimpleClientExt;
use crate::cache::{ProjectSyncError, ProjectSyncOutcome, sync_project_files};
use crate::catalog::{CatalogSyncError, CatalogSyncOutcome, sync_catalog};
use crate::store::list_catalog_projects;

const CATALOG_SYNC: &str = "catalog_sync";
const MAX_PROGRESS_UPDATES: usize = 100;

pub fn catalog_job(job: &ScheduledJob) -> Option<Result<Arc<dyn NodeJob>, String>> {
    let ScheduledJob::CatalogSync(parameters) = job else {
        return None;
    };
    Some(Ok(Arc::new(CatalogSyncJob {
        parameters: parameters.clone(),
    })))
}

struct CatalogSyncJob {
    parameters: CatalogSyncParameters,
}

#[async_trait]
impl NodeJob for CatalogSyncJob {
    fn kind(&self) -> &'static str {
        CATALOG_SYNC
    }

    fn scope(&self) -> &str {
        &self.parameters.repository
    }

    fn repository(&self) -> Option<&str> {
        Some(&self.parameters.repository)
    }

    fn persist_as(&self) -> Option<JobKind> {
        Some(JobKind::CatalogSync)
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobReport, JobFailure> {
        let state = ctx.state();
        if state.read_only {
            return Err(JobFailure::new(
                "read_only",
                "catalog sync is unavailable on a read-only node",
            ));
        }
        let index = state
            .indexes
            .iter()
            .find(|index| index.name == self.parameters.repository)
            .ok_or_else(|| {
                JobFailure::new(
                    "unknown_repository",
                    format!("unknown repository {:?}", self.parameters.repository),
                )
            })?;
        if index.ecosystem != crate::ECOSYSTEM {
            return Err(JobFailure::new(
                "unsupported_repository",
                format!("repository {:?} is not a PyPI repository", index.name),
            ));
        }
        let IndexKind::Cached { client, offline: false } = &index.kind else {
            return Err(JobFailure::new(
                "unsupported_repository",
                format!("repository {:?} is not an online cached repository", index.name),
            ));
        };
        let policy = index.policy.clone();
        let repository = index.name.clone();
        let timeout = self.parameters.timeout;
        let result = match &self.parameters.source {
            Some(source) => {
                let router = state.upstream_routes.get(&repository).ok_or_else(|| {
                    JobFailure::new(
                        "unknown_source",
                        format!("repository {repository:?} has no named upstream sources"),
                    )
                })?;
                let source = router.source(source).ok_or_else(|| {
                    JobFailure::new(
                        "unknown_source",
                        format!("unknown upstream source {source:?} for repository {repository:?}"),
                    )
                })?;
                tokio::time::timeout(
                    timeout,
                    sync_projects(
                        source.client(),
                        ctx,
                        &repository,
                        &policy,
                        &self.parameters,
                        source.client().base_url(),
                    ),
                )
                .await
            }
            None => match state.upstream_routes.get(&repository) {
                Some(router) => {
                    tokio::time::timeout(
                        timeout,
                        sync_projects(router, ctx, &repository, &policy, &self.parameters, client.base_url()),
                    )
                    .await
                }
                None => {
                    tokio::time::timeout(
                        timeout,
                        sync_projects(client, ctx, &repository, &policy, &self.parameters, client.base_url()),
                    )
                    .await
                }
            },
        };
        result.map_err(|_| JobFailure::new("retryable_timeout", format!("catalog sync exceeded {timeout:?}")))?
    }
}

async fn sync_projects<C: SimpleClientExt + Sync>(
    client: &C,
    ctx: &JobContext,
    repository: &str,
    policy: &peryx_policy::Policy,
    parameters: &CatalogSyncParameters,
    fallback_source: &str,
) -> Result<JobReport, JobFailure> {
    let state = ctx.state();
    let meta = &state.meta;
    let inflight = &state.cache.inflight;
    let root = tokio::select! {
        () = ctx.cancelled() => return Ok(JobReport::default()),
        root = sync_catalog(client, inflight, meta, repository, fallback_source) => root,
    };
    let (metric_outcome, root_changed, projects) = match root {
        Ok(CatalogSyncOutcome::Published { projects }) => (MetricOutcome::Published, 1, projects),
        Ok(CatalogSyncOutcome::NotModified { projects }) => (MetricOutcome::NotModified, 0, projects),
        Err(error) => {
            ctx.state().metrics.record(Event::CatalogSync {
                route: repository.to_owned(),
                outcome: MetricOutcome::Error,
                projects: None,
            });
            return Err(catalog_error(&error));
        }
    };
    ctx.state().metrics.record(Event::CatalogSync {
        route: repository.to_owned(),
        outcome: metric_outcome,
        projects: Some(projects),
    });

    let projects = catalog_projects_or_error(list_catalog_projects(meta, repository, parameters.max_projects.get()))?;
    let total = projects.len();
    let progress_interval = total.div_ceil(MAX_PROGRESS_UPDATES).max(1);
    let mut outcomes = stream::iter(projects)
        .map(|project| async move {
            let outcome =
                sync_project_files(client, inflight, meta, repository, policy, &project, fallback_source).await;
            (project, outcome)
        })
        .buffer_unordered(parameters.concurrency.get());
    let mut report = JobReport {
        processed: 0,
        changed: root_changed,
    };
    loop {
        let next = tokio::select! {
            () = ctx.cancelled() => return Ok(report),
            next = outcomes.next() => next,
        };
        let Some((project, outcome)) = next else {
            return Ok(report);
        };
        report.processed += 1;
        match outcome {
            Ok(ProjectSyncOutcome::Published { .. }) => report.changed += 1,
            Ok(ProjectSyncOutcome::NotModified { .. } | ProjectSyncOutcome::Missing) => {}
            Err(error) => {
                let error = project_error(&error);
                return Err(JobFailure::new(
                    error.code(),
                    format!("project {project:?}: {}", error.message()),
                ));
            }
        }
        if report.processed.is_multiple_of(progress_interval as u64) || report.processed == total as u64 {
            tracing::info!(repository, processed = report.processed, total, "catalog sync progress");
        }
    }
}

fn catalog_error(error: &CatalogSyncError) -> JobFailure {
    match error {
        CatalogSyncError::Upstream(error) => upstream_error(error),
        CatalogSyncError::Status(status) => status_error(*status),
        _ => JobFailure::new("catalog_sync", error.to_string()),
    }
}

fn project_error(error: &ProjectSyncError) -> JobFailure {
    match error {
        ProjectSyncError::Upstream(error) => upstream_error(error),
        ProjectSyncError::Status(status) => status_error(*status),
        _ => JobFailure::new("project_sync", error.to_string()),
    }
}

fn upstream_error(error: &UpstreamError) -> JobFailure {
    let category = if matches!(error.status(), Some(429 | 500..=u16::MAX))
        || matches!(error, UpstreamError::Http(error) if error.is_timeout() || error.is_connect())
    {
        "retryable_upstream"
    } else {
        "upstream"
    };
    JobFailure::new(category, error.user_message())
}

fn status_error(status: u16) -> JobFailure {
    let category = if status == 429 || status >= 500 {
        "retryable_upstream"
    } else {
        "upstream"
    };
    JobFailure::new(category, format!("upstream returned HTTP {status}"))
}

fn catalog_projects_or_error(projects: Result<Vec<String>, MetaError>) -> Result<Vec<String>, JobFailure> {
    match projects {
        Ok(projects) => Ok(projects),
        Err(error) => Err(JobFailure::new("storage", error.to_string())),
    }
}

#[cfg(test)]
#[path = "../tests/unit/catalog_job/tests.rs"]
mod tests;
