use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use peryx_events::metrics::{Metrics, ResourceUsage};
use peryx_pql::catalog::{Column, DomainAuth, DomainSchema, FieldClass, Indexability};
use peryx_pql::{Ast, DataSource, FetchFilter, Page, PqlError, QueryScope, RepoScope, Row, Value, ValueType, execute};
use peryx_search::{SearchAccess, SearchError, SearchParams, SearchResponse};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{MetaError, MetaStore};

use crate::retention::{
    RetentionExport, RetentionPage, RetentionPermit, RetentionPlanError, RetentionQuery, export_body, plan,
};
use crate::serving::RetentionDriver;
use crate::trash::{TrashItem, TrashPage, TrashQuery, TrashQueryError, TrashRef, TrashServices};

pub use peryx_storage::meta::{
    CreateRepositoryError, NewRepository, PolicyDecisionItem, PolicyDecisionPage, PolicyDecisionQuery,
    PolicyDecisionQueryError, PolicyInputGeneration, RepositoryFieldError, RepositoryId, RepositoryPage,
    RepositoryQuery, RepositoryQueryError, RepositoryRecord, RepositoryState, RepositoryStateError, RepositoryUpdate,
    UpdateRepositoryError, VersionPrecondition,
};

pub trait RepositoryService: Send + Sync {
    /// # Errors
    ///
    /// Returns [`CreateRepositoryError`] when validation, uniqueness, or persistence fails.
    fn create(&self, repository: NewRepository, now: i64) -> Result<RepositoryRecord, CreateRepositoryError>;

    /// # Errors
    ///
    /// Returns [`RepositoryQueryError`] for an invalid cursor or failed read.
    fn list(&self, query: &RepositoryQuery) -> Result<RepositoryPage, RepositoryQueryError>;

    /// # Errors
    ///
    /// Returns the backend error when the repository lookup fails.
    fn inspect(&self, id: &RepositoryId) -> Result<Option<RepositoryRecord>, String>;

    /// # Errors
    ///
    /// Returns [`UpdateRepositoryError`] when validation, revision checks, or persistence fails.
    fn update(
        &self,
        id: &RepositoryId,
        precondition: VersionPrecondition,
        update: RepositoryUpdate,
        actor: &peryx_identity::UserId,
        now: i64,
    ) -> Result<RepositoryRecord, UpdateRepositoryError>;

    /// # Errors
    ///
    /// Returns [`RepositoryStateError`] when the revision check or state change fails.
    fn set_enabled(
        &self,
        id: &RepositoryId,
        precondition: VersionPrecondition,
        enabled: bool,
        actor: &peryx_identity::UserId,
        now: i64,
    ) -> Result<RepositoryRecord, RepositoryStateError>;
}

pub trait PolicyDecisionService: Send + Sync {
    /// # Errors
    ///
    /// Returns [`PolicyDecisionQueryError`] for an invalid cursor or failed read.
    fn query(&self, query: &PolicyDecisionQuery) -> Result<PolicyDecisionPage, PolicyDecisionQueryError>;
}

pub trait QuotaReadService: Send + Sync {
    /// # Errors
    ///
    /// Returns the first backend error encountered while reading repository usage.
    fn summaries(
        &self,
        indexes: &[crate::Index],
        offset: usize,
        limit: usize,
    ) -> Result<Vec<crate::quota::RepositoryQuota>, String>;

    /// # Errors
    ///
    /// Returns the backend error when usage cannot be read.
    fn repository(&self, index: &crate::Index) -> Result<crate::quota::RepositoryQuota, String>;
}

#[async_trait]
pub trait RetentionPlanningService: Send + Sync {
    fn try_enter(&self, repository: &str) -> Option<RetentionPermit>;

    /// # Errors
    ///
    /// Returns [`RetentionPlanError`] when policy evaluation, metadata access, or emission fails.
    fn plan(
        &self,
        driver: &dyn RetentionDriver,
        query: &RetentionQuery<'_>,
        start: &mut dyn FnMut(peryx_policy::RetentionSummary) -> Result<(), String>,
        emit: &mut dyn FnMut(&peryx_policy::RetentionDecision) -> Result<(), String>,
    ) -> Result<RetentionPage, RetentionPlanError>;

    /// # Errors
    ///
    /// Returns [`RetentionPlanError`] when the snapshot cannot start.
    async fn export(
        &self,
        driver: Arc<dyn RetentionDriver>,
        export: RetentionExport,
        permit: RetentionPermit,
    ) -> Result<(peryx_policy::RetentionSummary, Body), RetentionPlanError>;
}

pub trait PqlQueryService: Send + Sync {
    /// # Errors
    ///
    /// Returns [`PqlError`] when validation, authorization, or data access fails.
    fn execute(&self, ast: &Ast, scope: &QueryScope, cursor: Option<&str>) -> Result<Page, PqlError>;
}

pub trait SearchQueryService: Send + Sync {
    /// # Errors
    ///
    /// Returns [`SearchError`] when the query or authorized search fails.
    fn search(&self, params: SearchParams, access: Option<&SearchAccess>) -> Result<SearchResponse, SearchError>;
}

pub trait TrashService: Send + Sync {
    /// # Errors
    ///
    /// Returns [`TrashQueryError`] for an invalid query or failed metadata read.
    fn query(&self, query: &TrashQuery) -> Result<TrashPage, TrashQueryError>;

    /// # Errors
    ///
    /// Returns [`TrashQueryError`] when the reference is invalid or metadata cannot be read.
    fn inspect(&self, reference: &TrashRef) -> Result<Option<TrashItem>, TrashQueryError>;
}

pub struct BlobStorageStatus {
    pub backend: &'static str,
    pub durability: &'static str,
    pub conditional_write: &'static str,
    pub range: &'static str,
    pub checksum: &'static str,
    pub delete: &'static str,
    pub listing: &'static str,
    pub local_staging: &'static str,
}

#[async_trait]
pub trait StatusStorageService: Send + Sync {
    /// # Errors
    ///
    /// Returns the metadata error when the serial cannot be read.
    fn current_serial(&self) -> Result<u64, MetaError>;
    async fn blobs_healthy(&self) -> bool;
    fn blob_status(&self) -> BlobStorageStatus;
}

#[derive(Clone)]
pub struct HttpDomainServices {
    repositories: Arc<dyn RepositoryService>,
    policy_decisions: Arc<dyn PolicyDecisionService>,
    quota: Arc<dyn QuotaReadService>,
    retention: Arc<dyn RetentionPlanningService>,
    pql: Arc<dyn PqlQueryService>,
    search: Arc<dyn SearchQueryService>,
    status: Arc<dyn StatusStorageService>,
    trash: Arc<dyn TrashService>,
}

impl HttpDomainServices {
    #[must_use]
    pub fn for_state(state: &Arc<crate::AppState>) -> Self {
        let meta = state.serving.meta.clone();
        Self {
            repositories: Arc::new(StoreServices::new(meta.clone())),
            policy_decisions: Arc::new(StoreServices::new(meta.clone())),
            quota: Arc::new(StoreServices::new(meta.clone())),
            retention: Arc::new(RetentionServices {
                meta: meta.clone(),
                gates: state.serving.retention_gates.clone(),
            }),
            pql: Arc::new(NeutralQuerySource::new(meta.clone(), state.serving.metrics.clone())),
            search: Arc::new(StateSearchService {
                state: Arc::clone(state),
            }),
            status: Arc::new(StorageStatusServices {
                meta,
                blobs: state.serving.blobs.clone(),
            }),
            trash: Arc::new(TrashServices::for_state(state)),
        }
    }

    #[must_use]
    pub fn with_repositories(mut self, repositories: Arc<dyn RepositoryService>) -> Self {
        self.repositories = repositories;
        self
    }

    #[must_use]
    pub fn repositories(&self) -> &dyn RepositoryService {
        self.repositories.as_ref()
    }

    #[must_use]
    pub fn policy_decisions(&self) -> &dyn PolicyDecisionService {
        self.policy_decisions.as_ref()
    }

    #[must_use]
    pub fn quota(&self) -> &dyn QuotaReadService {
        self.quota.as_ref()
    }

    #[must_use]
    pub fn retention(&self) -> &dyn RetentionPlanningService {
        self.retention.as_ref()
    }

    #[must_use]
    pub fn pql(&self) -> &dyn PqlQueryService {
        self.pql.as_ref()
    }

    #[must_use]
    pub fn search(&self) -> &dyn SearchQueryService {
        self.search.as_ref()
    }

    #[must_use]
    pub fn status(&self) -> &dyn StatusStorageService {
        self.status.as_ref()
    }

    #[must_use]
    pub fn trash(&self) -> &dyn TrashService {
        self.trash.as_ref()
    }
}

struct StateSearchService {
    state: Arc<crate::AppState>,
}

impl SearchQueryService for StateSearchService {
    fn search(&self, params: SearchParams, access: Option<&SearchAccess>) -> Result<SearchResponse, SearchError> {
        match access {
            Some(access) => self
                .state
                .serving
                .search
                .search_authorized(&self.state.search_ctx(), params, access),
            None => self.state.serving.search.search(&self.state.search_ctx(), params),
        }
    }
}

pub struct StoreServices {
    meta: MetaStore,
}

impl StoreServices {
    #[must_use]
    pub const fn new(meta: MetaStore) -> Self {
        Self { meta }
    }
}

impl RepositoryService for StoreServices {
    fn create(&self, repository: NewRepository, now: i64) -> Result<RepositoryRecord, CreateRepositoryError> {
        self.meta.create_repository(repository, now)
    }

    fn list(&self, query: &RepositoryQuery) -> Result<RepositoryPage, RepositoryQueryError> {
        self.meta.list_repositories(query)
    }

    fn inspect(&self, id: &RepositoryId) -> Result<Option<RepositoryRecord>, String> {
        self.meta.repository(id).map_err(|error| error.to_string())
    }

    fn update(
        &self,
        id: &RepositoryId,
        precondition: VersionPrecondition,
        update: RepositoryUpdate,
        actor: &peryx_identity::UserId,
        now: i64,
    ) -> Result<RepositoryRecord, UpdateRepositoryError> {
        self.meta.update_repository(id, precondition, update, actor, now)
    }

    fn set_enabled(
        &self,
        id: &RepositoryId,
        precondition: VersionPrecondition,
        enabled: bool,
        actor: &peryx_identity::UserId,
        now: i64,
    ) -> Result<RepositoryRecord, RepositoryStateError> {
        self.meta.set_repository_enabled(id, precondition, enabled, actor, now)
    }
}

impl PolicyDecisionService for StoreServices {
    fn query(&self, query: &PolicyDecisionQuery) -> Result<PolicyDecisionPage, PolicyDecisionQueryError> {
        self.meta.query_policy_decisions(query)
    }
}

impl QuotaReadService for StoreServices {
    fn summaries(
        &self,
        indexes: &[crate::Index],
        offset: usize,
        limit: usize,
    ) -> Result<Vec<crate::quota::RepositoryQuota>, String> {
        indexes
            .iter()
            .skip(offset)
            .take(limit)
            .map(|index| self.repository(index))
            .collect()
    }

    fn repository(&self, index: &crate::Index) -> Result<crate::quota::RepositoryQuota, String> {
        self.meta
            .quota_usage(&index.name)
            .map(|usage| crate::quota::repository_quota(index, &usage))
            .map_err(|error| error.to_string())
    }
}

struct RetentionServices {
    meta: MetaStore,
    gates: crate::retention::RetentionGates,
}

#[async_trait]
impl RetentionPlanningService for RetentionServices {
    fn try_enter(&self, repository: &str) -> Option<RetentionPermit> {
        self.gates.try_enter(repository)
    }

    fn plan(
        &self,
        driver: &dyn RetentionDriver,
        query: &RetentionQuery<'_>,
        start: &mut dyn FnMut(peryx_policy::RetentionSummary) -> Result<(), String>,
        emit: &mut dyn FnMut(&peryx_policy::RetentionDecision) -> Result<(), String>,
    ) -> Result<RetentionPage, RetentionPlanError> {
        plan(driver, &self.meta, query, start, emit)
    }

    async fn export(
        &self,
        driver: Arc<dyn RetentionDriver>,
        export: RetentionExport,
        permit: RetentionPermit,
    ) -> Result<(peryx_policy::RetentionSummary, Body), RetentionPlanError> {
        export_body(driver, self.meta.clone(), export, permit).await
    }
}

struct StorageStatusServices {
    meta: MetaStore,
    blobs: BlobStorage,
}

#[async_trait]
impl StatusStorageService for StorageStatusServices {
    fn current_serial(&self) -> Result<u64, MetaError> {
        self.meta.current_serial()
    }

    async fn blobs_healthy(&self) -> bool {
        self.blobs.health().await.is_ok()
    }

    fn blob_status(&self) -> BlobStorageStatus {
        let capabilities = self.blobs.capabilities();
        BlobStorageStatus {
            backend: self.blobs.name(),
            durability: capabilities.durability.as_str(),
            conditional_write: capabilities.create_if_absent.as_str(),
            range: capabilities.range.as_str(),
            checksum: capabilities.checksum.as_str(),
            delete: capabilities.delete.as_str(),
            listing: capabilities.list.as_str(),
            local_staging: capabilities.local_tail.as_str(),
        }
    }
}

const POLICY_DOMAIN: &str = "policy.decisions";
const USAGE_DOMAIN: &str = "usage.reads";
const STORE_PAGE: usize = 100;

struct NeutralQuerySource {
    meta: MetaStore,
    metrics: Metrics,
    policy: DomainSchema,
    usage: DomainSchema,
}

impl NeutralQuerySource {
    fn new(meta: MetaStore, metrics: Metrics) -> Self {
        Self {
            meta,
            metrics,
            policy: policy_schema(),
            usage: usage_schema(),
        }
    }

    fn fetch_policy(&self, scope: &QueryScope, filter: Option<&FetchFilter>) -> Result<Vec<Row>, PqlError> {
        let repository = single_repository(scope);
        let resource = indexed_resource(filter);
        let mut cursor = None;
        let mut rows = Vec::new();
        loop {
            let page = self
                .meta
                .query_policy_decisions(&PolicyDecisionQuery {
                    repository: repository.clone(),
                    resource: resource.clone(),
                    cursor: cursor.take(),
                    limit: STORE_PAGE,
                    ..PolicyDecisionQuery::default()
                })
                .map_err(|error| match error {
                    error @ PolicyDecisionQueryError::FilterTooLong { .. } => PqlError::Validation(error.to_string()),
                    error => PqlError::Backend(error.to_string()),
                })?;
            rows.extend(page.decisions.iter().map(policy_row));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(rows)
    }
}

impl PqlQueryService for NeutralQuerySource {
    fn execute(&self, ast: &Ast, scope: &QueryScope, cursor: Option<&str>) -> Result<Page, PqlError> {
        execute(ast, scope, cursor, self)
    }
}

impl DataSource for NeutralQuerySource {
    fn schema(&self, domain: &str) -> Option<&DomainSchema> {
        match domain {
            POLICY_DOMAIN => Some(&self.policy),
            USAGE_DOMAIN => Some(&self.usage),
            _ => None,
        }
    }

    fn fetch(&self, domain: &str, scope: &QueryScope, filter: Option<&FetchFilter>) -> Result<Vec<Row>, PqlError> {
        match domain {
            USAGE_DOMAIN => Ok(self
                .metrics
                .usage_totals(single_repository(scope).as_deref())
                .into_iter()
                .map(usage_row)
                .collect()),
            _ => self.fetch_policy(scope, filter),
        }
    }
}

fn usage_row(usage: ResourceUsage) -> Row {
    Row::new()
        .with("repository", Value::Str(usage.repository))
        .with("resource", Value::Str(usage.resource))
        .with("reads", Value::Int(i64::try_from(usage.reads).unwrap_or(i64::MAX)))
        .with("bytes", Value::Int(i64::try_from(usage.bytes).unwrap_or(i64::MAX)))
}

fn single_repository(scope: &QueryScope) -> Option<String> {
    match scope.repositories() {
        RepoScope::Only(set) if set.len() == 1 => set.iter().next().cloned(),
        RepoScope::All | RepoScope::Only(_) => None,
    }
}

fn indexed_resource(filter: Option<&FetchFilter>) -> Option<String> {
    let filter = filter?;
    match (filter.column, filter.values.as_slice()) {
        ("resource", [Value::Str(value)]) => Some(value.clone()),
        _ => None,
    }
}

fn policy_row(item: &PolicyDecisionItem) -> Row {
    let record = &item.record;
    Row::new()
        .with("repository", Value::Str(record.repository.clone()))
        .with("resource", Value::Str(record.resource.clone()))
        .with("group", optional(record.group.as_deref()))
        .with("artifact", optional(record.artifact.as_deref()))
        .with("source", optional(record.source.as_deref()))
        .with("action", Value::Str(record.action.to_string()))
        .with("state", Value::Str(policy_state_name(record.state).to_owned()))
        .with("rule", optional(record.rule.as_deref()))
        .with("reason", optional(record.reason.as_deref()))
        .with("evaluated_at", Value::Timestamp(record.evaluated_at_unix))
        .with("fresh", Value::Bool(item.fresh))
}

fn optional(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |text| Value::Str(text.to_owned()))
}

const fn policy_state_name(state: peryx_policy::PolicyDecisionState) -> &'static str {
    match state {
        peryx_policy::PolicyDecisionState::Allow => "allow",
        peryx_policy::PolicyDecisionState::Deny => "deny",
        peryx_policy::PolicyDecisionState::Wait => "wait",
    }
}

const POLICY_COLUMNS: &[(&str, ValueType, FieldClass, Indexability, bool)] = &[
    (
        "repository",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Indexed,
        false,
    ),
    (
        "resource",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Indexed,
        false,
    ),
    (
        "group",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Scan,
        false,
    ),
    (
        "artifact",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Scan,
        false,
    ),
    (
        "source",
        ValueType::Str,
        FieldClass::Operator,
        Indexability::Scan,
        false,
    ),
    (
        "action",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Scan,
        false,
    ),
    (
        "state",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Scan,
        false,
    ),
    ("rule", ValueType::Str, FieldClass::Operator, Indexability::Scan, false),
    (
        "reason",
        ValueType::Str,
        FieldClass::Operator,
        Indexability::Scan,
        false,
    ),
    (
        "evaluated_at",
        ValueType::Timestamp,
        FieldClass::Repository,
        Indexability::KeyOrdered,
        true,
    ),
    (
        "fresh",
        ValueType::Bool,
        FieldClass::Repository,
        Indexability::Scan,
        false,
    ),
];

const USAGE_COLUMNS: &[(&str, ValueType, FieldClass, Indexability, bool)] = &[
    (
        "repository",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Indexed,
        false,
    ),
    (
        "resource",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Indexed,
        false,
    ),
    (
        "reads",
        ValueType::Int,
        FieldClass::Repository,
        Indexability::Scan,
        true,
    ),
    ("bytes", ValueType::Int, FieldClass::Operator, Indexability::Scan, true),
];

fn columns(declarations: &[(&'static str, ValueType, FieldClass, Indexability, bool)]) -> Vec<Column> {
    declarations
        .iter()
        .map(|(name, value_type, class, indexability, numeric)| {
            Column::new(name, *value_type, *class, *indexability, *numeric)
        })
        .collect()
}

fn policy_schema() -> DomainSchema {
    DomainSchema {
        name: POLICY_DOMAIN,
        columns: columns(POLICY_COLUMNS),
        auth: DomainAuth::RepositoryOrOperator,
        natural_order: "evaluated_at",
        bounded: true,
        pushdown: &["resource"],
    }
}

fn usage_schema() -> DomainSchema {
    DomainSchema {
        name: USAGE_DOMAIN,
        columns: columns(USAGE_COLUMNS),
        auth: DomainAuth::RepositoryOrOperator,
        natural_order: "reads",
        bounded: true,
        pushdown: &[],
    }
}

#[cfg(test)]
#[path = "../tests/unit/http_services/tests.rs"]
mod tests;
