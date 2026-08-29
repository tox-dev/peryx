use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{StreamExt as _, stream};
use peryx_core::Role;
use std::num::NonZeroUsize;
use std::time::Duration;

use peryx_driver::jobs::{
    JobContext, JobFailure, JobReport, JobRunOutcome, LeaseScope, NodeJob, NodeJobMetadata, PluginScheduledJob,
    ScheduledJobFactory,
};
use peryx_driver::serving::JobConfig;
use peryx_events::metrics::{MetricFamily, MetricKind, Metrics};
use peryx_index::IndexKind;
use peryx_storage::meta::{JobKind, MetaError};
use peryx_upstream::UpstreamError;

use crate::SimpleClientExt;
use crate::cache::{ProjectSyncError, ProjectSyncOutcome, sync_project_files};
use crate::catalog::{CatalogSyncError, CatalogSyncOutcome, sync_catalog};
use crate::store::list_catalog_projects;

const CATALOG_SYNC: &str = "catalog_sync";
const MAX_PROGRESS_UPDATES: usize = 100;
pub const DEFAULT_CATALOG_PROJECTS: usize = 10_000;
pub const DEFAULT_CATALOG_CONCURRENCY: usize = 4;
pub const DEFAULT_CATALOG_TIMEOUT: Duration = Duration::from_mins(15);
pub const MAX_CATALOG_PROJECTS_PER_RUN: usize = 100_000;
pub const MAX_CATALOG_CONCURRENCY: usize = 32;
pub const MAX_CATALOG_TIMEOUT: Duration = Duration::from_hours(24);

pub static OPERATOR_JOB: CatalogOperatorJob = CatalogOperatorJob;

pub struct CatalogOperatorJob;

impl peryx_plugin_registry::OperatorJob for CatalogOperatorJob {
    fn command(&self) -> &'static str {
        "run"
    }

    fn defaults(&self) -> peryx_plugin_registry::OperatorJobDefaults {
        peryx_plugin_registry::OperatorJobDefaults {
            item_limit: DEFAULT_CATALOG_PROJECTS,
            concurrency: DEFAULT_CATALOG_CONCURRENCY,
            timeout_secs: DEFAULT_CATALOG_TIMEOUT.as_secs(),
        }
    }

    fn compile(&self, options: peryx_plugin_registry::OperatorJobOptions<'_>) -> Result<PluginScheduledJob, String> {
        scheduled_from_options(
            options.target,
            options.source,
            options.item_limit,
            options.concurrency,
            options.timeout_secs,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSyncParameters {
    pub repository: String,
    pub source: Option<String>,
    pub max_projects: NonZeroUsize,
    pub concurrency: NonZeroUsize,
    pub timeout: Duration,
}

impl CatalogSyncParameters {
    #[must_use]
    pub fn new(repository: impl Into<String>) -> Self {
        Self {
            repository: repository.into(),
            source: None,
            max_projects: NonZeroUsize::new(DEFAULT_CATALOG_PROJECTS).expect("default is positive"),
            concurrency: NonZeroUsize::new(DEFAULT_CATALOG_CONCURRENCY).expect("default is positive"),
            timeout: DEFAULT_CATALOG_TIMEOUT,
        }
    }
}

pub const CATALOG_METRIC_FAMILIES: &[MetricFamily] = &[
    CATALOG_SYNCS,
    CATALOG_PUBLISHED,
    CATALOG_NOT_MODIFIED,
    CATALOG_ERRORS,
    CATALOG_PROJECTS,
];
const CATALOG_SYNCS: MetricFamily = MetricFamily {
    key: "pypi.catalog.syncs",
    prom_name: "peryx_catalog_syncs_total",
    help: "Remote root-catalog synchronizations.",
    ui_label: "Catalog synchronizations",
    roles: &[Role::Cached],
    json_name: Some("catalog_syncs"),
    kind: MetricKind::Counter,
};
const CATALOG_PUBLISHED: MetricFamily = MetricFamily {
    key: "pypi.catalog.published",
    prom_name: "peryx_catalog_published_total",
    help: "Remote root-catalog generations published.",
    ui_label: "Catalog publications",
    roles: &[Role::Cached],
    json_name: Some("catalog_published"),
    kind: MetricKind::Counter,
};
const CATALOG_NOT_MODIFIED: MetricFamily = MetricFamily {
    key: "pypi.catalog.not_modified",
    prom_name: "peryx_catalog_not_modified_total",
    help: "Remote root-catalog revalidations answered not modified.",
    ui_label: "Catalog revalidations",
    roles: &[Role::Cached],
    json_name: Some("catalog_not_modified"),
    kind: MetricKind::Counter,
};
const CATALOG_ERRORS: MetricFamily = MetricFamily {
    key: "pypi.catalog.errors",
    prom_name: "peryx_catalog_errors_total",
    help: "Failed remote root-catalog synchronizations.",
    ui_label: "Catalog errors",
    roles: &[Role::Cached],
    json_name: Some("catalog_errors"),
    kind: MetricKind::Counter,
};
const CATALOG_PROJECTS: MetricFamily = MetricFamily {
    key: "pypi.catalog.projects",
    prom_name: "peryx_catalog_projects",
    help: "Projects in the current remote root catalog.",
    ui_label: "Catalog projects",
    roles: &[Role::Cached],
    json_name: Some("catalog_projects"),
    kind: MetricKind::Gauge,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogMetricOutcome {
    Published { projects: u64 },
    NotModified { projects: u64 },
    Error,
}

pub fn record_catalog_metrics(metrics: &Metrics, route: &str, outcome: CatalogMetricOutcome) {
    metrics.increment(route, &CATALOG_SYNCS, 1);
    match outcome {
        CatalogMetricOutcome::Published { projects } => {
            metrics.increment(route, &CATALOG_PUBLISHED, 1);
            metrics.set(route, &CATALOG_PROJECTS, projects);
        }
        CatalogMetricOutcome::NotModified { projects } => {
            metrics.increment(route, &CATALOG_NOT_MODIFIED, 1);
            metrics.set(route, &CATALOG_PROJECTS, projects);
        }
        CatalogMetricOutcome::Error => metrics.increment(route, &CATALOG_ERRORS, 1),
    }
}

pub fn compile(config: JobConfig<'_>) -> Option<Result<PluginScheduledJob, String>> {
    if config.kind != CATALOG_SYNC {
        return None;
    }
    Some(
        compile_parameters(config.settings, config.indexes)
            .map(|parameters| PluginScheduledJob::new(crate::ECOSYSTEM, Arc::new(CatalogSyncFactory { parameters }))),
    )
}

#[must_use]
pub fn scheduled(parameters: CatalogSyncParameters) -> PluginScheduledJob {
    PluginScheduledJob::new(crate::ECOSYSTEM, Arc::new(CatalogSyncFactory { parameters }))
}

/// # Errors
/// Returns the same validation errors as a configured catalog schedule.
pub fn scheduled_from_options(
    repository: &str,
    source: Option<&str>,
    max_projects: usize,
    concurrency: usize,
    timeout_secs: u64,
) -> Result<PluginScheduledJob, String> {
    if repository.trim().is_empty() {
        return Err("repository must not be empty".to_owned());
    }
    if source.is_some_and(|source| source.trim().is_empty()) {
        return Err("source must not be empty".to_owned());
    }
    if max_projects > MAX_CATALOG_PROJECTS_PER_RUN {
        return Err("max-projects exceeds the per-run limit".to_owned());
    }
    if concurrency > MAX_CATALOG_CONCURRENCY {
        return Err("concurrency exceeds the per-run limit".to_owned());
    }
    if timeout_secs > MAX_CATALOG_TIMEOUT.as_secs() {
        return Err("timeout-secs exceeds the per-run limit".to_owned());
    }
    let max_projects = NonZeroUsize::new(max_projects).ok_or_else(|| "max-projects must be positive".to_owned())?;
    let concurrency = NonZeroUsize::new(concurrency).ok_or_else(|| "concurrency must be positive".to_owned())?;
    if timeout_secs == 0 {
        return Err("timeout-secs must be positive".to_owned());
    }
    Ok(scheduled(CatalogSyncParameters {
        repository: repository.to_owned(),
        source: source.map(str::to_owned),
        max_projects,
        concurrency,
        timeout: Duration::from_secs(timeout_secs),
    }))
}

#[must_use]
pub const fn default_project_limit() -> usize {
    DEFAULT_CATALOG_PROJECTS
}

#[must_use]
pub const fn default_concurrency() -> usize {
    DEFAULT_CATALOG_CONCURRENCY
}

#[must_use]
pub const fn default_timeout_secs() -> u64 {
    DEFAULT_CATALOG_TIMEOUT.as_secs()
}

#[derive(Debug)]
struct CatalogSyncFactory {
    parameters: CatalogSyncParameters,
}

impl ScheduledJobFactory for CatalogSyncFactory {
    fn kind(&self) -> &'static str {
        CATALOG_SYNC
    }

    fn settings(&self) -> toml::Table {
        let mut settings = toml::Table::new();
        settings.insert(
            "repository".to_owned(),
            toml::Value::String(self.parameters.repository.clone()),
        );
        if let Some(source) = &self.parameters.source {
            settings.insert("source".to_owned(), toml::Value::String(source.clone()));
        }
        settings.insert(
            "max_projects".to_owned(),
            toml::Value::Integer(
                i64::try_from(self.parameters.max_projects.get()).expect("validated limit fits a TOML integer"),
            ),
        );
        settings.insert(
            "concurrency".to_owned(),
            toml::Value::Integer(
                i64::try_from(self.parameters.concurrency.get()).expect("validated limit fits a TOML integer"),
            ),
        );
        settings.insert(
            "timeout_secs".to_owned(),
            toml::Value::Integer(
                i64::try_from(self.parameters.timeout.as_secs()).expect("validated timeout fits a TOML integer"),
            ),
        );
        settings
    }

    fn create(&self, _app: &peryx_driver::AppState) -> Result<Arc<dyn NodeJob>, String> {
        Ok(Arc::new(CatalogSyncJob {
            parameters: self.parameters.clone(),
        }))
    }
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

    fn metadata(&self) -> NodeJobMetadata<'_> {
        NodeJobMetadata {
            lease_scope: LeaseScope::NodeLocal,
            repository: Some(&self.parameters.repository),
            persist_as: Some(JobKind::new(CATALOG_SYNC).expect("static job kind is valid")),
        }
    }

    async fn run(&self, ctx: &JobContext) -> Result<JobRunOutcome, JobFailure> {
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

fn compile_parameters(
    settings: &toml::Table,
    indexes: &[peryx_driver::serving::JobIndexConfig<'_>],
) -> Result<CatalogSyncParameters, String> {
    const FIELDS: &[&str] = &["repository", "source", "max_projects", "concurrency", "timeout_secs"];
    if let Some(field) = settings.keys().find(|field| !FIELDS.contains(&field.as_str())) {
        return Err(format!("unknown field `{field}`"));
    }
    let repository = settings
        .get("repository")
        .and_then(toml::Value::as_str)
        .filter(|repository| !repository.trim().is_empty())
        .ok_or_else(|| "catalog sync needs a non-empty `repository`".to_owned())?;
    let source = settings
        .get("source")
        .map(|value| {
            value
                .as_str()
                .filter(|source| !source.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| "catalog sync `source` must not be empty".to_owned())
        })
        .transpose()?;
    let mut parameters = CatalogSyncParameters::new(repository);
    parameters.source = source;
    parameters.max_projects = positive_setting(
        settings,
        "max_projects",
        DEFAULT_CATALOG_PROJECTS,
        MAX_CATALOG_PROJECTS_PER_RUN,
    )?;
    parameters.concurrency = positive_setting(
        settings,
        "concurrency",
        DEFAULT_CATALOG_CONCURRENCY,
        MAX_CATALOG_CONCURRENCY,
    )?;
    let timeout_secs = unsigned_setting(settings, "timeout_secs")?.unwrap_or(DEFAULT_CATALOG_TIMEOUT.as_secs());
    parameters.timeout = Duration::from_secs(timeout_secs);
    if parameters.timeout.is_zero() || parameters.timeout > MAX_CATALOG_TIMEOUT {
        return Err("catalog sync `timeout_secs` must be between 1 and 86400".to_owned());
    }
    let configured = indexes
        .iter()
        .find(|index| index.name == parameters.repository)
        .ok_or_else(|| "catalog sync `repository` must name a configured index".to_owned())?;
    if !configured.cached {
        return Err("catalog sync `repository` must name a cached index".to_owned());
    }
    if configured.ecosystem != crate::ECOSYSTEM || configured.offline {
        return Err("catalog sync needs an online repository with catalog support".to_owned());
    }
    if let Some(source) = &parameters.source
        && !configured.upstreams.contains(&source.as_str())
    {
        return Err("catalog sync `source` must name a repository upstream".to_owned());
    }
    Ok(parameters)
}

fn positive_setting(
    settings: &toml::Table,
    field: &'static str,
    default: usize,
    maximum: usize,
) -> Result<NonZeroUsize, String> {
    let value = unsigned_setting(settings, field)?.unwrap_or_else(|| u64::try_from(default).expect("default fits u64"));
    if value > u64::try_from(maximum).expect("per-run limit fits u64") {
        return Err(format!("catalog sync `{field}` exceeds the per-run limit"));
    }
    NonZeroUsize::new(usize::try_from(value).expect("validated per-run limit fits usize"))
        .ok_or_else(|| format!("catalog sync `{field}` must be positive"))
}

fn unsigned_setting(settings: &toml::Table, field: &str) -> Result<Option<u64>, String> {
    settings
        .get(field)
        .map(|value| {
            value
                .as_integer()
                .and_then(|value| u64::try_from(value).ok())
                .ok_or_else(|| format!("`{field}` must be a non-negative integer"))
        })
        .transpose()
}

async fn sync_projects<C: SimpleClientExt + Sync>(
    client: &C,
    ctx: &JobContext,
    repository: &str,
    policy: &peryx_policy::Policy,
    parameters: &CatalogSyncParameters,
    fallback_source: &str,
) -> Result<JobRunOutcome, JobFailure> {
    let state = ctx.state();
    let meta = &state.meta;
    let inflight = &state.cache.inflight;
    let root = tokio::select! {
        () = ctx.cancelled() => return Ok(JobRunOutcome::cancelled(JobReport::default())),
        root = sync_catalog(client, inflight, meta, repository, fallback_source) => root,
    };
    let (metric_outcome, root_changed) = match root {
        Ok(CatalogSyncOutcome::Published { projects }) => (CatalogMetricOutcome::Published { projects }, 1),
        Ok(CatalogSyncOutcome::NotModified { projects }) => (CatalogMetricOutcome::NotModified { projects }, 0),
        Err(error) => {
            record_catalog_metrics(&ctx.state().metrics, repository, CatalogMetricOutcome::Error);
            return Err(catalog_error(&error));
        }
    };
    record_catalog_metrics(&ctx.state().metrics, repository, metric_outcome);

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
        ..JobReport::default()
    };
    loop {
        let next = tokio::select! {
            () = ctx.cancelled() => return Ok(JobRunOutcome::cancelled(report)),
            next = outcomes.next() => next,
        };
        let Some((project, outcome)) = next else {
            return Ok(JobRunOutcome::succeeded(report));
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
