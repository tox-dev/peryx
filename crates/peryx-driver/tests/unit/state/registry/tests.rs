use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use peryx_core::{Ecosystem, Lexicon};
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_search::{
    ContentSource, IndexerCtx, SearchDocument, SearchDocumentProvider, SearchError, SearchParams, default_indexer,
};
use peryx_storage::blob::{BlobDurability, BlobStore};
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use super::AppState;
use crate::rate_limit::RouteClass;
use crate::serving::{
    AbsoluteProtocolDriver, CacheRefresher, ClientDiscovery, EcosystemDriver, EcosystemRegistration, IdleReclaimer,
    IndexCredentialDriver, IndexSummary, IndexSummaryDriver, IndexSummaryError, IndexedProtocolDriver, IntentFinalizer,
    MirrorAction, MirrorDriver, MirrorRequest, ProtocolDriver, RateLimitPrincipal, RefreshSweep, ReplicatedApplyDriver,
};
use crate::state::{HttpRoutes, ServingState, ViewBlock};
use tower::ServiceExt;

struct Driver;

struct BareDriver;

struct IndexedDriver;

struct ReplacementDriver;

struct Drainer;

struct MutableDocs(Arc<Mutex<String>>);

impl SearchDocumentProvider for MutableDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        let text = self.0.lock().unwrap().clone();
        Ok(vec![SearchDocument {
            display_label: "package".to_owned(),
            resource_key: "package".to_owned(),
            route: "root".to_owned(),
            index: "root".to_owned(),
            ecosystem: "indexed".to_owned(),
            source: ContentSource::Cached,
            available_locally: false,
            summary: None,
            text,
        }])
    }
}

#[async_trait]
impl peryx_ha::AuthorityDrainer for Drainer {
    async fn drain(
        &self,
        _now: i64,
        _cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<peryx_ha::AvailabilityTaskReport, peryx_ha::AvailabilityTaskError> {
        Ok(peryx_ha::AvailabilityTaskReport {
            processed: 0,
            changed: 0,
        })
    }
}

impl EcosystemDriver for BareDriver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("bare")
    }
}

impl EcosystemDriver for IndexedDriver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("indexed")
    }
}

impl EcosystemDriver for ReplacementDriver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }
}

#[async_trait]
impl AbsoluteProtocolDriver for ReplacementDriver {
    fn prefixes(&self) -> &'static [&'static str] {
        &["/resources"]
    }

    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    async fn serve(&self, _state: Arc<ServingState>, _request: Request) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

#[async_trait]
impl IndexedProtocolDriver for IndexedDriver {
    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    async fn get(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        _rest: String,
        _uri: Uri,
        _headers: HeaderMap,
        _method: Method,
    ) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }

    async fn post(&self, _state: Arc<ServingState>, _path: String, _request: Request) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }

    async fn put(&self, _state: Arc<ServingState>, _request: Request) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }

    async fn delete(&self, _state: Arc<ServingState>, _uri: Uri, _headers: HeaderMap) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

impl IndexSummaryDriver for Driver {
    fn summarize_indexes(
        &self,
        _meta: &MetaStore,
        index_names: &[String],
        _recent_limit: usize,
    ) -> Result<std::collections::HashMap<String, IndexSummary>, IndexSummaryError> {
        if index_names == ["failure"] {
            return Err(IndexSummaryError::Storage);
        }
        Ok(index_names
            .iter()
            .filter(|name| name.as_str() != "omitted")
            .map(|name| (name.clone(), IndexSummary::default()))
            .collect())
    }
}

impl IndexCredentialDriver for Driver {
    fn recognizes(&self, authorization: &str) -> bool {
        authorization == "accepted"
    }

    fn authorize(
        &self,
        _index: &Index,
        authorization: Option<&str>,
        action: peryx_identity::Action,
        _now: i64,
    ) -> Result<(), peryx_identity::Denial> {
        (authorization == Some("accepted") && action == peryx_identity::Action::Read)
            .then_some(())
            .ok_or(peryx_identity::Denial::Forbidden)
    }
}

impl EcosystemDriver for Driver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }
}

#[async_trait]
impl AbsoluteProtocolDriver for Driver {
    fn prefixes(&self) -> &'static [&'static str] {
        &["/artifacts"]
    }

    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    async fn serve(&self, _state: Arc<ServingState>, _request: Request) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

impl ReplicatedApplyDriver for Driver {
    fn apply_replicated_changes(
        &self,
        _state: &crate::ServingState,
        _changed_keys: &[String],
    ) -> Result<(), ViewBlock> {
        Ok(())
    }
}

#[async_trait]
impl MirrorDriver for Driver {
    fn validate_options(&self, _configured: &toml::Table, _overrides: &toml::Table) -> Result<(), String> {
        Ok(())
    }

    async fn mirror(
        &self,
        _state: Arc<AppState>,
        _request: MirrorRequest<'_>,
        output: &mut (dyn std::io::Write + Send),
    ) -> Result<(), String> {
        output.write_all(b"mirror").map_err(|error| error.to_string())
    }
}

struct PrincipalCapability;

impl RateLimitPrincipal for PrincipalCapability {
    fn resolve(
        &self,
        _state: &ServingState,
        _position: Option<usize>,
        _headers: &HeaderMap,
    ) -> peryx_identity::Principal {
        peryx_identity::Principal::Anonymous
    }
}

struct DiscoveryCapability;

impl ClientDiscovery for DiscoveryCapability {
    fn discover_index(
        &self,
        _index: crate::state::IndexDescription,
        _base: Option<&crate::discovery::BaseUrl>,
    ) -> serde_json::Value {
        serde_json::Value::Null
    }

    fn client_endpoint(&self, route: &str) -> String {
        format!("/client/{route}")
    }
}

static PRINCIPAL_CAPABILITY: PrincipalCapability = PrincipalCapability;
static DISCOVERY_CAPABILITY: DiscoveryCapability = DiscoveryCapability;

struct Durability;

#[async_trait]
impl peryx_ha::BlobWriteDurability for Durability {
    async fn confirm(&self, _write: peryx_ha::CommittedBlob<'_>) -> peryx_ha::WriteDurability {
        peryx_ha::WriteDurability::Unavailable
    }
}

struct Completeness;

struct Routes;

struct RuntimeCapability;

struct Registration;

impl HttpRoutes for Routes {
    fn routes(&self) -> Router<Arc<AppState>> {
        Router::new()
    }
}

#[async_trait]
impl IdleReclaimer for RuntimeCapability {
    async fn reclaim_idle(&self, _state: Arc<ServingState>) -> usize {
        1
    }
}

#[async_trait]
impl IntentFinalizer for RuntimeCapability {
    async fn finalize_admitted(&self, _state: Arc<ServingState>) -> u64 {
        2
    }
}

#[async_trait]
impl CacheRefresher for RuntimeCapability {
    async fn refresh_stale(&self, _state: Arc<ServingState>) -> Result<RefreshSweep, String> {
        Ok(RefreshSweep { checked: 3, changed: 1 })
    }
}

impl EcosystemRegistration for Registration {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }

    fn default_indexes(&self) -> &'static [peryx_core::DefaultIndex] {
        &[]
    }

    fn absolute_prefixes(&self) -> &'static [&'static str] {
        &["/artifacts"]
    }

    fn webhook_events(&self) -> &'static [&'static str] {
        &[]
    }

    fn driver(&self) -> ProtocolDriver {
        ProtocolDriver::Absolute(Arc::new(Driver))
    }

    fn register_capabilities(&self, _: &mut dyn crate::serving::CapabilityRegistrar) {}
}

impl peryx_ha::AnalyticsCompleteness for Completeness {
    fn assess(
        &self,
        _meta: &dyn peryx_ha::AnalyticsSnapshotStore,
        _expected: &[peryx_ha::ExpectedProducer],
        _query: &peryx_ha::CompletenessQuery,
    ) -> Result<peryx_ha::CompletenessReport, peryx_ha::CompletenessError> {
        Err(peryx_ha::CompletenessError)
    }
}

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, AppState::new(meta, blobs, 60, Vec::new()))
}

fn register_driver_capabilities(state: &mut AppState) {
    state.register_capabilities(|registrar| {
        registrar.register_index_summary(Ecosystem::new("example"), Arc::new(Driver));
        registrar.register_index_credentials(Ecosystem::new("example"), Arc::new(Driver));
    });
}

#[test]
fn test_registry_installs_neutral_driver_capabilities() {
    let (_dir, mut state) = state();
    let driver = Arc::new(Driver);
    let ecosystem = Ecosystem::new("example");

    state.register_lexicon(ecosystem.clone(), &Lexicon::NEUTRAL);
    register_driver_capabilities(&mut state);
    state
        .register_protocol(ProtocolDriver::Absolute(driver.clone()), default_indexer())
        .unwrap();
    state.register_replicated_apply_driver(ecosystem.clone(), driver.clone());
    state.register_mirror_driver(ecosystem.clone(), driver);
    state.register_http_routes(Arc::new(Routes));
    let index = Index {
        name: "catalog".to_owned(),
        route: "catalog".to_owned(),
        ecosystem: ecosystem.clone(),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    };

    assert!(state.has_any_driver());
    assert!(state.driver_for(&ecosystem).is_some());
    assert!(state.driver_for_name("example").is_some());
    assert_eq!(state.drivers().count(), 1);
    assert_eq!(state.idle_reclaimers().count(), 0);
    assert_eq!(state.intent_finalizers().count(), 0);
    assert_eq!(state.cache_refreshers().count(), 0);
    assert_eq!(state.replicated_apply_drivers().count(), 1);
    assert_eq!(
        state
            .mirror_driver_for(&ecosystem)
            .unwrap()
            .validate_options(&toml::Table::new(), &toml::Table::new()),
        Ok(())
    );
    assert_eq!(
        state
            .absolute_driver_for_path("/artifacts/item")
            .unwrap()
            .classify_route("/artifacts/item"),
        RouteClass::Artifact
    );
    assert!(state.absolute_driver_for_path("/artifacts").is_some());
    assert!(state.absolute_driver_for_path("/artifacts/item").is_some());
    assert!(state.absolute_driver_for_path("/artifacts-evil").is_none());
    assert_eq!(
        state.absolute_mounts().map(|(prefix, _)| prefix).collect::<Vec<_>>(),
        vec!["/artifacts"]
    );
    let indexer = state.serving.indexer_ctx();
    assert!(std::ptr::eq(indexer.meta, &raw const state.serving.meta));
    let search = state.search_ctx();
    assert_eq!(search.lexicon(&ecosystem).repository, "repository");
    assert_eq!(state.http_routes().count(), 1);
    assert!(state.recognizes_index_credential("accepted"));
    assert!(!state.recognizes_index_credential("rejected"));
    assert_eq!(
        state.authorize_index_credential(&index, Some("accepted"), peryx_identity::Action::Read),
        Ok(())
    );
    assert_eq!(
        state.authorize_index_credential(&index, None, peryx_identity::Action::Read),
        Err(peryx_identity::Denial::Forbidden)
    );
}

#[tokio::test]
async fn test_registered_capabilities_delegate_behavior() {
    let (_dir, mut state) = state();
    let driver = Arc::new(Driver);
    let ecosystem = Ecosystem::new("example");

    register_driver_capabilities(&mut state);
    state
        .register_protocol(ProtocolDriver::Absolute(driver.clone()), default_indexer())
        .unwrap();
    state.register_replicated_apply_driver(ecosystem, driver);
    state.register_http_routes(Arc::new(Routes));

    assert_eq!(
        state
            .replicated_apply_drivers()
            .next()
            .unwrap()
            .apply_replicated_changes(state.serving.as_ref(), &[]),
        Ok(())
    );
    assert_eq!(
        state
            .absolute_driver_for_path("/artifacts/item")
            .unwrap()
            .serve(state.serving.clone(), Request::new(Body::empty()))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let state = Arc::new(state);
    assert_eq!(
        state
            .http_routes()
            .next()
            .unwrap()
            .routes()
            .with_state(state.clone())
            .oneshot(Request::new(Body::empty()))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_registered_mirror_delegates_success_and_failure() {
    let (dir, mut state) = state();
    let ecosystem = Ecosystem::new("example");
    state.register_mirror_driver(ecosystem.clone(), Arc::new(Driver));
    let state = Arc::new(state);
    let settings = toml::Table::new();
    let mut output = Vec::new();
    state
        .mirror_driver_for(&ecosystem)
        .unwrap()
        .mirror(
            state.clone(),
            MirrorRequest {
                action: MirrorAction::Sync,
                index: "catalog",
                settings: &settings,
                configured: &settings,
                overrides: &settings,
            },
            &mut output,
        )
        .await
        .unwrap();
    assert_eq!(output, b"mirror");
    let path = dir.path().join("read-only");
    std::fs::write(&path, b"").unwrap();
    let mut read_only = std::fs::File::open(path).unwrap();
    assert!(
        state
            .mirror_driver_for(&ecosystem)
            .unwrap()
            .mirror(
                state.clone(),
                MirrorRequest {
                    action: MirrorAction::Verify,
                    index: "catalog",
                    settings: &settings,
                    configured: &settings,
                    overrides: &settings,
                },
                &mut read_only,
            )
            .await
            .is_err()
    );
}

#[test]
fn test_registry_installs_optional_owner_capabilities() {
    let (_dir, mut state) = state();
    let ecosystem = Ecosystem::new("example");

    assert!(state.rate_limit_principal_for(&ecosystem).is_none());
    assert!(state.client_discovery_for(&ecosystem).is_none());

    state.register_rate_limit_principal(ecosystem.clone(), &PRINCIPAL_CAPABILITY);
    state.register_client_discovery(ecosystem.clone(), &DISCOVERY_CAPABILITY);

    assert_eq!(
        state
            .rate_limit_principal_for(&ecosystem)
            .unwrap()
            .resolve(state.serving.as_ref(), None, &HeaderMap::new()),
        peryx_identity::Principal::Anonymous
    );
    assert_eq!(
        state
            .client_discovery_for(&ecosystem)
            .unwrap()
            .client_endpoint("catalog"),
        "/client/catalog"
    );
    assert_eq!(
        state.client_discovery_for(&ecosystem).unwrap().discover_index(
            crate::state::IndexDescription {
                name: "catalog".to_owned(),
                route: "catalog".to_owned(),
                ecosystem: "example".to_owned(),
                kind: "hosted",
                layers: Vec::new(),
                precedence: Vec::new(),
                uploads: false,
                volatile_deletes: false,
                upload_to: None,
                upstream: None,
                hosted: None,
            },
            None,
        ),
        serde_json::Value::Null
    );
}

#[tokio::test]
async fn test_registry_returns_typed_indexed_driver() {
    let (_dir, mut state) = state();
    let ecosystem = Ecosystem::new("indexed");

    state
        .register_protocol(ProtocolDriver::Indexed(Arc::new(IndexedDriver)), default_indexer())
        .unwrap();

    let driver = state.indexed_driver_for(&ecosystem).unwrap();
    assert_eq!(driver.ecosystem(), ecosystem);
    assert_eq!(driver.classify_route("/artifact"), RouteClass::Artifact);
    assert_eq!(
        driver
            .get(
                state.serving.clone(),
                0,
                String::new(),
                Uri::from_static("/"),
                HeaderMap::new(),
                Method::GET,
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        driver
            .post(
                state.serving.clone(),
                String::new(),
                Request::builder().body(Body::from("post body")).unwrap(),
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        driver
            .put(
                state.serving.clone(),
                Request::builder()
                    .method(Method::PUT)
                    .uri("/artifact")
                    .body(Body::from("put body"))
                    .unwrap(),
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        driver
            .delete(state.serving.clone(), Uri::from_static("/artifact"), HeaderMap::new())
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert!(state.absolute_driver_for_path("/indexed/item").is_none());
}

#[tokio::test]
async fn test_protocol_replacement_removes_stale_absolute_mounts() {
    let (_dir, mut state) = state();

    state
        .register_protocol(ProtocolDriver::Absolute(Arc::new(Driver)), default_indexer())
        .unwrap();
    state
        .register_protocol(ProtocolDriver::Absolute(Arc::new(ReplacementDriver)), default_indexer())
        .unwrap();

    assert!(state.absolute_driver_for_path("/artifacts/item").is_none());
    assert!(state.absolute_driver_for_path("/resources/item").is_some());
    assert_eq!(
        state.absolute_mounts().map(|(prefix, _)| prefix).collect::<Vec<_>>(),
        ["/resources"]
    );
    let driver = state.absolute_driver_for_path("/resources/item").unwrap();
    assert_eq!(driver.classify_route("/resources/item"), RouteClass::Artifact);
    assert_eq!(
        driver
            .serve(state.serving.clone(), Request::new(Body::empty()))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn test_registry_runtime_settings_replace_defaults() {
    let (_dir, mut state) = state();

    state.set_openapi("{\"openapi\":\"3.1.0\"}");
    state
        .set_token_realm(peryx_identity::Signer::new(b"signing-key", "peryx"), 41)
        .unwrap();
    state.register_plugin_service(Arc::new("plugin-service")).unwrap();
    state
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: peryx_core::NodeRole::Replica,
            topology: peryx_core::TopologyConfig::default(),
            blobs: peryx_ha::BlobServices::new(None, Arc::new(Durability)),
            analytics: Arc::new(Completeness),
            capabilities: peryx_ha::AvailabilityCapabilities::default(),
            authority_drainer: Some(Arc::new(Drainer)),
            operations: None,
        })
        .unwrap();
    let digest = peryx_storage::blob::Digest::of(b"registry");
    let serving = state.serving.as_ref();

    assert_eq!(state.openapi(), "{\"openapi\":\"3.1.0\"}");
    assert!(serving.signer.is_some());
    assert_eq!(serving.plugin_service::<&str>(), Some(&"plugin-service"));
    assert_eq!(serving.token_ttl_secs, 41);
    assert_eq!(serving.availability_role(), peryx_core::NodeRole::Replica);
    assert_eq!(
        serving.authority_drainer().unwrap().drain(0, &|| false).await.unwrap(),
        peryx_ha::AvailabilityTaskReport {
            processed: 0,
            changed: 0,
        }
    );
    assert!(
        serving
            .analytics_completeness()
            .unwrap()
            .assess(
                &serving.meta,
                &[],
                &peryx_ha::CompletenessQuery {
                    from_day: 1,
                    to_day: 2,
                    today: 3,
                    repository: Some("catalog".to_owned()),
                },
            )
            .is_err()
    );
    assert_eq!(
        serving
            .confirm_blob_write(peryx_ha::CommittedBlob::new(
                &digest,
                "catalog",
                peryx_ha::AuthorityEpoch(1),
                None,
                BlobDurability::Filesystem,
            ))
            .await,
        peryx_ha::WriteDurability::Unavailable
    );
}

#[test]
fn test_index_summaries_skip_missing_drivers() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let state = AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "catalog".to_owned(),
            route: "catalog".to_owned(),
            ecosystem: Ecosystem::new("missing"),
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );

    assert!(state.index_summaries(5).is_empty());
}

#[test]
fn test_index_summaries_skip_drivers_without_the_capability() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "catalog".to_owned(),
            route: "catalog".to_owned(),
            ecosystem: Ecosystem::new("bare"),
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    state.register_driver(Arc::new(BareDriver));

    assert!(state.index_summaries(5).is_empty());
}

#[test]
fn test_index_summaries_dispatch_to_matching_capability() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "catalog".to_owned(),
            route: "catalog".to_owned(),
            ecosystem: Ecosystem::new("example"),
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    register_driver_capabilities(&mut state);
    state.register_driver(Arc::new(Driver));

    assert_eq!(
        state.index_summaries(5).get("catalog"),
        Some(&Ok(IndexSummary::default()))
    );
}

#[rstest]
#[case::driver_failure("failure", IndexSummaryError::Storage, "storage")]
#[case::driver_omission("omitted", IndexSummaryError::InvalidData, "invalid_data")]
fn test_index_summaries_report_failures(
    #[case] name: &str,
    #[case] expected: IndexSummaryError,
    #[case] expected_class: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        vec![Index {
            name: name.to_owned(),
            route: name.to_owned(),
            ecosystem: Ecosystem::new("example"),
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    register_driver_capabilities(&mut state);
    state.register_driver(Arc::new(Driver));

    let error = state.index_summaries(5).remove(name).unwrap().unwrap_err();
    assert_eq!((error, error.as_str()), (expected, expected_class));
}

#[test]
fn test_auth_install_context_exposes_the_configured_signer() {
    let (_dir, mut state) = state();
    state
        .set_token_realm(peryx_identity::Signer::new(b"signing-key", "peryx"), 300)
        .unwrap();

    assert!(state.auth_install_context().unwrap().signer().is_some());
}

#[test]
fn test_runtime_install_context_finds_a_configured_ecosystem() {
    let dir = tempfile::tempdir().unwrap();
    let ecosystem = Ecosystem::new("example");
    let mut state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        vec![Index {
            name: "catalog".to_owned(),
            route: "catalog".to_owned(),
            ecosystem: ecosystem.clone(),
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );

    assert!(state.runtime_install_context().unwrap().has_ecosystem(&ecosystem));
}

#[tokio::test]
async fn test_install_contexts_publish_registered_behavior() {
    let (_dir, mut state) = state();
    let ecosystem = Ecosystem::new("example");
    {
        let mut context = state.capability_install_context();
        context.replace_drivers(crate::DriverSet::default().with(Arc::new(BareDriver)));
        context.register_protocol(ProtocolDriver::Absolute(Arc::new(Driver)));
        context.register_protocol(ProtocolDriver::Indexed(Arc::new(IndexedDriver)));
        context.register_rate_limit_principal(ecosystem.clone(), &PRINCIPAL_CAPABILITY);
        context.register_client_discovery(ecosystem.clone(), &DISCOVERY_CAPABILITY);
    }
    assert!(state.driver_for(&Ecosystem::new("bare")).is_some());
    assert!(state.absolute_driver_for_path("/artifacts/item").is_some());
    assert!(state.indexed_driver_for(&Ecosystem::new("indexed")).is_some());
    assert!(state.rate_limit_principal_for(&ecosystem).is_some());
    assert!(state.client_discovery_for(&ecosystem).is_some());

    {
        let mut context = state.auth_install_context().unwrap();
        assert!(context.signer().is_none());
        assert_eq!(context.token_ttl_secs(), 300);
        context.register_service(Arc::new(41_u64));
        context.register_routes(Arc::new(Routes));
    }
    assert_eq!(state.serving.plugin_service::<u64>(), Some(&41));

    {
        let mut context = state.runtime_install_context().unwrap();
        assert!(!context.has_ecosystem(&ecosystem));
        context.register_service(Arc::new("runtime".to_owned()));
        context.register_protocol(ProtocolDriver::Absolute(Arc::new(ReplacementDriver)), default_indexer());
        context.register_protocol(ProtocolDriver::Indexed(Arc::new(IndexedDriver)), default_indexer());
        context.register_idle_reclaimer(ecosystem.clone(), Arc::new(RuntimeCapability));
        context.register_intent_finalizer(ecosystem.clone(), Arc::new(RuntimeCapability));
        context.register_cache_refresher(ecosystem.clone(), Arc::new(RuntimeCapability));
        context.register_mirror(ecosystem.clone(), Arc::new(Driver));
        context.register_lexicon(ecosystem.clone(), &Lexicon::NEUTRAL);
        context.register_routes(Arc::new(Routes));
    }
    assert_eq!(
        state.serving.plugin_service::<String>().map(String::as_str),
        Some("runtime")
    );
    assert!(state.absolute_driver_for_path("/resources/item").is_some());
    assert!(state.absolute_driver_for_path("/artifacts/item").is_none());
    assert!(state.indexed_driver_for(&Ecosystem::new("indexed")).is_some());
    assert_eq!(
        state
            .idle_reclaimers()
            .next()
            .unwrap()
            .1
            .reclaim_idle(state.serving.clone())
            .await,
        1
    );
    assert_eq!(
        state
            .intent_finalizers()
            .next()
            .unwrap()
            .1
            .finalize_admitted(state.serving.clone())
            .await,
        2
    );
    assert_eq!(
        state
            .cache_refreshers()
            .next()
            .unwrap()
            .1
            .refresh_stale(state.serving.clone())
            .await
            .unwrap(),
        RefreshSweep { checked: 3, changed: 1 }
    );
    assert!(state.mirror_driver_for(&ecosystem).is_some());
    assert_eq!(state.search_ctx().lexicon(&ecosystem).repository, "repository");
    assert_eq!(state.http_routes().count(), 2);

    {
        let mut context = state.distributed_install_context().unwrap();
        assert!(!context.runtime().has_ecosystem(&ecosystem));
        context.register_replicated_apply(ecosystem.clone(), Arc::new(Driver));
    }
    assert_eq!(state.replicated_apply_drivers().count(), 1);
    assert_eq!(
        state
            .replicated_apply_drivers()
            .next()
            .unwrap()
            .apply_replicated_changes(state.serving.as_ref(), &[]),
        Ok(())
    );
}

#[test]
fn test_registration_defaults_and_read_only_mutation_are_observable() {
    let registration = Registration;
    let ecosystem = registration.ecosystem();
    assert_eq!(
        (
            registration.default_indexes(),
            registration.absolute_prefixes(),
            registration.webhook_events(),
            registration.driver().ecosystem(),
        ),
        (&[][..], &["/artifacts"][..], &[][..], ecosystem)
    );
    let mut drivers = crate::DriverSet::default();
    registration.register_capabilities(&mut drivers);
    assert!(drivers.present().next().is_none());

    let (_dir, mut state) = state();
    state.set_read_only(true).unwrap();
    assert!(state.serving.read_only);
    let shared = state.serving.clone();
    assert_eq!(
        state.set_read_only(false),
        Err("serving state is already shared".to_owned())
    );
    assert!(shared.read_only);
}

#[test]
fn test_read_only_retry_interval_is_observable() {
    let (_dir, mut state) = state();

    state.set_read_only_retry_after(Some(Duration::from_secs(17))).unwrap();

    assert_eq!(state.serving.read_only_retry_after(), Some(Duration::from_secs(17)));
}

fn assert_search_invalidation_refreshes(invalidate: impl FnOnce(&ServingState)) {
    let (_dir, mut state) = state();
    let text = Arc::new(Mutex::new("old".to_owned()));
    state.register_lexicon(Ecosystem::new("indexed"), &Lexicon::NEUTRAL);
    state
        .register_protocol(
            ProtocolDriver::Indexed(Arc::new(IndexedDriver)),
            Arc::new(MutableDocs(text.clone())),
        )
        .unwrap();
    let state = Arc::new(state);
    let services = crate::http_services::HttpDomainServices::for_state(&state);
    assert_eq!(
        services.search().search(SearchParams::default(), None).unwrap().total,
        1
    );
    *text.lock().unwrap() = "new".to_owned();

    invalidate(&state.serving);

    assert_eq!(
        services
            .search()
            .search(
                SearchParams {
                    query: "new".to_owned(),
                    ..SearchParams::default()
                },
                None,
            )
            .unwrap()
            .total,
        1
    );
}

#[test]
fn test_search_epoch_refreshes_the_published_index() {
    assert_search_invalidation_refreshes(ServingState::bump_search_epoch);
}

#[test]
fn test_scoped_search_invalidation_refreshes_the_resource() {
    assert_search_invalidation_refreshes(|state| state.invalidate_search_resource("package"));
}
