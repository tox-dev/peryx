use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse as _, Response};
use peryx_core::{DefaultIndex, DefaultIndexKind, Ecosystem};
use peryx_driver::discovery::BaseUrl;
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{
    AbsoluteProtocolDriver, BlobReferenceDriver, CapabilityRegistrar, ClientDiscovery, CompiledEcosystemSettings,
    DistributedInstallContext, DistributedRuntime, EcosystemConfig, EcosystemDriver, EcosystemOpenApi,
    EcosystemRegistration, EcosystemRuntime, EcosystemSnippet, FsckDriver, ImportDriver, JobConfig, JobDriver,
    MirrorAction, MirrorDriver, MirrorRequest, NameDriver, ProtocolDriver, RetentionDriver, RuntimeInstallContext,
};
use peryx_driver::state::{AppState, IndexDescription, ServingState};
use peryx_plugin_registry::{PluginRegistration, PluginRegistry};
use peryx_policy::{
    RetentionCandidate, RetentionClass, RetentionDecision, RetentionPolicy, RetentionSummary, RetentionVisibility,
};
use peryx_search::default_indexer;
use peryx_storage::blob::Digest;
use peryx_storage::meta::MetaStore;
use peryx_test_support::EcosystemDriverFixture;
use utoipa::openapi::PathsBuilder;

/// Leptos SSR uses process-global arenas and can lose wakes during concurrent test renders.
pub fn render_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(tokio::sync::Mutex::default)
}

pub fn plugins() -> PluginRegistry {
    registry(&REGISTRATION)
}

pub fn plugins_with_blob_references() -> PluginRegistry {
    registry(&BLOB_REGISTRATION)
}

pub fn plugins_with_broken_blob_references() -> PluginRegistry {
    registry(&BROKEN_BLOB_REGISTRATION)
}

#[cfg(unix)]
pub fn with_blob_reference_event<T>(event: impl FnOnce() + 'static, action: impl FnOnce() -> T) -> T {
    BLOB_REFERENCE_EVENT.with(|slot| {
        assert!(slot.borrow().is_none());
        slot.replace(Some(Box::new(event)));
    });
    let result = action();
    BLOB_REFERENCE_EVENT.with(|slot| assert!(slot.borrow().is_none()));
    result
}

pub fn plugins_with_fsck() -> PluginRegistry {
    registry(&FSCK_REGISTRATION)
}

pub fn store_repositories(meta: &MetaStore, ecosystems: &[&str]) {
    for ecosystem in ecosystems {
        meta.create_repository(
            peryx_storage::meta::NewRepository {
                route: (*ecosystem).to_owned(),
                display_name: (*ecosystem).to_owned(),
                ecosystem: (*ecosystem).to_owned(),
                definition: serde_json::json!({}),
                created_by: peryx_identity::UserId::random(),
            },
            1,
        )
        .unwrap();
    }
}

pub fn plugins_without_retention() -> PluginRegistry {
    plugin_registry(&PLAIN_REGISTRATION, &PLAIN_RUNTIME, &PLAIN_RUNTIME, &PLAIN_DRIVER, None)
}

pub fn plugins_with_metadata_migration(migration: Arc<dyn peryx_storage::meta::MetadataMigration>) -> PluginRegistry {
    registry_with_migration(&REGISTRATION, Some(migration))
}

#[cfg(feature = "composition-pypi")]
pub fn plugins_with_inactive_owner(
    migration: Option<Arc<dyn peryx_storage::meta::MetadataMigration>>,
) -> PluginRegistry {
    PluginRegistry::new(vec![
        plugin_registration(&BLOB_REGISTRATION, &BLOB_RUNTIME, &BLOB_RUNTIME, &BLOB_DRIVER, None, 1),
        plugin_registration(
            &INACTIVE_REGISTRATION,
            &INACTIVE_RUNTIME,
            &INACTIVE_RUNTIME,
            &INACTIVE_DRIVER,
            migration,
            2,
        ),
    ])
    .unwrap()
}

pub fn fixture_job() -> peryx_driver::jobs::PluginScheduledJob {
    peryx_driver::jobs::PluginScheduledJob::new(
        CORE,
        Arc::new(FixtureJob {
            settings: toml::Table::from_iter([("limit".to_owned(), toml::Value::Integer(9))]),
        }),
    )
}

fn registry(registration: &'static Registration) -> PluginRegistry {
    registry_with_migration(registration, None)
}

fn registry_with_migration(
    registration: &'static Registration,
    metadata_migration: Option<Arc<dyn peryx_storage::meta::MetadataMigration>>,
) -> PluginRegistry {
    plugin_registry(
        registration,
        registration.runtime,
        registration.runtime,
        registration.driver,
        metadata_migration,
    )
}

fn plugin_registry(
    registration: &'static dyn EcosystemRegistration,
    runtime: &'static dyn EcosystemRuntime,
    distributed_runtime: &'static dyn DistributedRuntime,
    client_discovery: &'static dyn ClientDiscovery,
    metadata_migration: Option<Arc<dyn peryx_storage::meta::MetadataMigration>>,
) -> PluginRegistry {
    PluginRegistry::new(vec![plugin_registration(
        registration,
        runtime,
        distributed_runtime,
        client_discovery,
        metadata_migration,
        1,
    )])
    .unwrap()
}

fn plugin_registration(
    registration: &'static dyn EcosystemRegistration,
    runtime: &'static dyn EcosystemRuntime,
    distributed_runtime: &'static dyn DistributedRuntime,
    client_discovery: &'static dyn ClientDiscovery,
    metadata_migration: Option<Arc<dyn peryx_storage::meta::MetadataMigration>>,
    priority: u16,
) -> PluginRegistration {
    PluginRegistration {
        registration,
        config: &ECOSYSTEM_CONFIG,
        runtime,
        distributed_runtime: Some(distributed_runtime),
        rate_limit_principal: None,
        client_discovery: Some(client_discovery),
        openapi: &OPEN_API,
        auth: None,
        browse: None,
        snippets: Some(&SNIPPETS),
        metadata_migration,
        operator_jobs: &[],
        priority,
    }
}

const CORE: Ecosystem = Ecosystem::new("core");
const PLAIN: Ecosystem = Ecosystem::new("plain");
thread_local! {
    static BLOB_REFERENCE_EVENT: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::default();
}
static REGISTRATION: Registration = Registration {
    driver: &DRIVER,
    runtime: &RUNTIME,
};
static BLOB_REGISTRATION: Registration = Registration {
    driver: &BLOB_DRIVER,
    runtime: &BLOB_RUNTIME,
};
static BROKEN_BLOB_REGISTRATION: Registration = Registration {
    driver: &BROKEN_BLOB_DRIVER,
    runtime: &BROKEN_BLOB_RUNTIME,
};
static FSCK_REGISTRATION: Registration = Registration {
    driver: &FSCK_DRIVER,
    runtime: &FSCK_RUNTIME,
};
static PLAIN_REGISTRATION: PlainRegistration = PlainRegistration {
    default_indexes: &PLAIN_DEFAULT_INDEXES,
};
#[cfg(feature = "composition-pypi")]
static INACTIVE_REGISTRATION: PlainRegistration = PlainRegistration { default_indexes: &[] };
static CORE_DEFAULT_INDEXES: [DefaultIndex; 1] = [DefaultIndex {
    name: "main",
    route: "main",
    ecosystem: CORE,
    kind: DefaultIndexKind::Hosted,
}];
static PLAIN_DEFAULT_INDEXES: [DefaultIndex; 1] = [DefaultIndex {
    name: "plain",
    route: "plain",
    ecosystem: PLAIN,
    kind: DefaultIndexKind::Hosted,
}];
static DRIVER: Driver = Driver {
    ecosystem: CORE,
    capabilities: &[
        Capability::DirectoryImport,
        Capability::Mirroring,
        Capability::Names,
        Capability::Retention,
    ],
};
static BLOB_DRIVER: Driver = Driver {
    ecosystem: CORE,
    capabilities: &[
        Capability::BlobReferences,
        Capability::DirectoryImport,
        Capability::Mirroring,
        Capability::Names,
        Capability::Retention,
    ],
};
static BROKEN_BLOB_DRIVER: Driver = Driver {
    ecosystem: CORE,
    capabilities: &[Capability::BrokenBlobReferences],
};
static FSCK_DRIVER: Driver = Driver {
    ecosystem: CORE,
    capabilities: &[
        Capability::DirectoryImport,
        Capability::Fsck,
        Capability::MetadataRepair,
        Capability::Mirroring,
        Capability::Names,
        Capability::Retention,
    ],
};
static PLAIN_DRIVER: EcosystemDriverFixture = EcosystemDriverFixture::new(PLAIN, RouteClass::Metadata);
#[cfg(feature = "composition-pypi")]
static INACTIVE_DRIVER: Driver = Driver {
    ecosystem: PLAIN,
    capabilities: &[Capability::BrokenBlobReferences],
};
static ECOSYSTEM_CONFIG: TestConfig = TestConfig;
static RUNTIME: Runtime = Runtime { driver: &DRIVER };
static BLOB_RUNTIME: Runtime = Runtime { driver: &BLOB_DRIVER };
static BROKEN_BLOB_RUNTIME: Runtime = Runtime {
    driver: &BROKEN_BLOB_DRIVER,
};
static FSCK_RUNTIME: Runtime = Runtime { driver: &FSCK_DRIVER };
static PLAIN_RUNTIME: PlainRuntime = PlainRuntime;
#[cfg(feature = "composition-pypi")]
static INACTIVE_RUNTIME: Runtime = Runtime {
    driver: &INACTIVE_DRIVER,
};
static OPEN_API: OpenApi = OpenApi;
static SNIPPETS: Snippets = Snippets;

struct Registration {
    driver: &'static Driver,
    runtime: &'static Runtime,
}

impl EcosystemRegistration for Registration {
    fn ecosystem(&self) -> Ecosystem {
        CORE
    }

    fn default_indexes(&self) -> &'static [DefaultIndex] {
        &CORE_DEFAULT_INDEXES
    }

    fn absolute_prefixes(&self) -> &'static [&'static str] {
        &["/+fixture"]
    }

    fn webhook_events(&self) -> &'static [&'static str] {
        &["upload"]
    }

    fn driver(&self) -> ProtocolDriver {
        ProtocolDriver::Absolute(Arc::new((*self.driver).clone()))
    }

    fn register_capabilities(&self, registrar: &mut dyn CapabilityRegistrar) {
        self.driver.register_capabilities(registrar);
    }
}

struct PlainRegistration {
    default_indexes: &'static [DefaultIndex],
}

impl EcosystemRegistration for PlainRegistration {
    fn ecosystem(&self) -> Ecosystem {
        PLAIN
    }

    fn default_indexes(&self) -> &'static [DefaultIndex] {
        self.default_indexes
    }

    fn absolute_prefixes(&self) -> &'static [&'static str] {
        &[]
    }

    fn webhook_events(&self) -> &'static [&'static str] {
        &["upload"]
    }

    fn driver(&self) -> ProtocolDriver {
        ProtocolDriver::Absolute(Arc::new(PLAIN_DRIVER.clone()))
    }

    fn register_capabilities(&self, _: &mut dyn peryx_driver::serving::CapabilityRegistrar) {}
}

struct TestConfig;

impl EcosystemConfig for TestConfig {
    fn compile_index_settings(&self, _: &str, _: &toml::Table) -> Result<Option<CompiledEcosystemSettings>, String> {
        Ok(None)
    }
}

struct Runtime {
    driver: &'static Driver,
}

impl EcosystemRuntime for Runtime {
    fn install(
        &self,
        context: &mut RuntimeInstallContext<'_>,
        _: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        self.register_drivers(context);
        Ok(())
    }
}

impl DistributedRuntime for Runtime {
    fn install(
        &self,
        context: &mut DistributedInstallContext<'_>,
        _: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        self.register_drivers(context.runtime());
        Ok(())
    }
}

impl Runtime {
    fn register_drivers(&self, context: &mut RuntimeInstallContext<'_>) {
        context.register_protocol(
            ProtocolDriver::Absolute(Arc::new((*self.driver).clone())),
            default_indexer(),
        );
        if self.driver.has(Capability::Mirroring) {
            context.register_mirror(CORE, Arc::new((*self.driver).clone()));
        }
    }
}

struct PlainRuntime;

impl EcosystemRuntime for PlainRuntime {
    fn install(
        &self,
        context: &mut RuntimeInstallContext<'_>,
        _: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        register_plain_driver(context);
        Ok(())
    }
}

impl DistributedRuntime for PlainRuntime {
    fn install(
        &self,
        context: &mut DistributedInstallContext<'_>,
        _: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        register_plain_driver(context.runtime());
        Ok(())
    }
}

fn register_plain_driver(context: &mut RuntimeInstallContext<'_>) {
    context.register_protocol(
        ProtocolDriver::Absolute(Arc::new(PLAIN_DRIVER.clone())),
        default_indexer(),
    );
}

struct OpenApi;

impl EcosystemOpenApi for OpenApi {
    fn paths(&self, paths: PathsBuilder, _reads: peryx_driver::route_auth::ReadExposure) -> PathsBuilder {
        paths
    }
}

struct Snippets;

impl EcosystemSnippet for Snippets {
    fn text(&self, base: &BaseUrl, route: &str, _: bool, format: &str) -> Result<Option<String>, String> {
        if format != "client.conf" {
            return Err(format!("unsupported snippet format {format:?}"));
        }
        if route == "read-only" {
            return Ok(None);
        }
        Ok(Some(format!("endpoint = {}\n", base.join(&format!("/{route}/")))))
    }
}

#[derive(Clone)]
struct Driver {
    ecosystem: Ecosystem,
    capabilities: &'static [Capability],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Capability {
    BlobReferences,
    BrokenBlobReferences,
    DirectoryImport,
    Fsck,
    MetadataRepair,
    Mirroring,
    Names,
    Retention,
}

impl Driver {
    fn has(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    fn register_capabilities(&self, registrar: &mut dyn CapabilityRegistrar) {
        if self.has(Capability::BlobReferences) || self.has(Capability::BrokenBlobReferences) {
            registrar.register_blob_references(self.ecosystem.clone(), Arc::new(self.clone()));
        }
        if self.has(Capability::DirectoryImport) {
            registrar.register_import(self.ecosystem.clone(), Arc::new(self.clone()));
        }
        if self.ecosystem == CORE {
            registrar.register_job(self.ecosystem.clone(), Arc::new(self.clone()));
        }
        if self.has(Capability::Names) {
            registrar.register_name(self.ecosystem.clone(), Arc::new(self.clone()));
        }
        if self.has(Capability::Retention) {
            registrar.register_retention(self.ecosystem.clone(), Arc::new(self.clone()));
        }
        if self.has(Capability::Fsck) {
            registrar.register_fsck(self.ecosystem.clone(), Arc::new(self.clone()));
        }
        if self.has(Capability::MetadataRepair) {
            registrar.register_metadata_repair(self.ecosystem.clone(), Arc::new(self.clone()));
        }
    }
}

impl EcosystemDriver for Driver {
    fn ecosystem(&self) -> Ecosystem {
        self.ecosystem.clone()
    }
}

impl ClientDiscovery for Driver {
    fn discover_index(&self, _: IndexDescription, _: Option<&peryx_driver::discovery::BaseUrl>) -> serde_json::Value {
        serde_json::Value::Null
    }

    fn client_endpoint(&self, route: &str) -> String {
        format!("/{route}/")
    }
}

#[test]
fn driver_client_discovery_builds_the_route_endpoint() {
    assert_eq!(ClientDiscovery::client_endpoint(&DRIVER, "index"), "/index/");
}

impl peryx_driver::serving::MetadataRepairDriver for Driver {
    fn preview_metadata_repair(
        &self,
        _: &MetaStore,
        _: &[peryx_driver::Index],
        out: &mut dyn std::io::Write,
    ) -> Result<u64, String> {
        writeln!(out, "metadata\t{}\twould rebuild", self.ecosystem.as_str()).map_err(|error| error.to_string())?;
        Ok(1)
    }

    fn repair_metadata(
        &self,
        _: &MetaStore,
        _: &[peryx_driver::Index],
        out: &mut dyn std::io::Write,
    ) -> Result<u64, String> {
        writeln!(out, "metadata\t{}\trebuilt", self.ecosystem.as_str()).map_err(|error| error.to_string())?;
        Ok(1)
    }
}

impl FsckDriver for Driver {
    fn fsck_metadata(
        &self,
        _: &MetaStore,
        _: &peryx_storage::blob::BlobStorage,
        _: &[peryx_driver::Index],
        out: &mut dyn std::io::Write,
    ) -> Result<u64, String> {
        writeln!(out, "metadata\t{}\tinvalid", self.ecosystem.as_str()).map_err(|error| error.to_string())?;
        Ok(1)
    }
}

impl JobDriver for Driver {
    fn compile_job(&self, config: JobConfig<'_>) -> Option<Result<peryx_driver::jobs::PluginScheduledJob, String>> {
        (config.kind == "fixture").then(|| {
            Ok(peryx_driver::jobs::PluginScheduledJob::new(
                self.ecosystem.clone(),
                Arc::new(FixtureJob {
                    settings: config.settings.clone(),
                }),
            ))
        })
    }
}

struct FixtureJob {
    settings: toml::Table,
}

impl peryx_driver::jobs::ScheduledJobFactory for FixtureJob {
    fn kind(&self) -> &'static str {
        "fixture"
    }

    fn settings(&self) -> toml::Table {
        self.settings.clone()
    }

    fn create(&self, _: &AppState) -> Result<Arc<dyn peryx_driver::jobs::NodeJob>, String> {
        Err("the serialization fixture cannot run".to_owned())
    }
}

#[async_trait::async_trait]
impl MirrorDriver for Driver {
    fn validate_options(&self, _configured: &toml::Table, _overrides: &toml::Table) -> Result<(), String> {
        Ok(())
    }

    async fn mirror(
        &self,
        _: Arc<AppState>,
        request: MirrorRequest<'_>,
        output: &mut (dyn std::io::Write + Send),
    ) -> Result<(), String> {
        if request.overrides.get("fail").and_then(toml::Value::as_bool) == Some(true) {
            return Err("mirror failed".to_owned());
        }
        writeln!(
            output,
            "{:?}\t{}\t{}\t{}",
            request.action,
            request.index,
            request.configured.len(),
            request.overrides.len(),
        )
        .map_err(|error| error.to_string())
    }
}

impl NameDriver for Driver {
    fn normalize_name(&self, name: &str) -> String {
        name.to_ascii_lowercase()
    }
}

impl ImportDriver for Driver {
    fn import_dir(
        &self,
        _: &MetaStore,
        _: &peryx_storage::blob::BlobStorage,
        _: &str,
        _: &str,
        _: &std::path::Path,
        out: &mut dyn std::io::Write,
    ) -> Result<(), String> {
        out.write_all(b"status\tartifact\tresource\tgroup\treason\nsummary\t\t\t\timported=0 skipped=0 rejected=0\n")
            .map_err(|error| error.to_string())
    }
}

impl BlobReferenceDriver for Driver {
    fn referenced_blob_digests(&self, _: &MetaStore) -> Result<BTreeSet<String>, String> {
        BLOB_REFERENCE_EVENT.with(|slot| {
            if let Some(event) = slot.take() {
                event();
            }
        });
        if self.has(Capability::BrokenBlobReferences) {
            return Err("blob-reference scan failed".to_owned());
        }
        Ok([Digest::of(b"artifact bytes"), Digest::of(b"metadata bytes")]
            .into_iter()
            .map(|digest| digest.as_str().to_owned())
            .collect())
    }
}

impl RetentionDriver for Driver {
    fn validate_retention(&self, _policy: &RetentionPolicy) -> Result<(), String> {
        Ok(())
    }

    fn plan_retention(
        &self,
        scan: &peryx_driver::serving::RetentionScan<'_>,
        start: &mut dyn FnMut(RetentionSummary) -> Result<(), String>,
        emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate_retention(scan.policy)?;
        let generation = scan
            .meta
            .policy_input_generation(scan.index)
            .map_err(|error| error.to_string())?;
        start(RetentionSummary {
            policy_version: scan.policy.version(),
            frontier: peryx_policy::RetentionFrontier {
                repository: generation.repository,
                catalog: generation.catalog,
                policy: generation.policy,
            },
        })?;
        let mut plan = scan.policy.plan_resource(
            scan.now,
            [("2.0", 0), ("1.0", 1)]
                .into_iter()
                .map(|(group, rank)| RetentionCandidate {
                    resource: "item".to_owned(),
                    group: Some(group.to_owned()),
                    artifact: format!("item-{group}.bin"),
                    digest: format!("sha-{group}"),
                    class: RetentionClass::Hosted,
                    visibility: RetentionVisibility::Active,
                    source: None,
                    bytes: 1024,
                    upload_time_unix: Some(0),
                    rank,
                    orphan: false,
                })
                .collect(),
        );
        plan.skip(scan.skip);
        for decision in plan.decisions() {
            emit(decision)?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl AbsoluteProtocolDriver for Driver {
    fn prefixes(&self) -> &'static [&'static str] {
        &["/+fixture"]
    }

    fn classify_route(&self, _: &str) -> RouteClass {
        RouteClass::Metadata
    }

    async fn serve(&self, state: Arc<ServingState>, request: Request) -> Response {
        if request.uri().path() != "/+fixture/upload" {
            return StatusCode::NOT_FOUND.into_response();
        }
        if request.method() != Method::POST {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        let Ok(bytes) = axum::body::to_bytes(request.into_body(), usize::MAX).await else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        match state.blobs.put_bytes(&bytes).await {
            Ok(_) => StatusCode::CREATED.into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

#[test]
fn test_distributed_install_registers_the_mirror_driver() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = fixture_state(dir.path());
    let plugins = plugins().activate([CORE]).unwrap();

    plugins.register_activated_capabilities(&mut state.capability_install_context());
    plugins
        .install_distributed_drivers(&mut state.distributed_install_context().unwrap(), &HashMap::new())
        .unwrap();

    assert!(state.mirror_driver_for(&CORE).is_some());
}

#[test]
fn test_driver_metadata_contract() {
    let protocol = REGISTRATION.driver();
    let plugins = plugins().activate([CORE]).unwrap();

    assert_eq!(protocol.classify_route("artifact"), RouteClass::Metadata);
    assert_eq!(
        DRIVER.discover_index(
            IndexDescription {
                name: "main".to_owned(),
                route: "main".to_owned(),
                ecosystem: CORE.to_string(),
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
        serde_json::Value::Null,
    );
    assert_eq!(
        plugins.drivers().get_name(&CORE).unwrap().normalize_name("MiXeD"),
        "mixed"
    );
    assert_eq!(protocol.absolute().unwrap().prefixes(), &["/+fixture"]);
}

#[test]
fn test_fixture_job_refuses_runtime_creation() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        peryx_driver::jobs::scheduled_job(
            &fixture_state(dir.path()),
            &peryx_driver::jobs::ScheduledJob::Plugin(fixture_job()),
        )
        .err()
        .unwrap(),
        "the serialization fixture cannot run",
    );
}

#[tokio::test]
async fn test_mirror_reports_the_output_error() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(fixture_state(dir.path()));
    let settings = toml::Table::new();
    let (mut output, expected) = read_only_writer(dir.path(), "mirror-output");

    assert_eq!(
        MirrorDriver::mirror(
            &DRIVER,
            state,
            MirrorRequest {
                action: MirrorAction::Plan,
                index: "main",
                settings: &settings,
                configured: &settings,
                overrides: &settings,
            },
            &mut output,
        )
        .await,
        Err(expected),
    );
}

#[test]
fn test_import_reports_the_output_error() {
    let dir = tempfile::tempdir().unwrap();
    let state = fixture_state(dir.path());
    let (mut output, expected) = read_only_writer(dir.path(), "import-output");

    assert_eq!(
        ImportDriver::import_dir(
            &DRIVER,
            &state.serving.meta,
            &state.serving.blobs,
            "main",
            "main",
            dir.path(),
            &mut output,
        ),
        Err(expected),
    );
}

#[tokio::test]
async fn test_absolute_protocol_rejects_unknown_requests() {
    let dir = tempfile::tempdir().unwrap();
    let state = fixture_state(dir.path());

    assert_eq!(
        REGISTRATION
            .driver()
            .absolute()
            .unwrap()
            .serve(Arc::clone(&state.serving), Request::new(Body::empty()))
            .await
            .status(),
        StatusCode::NOT_FOUND,
    );
}

fn fixture_state(dir: &std::path::Path) -> AppState {
    AppState::new(
        MetaStore::open(dir.join("peryx.redb")).unwrap(),
        peryx_storage::blob::BlobStorage::filesystem(dir.join("blobs")),
        60,
        Vec::new(),
    )
}

fn read_only_writer(dir: &std::path::Path, name: &str) -> (std::fs::File, String) {
    let path = dir.join(name);
    std::fs::write(&path, b"existing").unwrap();
    let mut expected = std::fs::File::open(&path).unwrap();
    let error = expected.write_all(b"output").unwrap_err().to_string();
    (std::fs::File::open(path).unwrap(), error)
}
