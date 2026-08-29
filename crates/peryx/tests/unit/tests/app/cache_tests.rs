use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use peryx_core::{DefaultIndex, DefaultIndexKind, Ecosystem};
use peryx_driver::discovery::BaseUrl;
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{
    AbsoluteProtocolDriver, CacheDriver, CachePage, CapabilityRegistrar, ClientDiscovery, CompiledEcosystemSettings,
    DistributedInstallContext, EcosystemConfig, EcosystemDriver, EcosystemOpenApi, EcosystemRegistration,
    EcosystemRuntime, NameDriver, ProtocolDriver, PurgeReport, RuntimeInstallContext,
};
use peryx_driver::state::{AppState, IndexDescription, ServingState};
use peryx_plugin_registry::{PluginRegistration, PluginRegistry};
use peryx_search::default_indexer;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use utoipa::openapi::PathsBuilder;
use utoipa::openapi::path::{HttpMethod, Operation, PathItem};

use crate::app::cache_with_plugins;
use crate::cli::{
    CacheCommand, CacheListArgs, CachePurgeCommand, CachePurgeOrphanedBlobsArgs, CachePurgeResourceArgs,
    CacheRuntimeArgs, RuntimeArgs,
};
use crate::config::Config;

const CACHE_LIST_HEADER: &str = "kind\tindex\tresource\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n";
const CORE: Ecosystem = Ecosystem::new("core");

#[test]
fn test_cache_list_reports_an_index_page() {
    let plugins = plugins();
    let (_directory, meta, config) = store_and_config(&plugins);
    drop(meta);
    let mut output = Vec::new();

    cache_with_plugins(
        &config,
        &plugins,
        &CacheCommand::List(page_args(None, Some("widget"), false, None, None)),
        &mut output,
    )
    .unwrap();

    assert_eq!(
        String::from_utf8(output).unwrap(),
        format!("{CACHE_LIST_HEADER}index\tmain\twidget\t\t0\t30\tfalse\t4\tmain/widget\n")
    );
}

#[test]
fn test_cache_list_reports_a_blob() {
    let plugins = plugins();
    let (directory, meta, config) = store_and_config(&plugins);
    drop(meta);
    let blobs = BlobStore::new(directory.path().join("blobs"));
    let digest = blobs.write(b"payload").unwrap();
    write_invalid_blob_path(directory.path());
    let mut output = Vec::new();

    cache_with_plugins(
        &config,
        &plugins,
        &CacheCommand::List(CacheListArgs {
            runtime: RuntimeArgs::default(),
            index: None,
            resource: None,
            digest: Some(digest.as_str().to_owned()),
            stale: false,
            min_age_secs: None,
            min_size_bytes: None,
        }),
        &mut output,
    )
    .unwrap();

    assert_eq!(
        String::from_utf8(output).unwrap(),
        format!(
            "{CACHE_LIST_HEADER}blob\t\t\t{}\t-\t-\t-\t7\t{}\n",
            digest.as_str(),
            blobs.path_for(&digest).display()
        )
    );
}

#[test]
fn test_cache_list_filters_index_pages() {
    let plugins = plugins();
    let (_directory, meta, config) = store_and_config(&plugins);
    drop(meta);
    let cases: [(&str, CacheListArgs, &[&str]); 6] = [
        ("index", page_args(Some("other"), None, false, None, None), &[]),
        ("resource", page_args(None, Some("other"), false, None, None), &[]),
        (
            "normalized resource",
            page_args(None, Some("Widget"), false, None, None),
            &["widget"],
        ),
        ("stale", page_args(None, None, true, None, None), &["gadget"]),
        ("minimum age", page_args(None, None, false, Some(1), None), &["gadget"]),
        ("minimum size", page_args(None, None, false, None, Some(5)), &["gadget"]),
    ];

    for (case, args, expected) in cases {
        let mut output = Vec::new();
        cache_with_plugins(&config, &plugins, &CacheCommand::List(args), &mut output).unwrap();
        assert_eq!(listed_resources(&output), expected, "{case}");
    }
}

#[test]
fn test_cache_list_preserves_resource_without_name_driver() {
    let plugins = plugins_without_names();
    let (_directory, meta, config) = store_and_config(&plugins);
    drop(meta);
    let mut output = Vec::new();

    cache_with_plugins(
        &config,
        &plugins,
        &CacheCommand::List(page_args(None, Some("Widget"), false, None, None)),
        &mut output,
    )
    .unwrap();

    assert!(listed_resources(&output).is_empty());
}

#[test]
fn test_cache_list_filters_blobs() {
    let plugins = plugins();
    let (directory, meta, config) = store_and_config(&plugins);
    drop(meta);
    let blobs = BlobStore::new(directory.path().join("blobs"));
    let digest = blobs.write(b"payload").unwrap();
    let blob = format!(
        "blob\t\t\t{}\t-\t-\t-\t7\t{}\n",
        digest.as_str(),
        blobs.path_for(&digest).display()
    );
    let cases = [
        ("matching", digest.as_str().to_owned(), None, Some(blob.as_str())),
        ("different", "0".repeat(64), None, None),
        ("too small", digest.as_str().to_owned(), Some(8), None),
    ];

    for (case, digest, min_size_bytes, expected) in cases {
        let mut output = Vec::new();
        cache_with_plugins(
            &config,
            &plugins,
            &CacheCommand::List(CacheListArgs {
                runtime: RuntimeArgs::default(),
                index: None,
                resource: None,
                digest: Some(digest),
                stale: false,
                min_age_secs: None,
                min_size_bytes,
            }),
            &mut output,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("{CACHE_LIST_HEADER}{}", expected.unwrap_or_default()),
            "{case}"
        );
    }
}

#[test]
fn test_cache_size_reports_driver_and_blob_totals() {
    let plugins = plugins();
    let (directory, meta, config) = store_and_config(&plugins);
    drop(meta);
    BlobStore::new(directory.path().join("blobs"))
        .write(b"payload")
        .unwrap();
    write_invalid_blob_path(directory.path());
    let mut output = Vec::new();

    cache_with_plugins(
        &config,
        &plugins,
        &CacheCommand::Size(CacheRuntimeArgs {
            runtime: RuntimeArgs::default(),
        }),
        &mut output,
    )
    .unwrap();

    assert_eq!(
        output_counts(&output),
        BTreeMap::from([
            ("blob_bytes".to_owned(), 8),
            ("blob_files".to_owned(), 2),
            ("index_bytes".to_owned(), 24),
            ("index_pages".to_owned(), 2),
            ("invalid_blob_paths".to_owned(), 1),
            ("resource_records".to_owned(), 3),
            ("stale_index_pages".to_owned(), 1),
        ])
    );
}

#[test]
fn test_cache_dispatches_maintenance_commands() {
    let plugins = plugins();
    let (_directory, meta, config) = store_and_config(&plugins);
    drop(meta);
    let cases = [
        (
            "fsck",
            CacheCommand::Fsck(CacheRuntimeArgs {
                runtime: RuntimeArgs::default(),
            }),
            "ok\n",
        ),
        (
            "resource",
            CacheCommand::Purge(CachePurgeCommand::Resource(CachePurgeResourceArgs {
                runtime: RuntimeArgs::default(),
                index: "main".to_owned(),
                resource: "widget".to_owned(),
                yes: false,
            })),
            concat!(
                "action\ttarget\tindex\tresource\tresource_records\n",
                "dry-run\tresource\tmain\twidget\t2\n"
            ),
        ),
        (
            "orphaned blobs",
            CacheCommand::Purge(CachePurgeCommand::OrphanedBlobs(CachePurgeOrphanedBlobsArgs {
                runtime: RuntimeArgs::default(),
                yes: false,
            })),
            concat!(
                "action\ttarget\tdigest\tsize_bytes\tpath\n",
                "summary\tdry-run\torphaned-blobs\t0\t0\n",
                "scope\tecosystems\t\n"
            ),
        ),
    ];

    for (case, command, expected) in cases {
        let mut output = Vec::new();
        cache_with_plugins(&config, &plugins, &command, &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), expected, "{case}");
    }
}

#[test]
fn test_cache_list_reports_output_failures() {
    let plugins = plugins();
    let (_directory, meta, config) = store_and_config(&plugins);
    drop(meta);

    for (capacity, expected) in [
        (0, "failed to write whole buffer"),
        (CACHE_LIST_HEADER.len(), "scan cached index pages"),
    ] {
        let error = cache_with_plugins(
            &config,
            &plugins,
            &CacheCommand::List(page_args(None, None, false, None, None)),
            &mut Cursor::new(vec![0; capacity].into_boxed_slice()),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn test_cache_list_skips_drivers_without_cache_capability() {
    let plugins = plugins_without_cache();
    let (_directory, meta, config) = store_and_config(&plugins);
    drop(meta);
    let mut output = Vec::new();

    cache_with_plugins(
        &config,
        &plugins,
        &CacheCommand::List(page_args(None, None, false, None, None)),
        &mut output,
    )
    .unwrap();

    assert_eq!(String::from_utf8(output).unwrap(), CACHE_LIST_HEADER);
}

#[tokio::test]
async fn test_cache_plugin_contract() {
    let plugins = plugins().activate([CORE]).unwrap();
    assert_eq!(
        plugins
            .compile_index_settings(&CORE, "main", &toml::Table::new())
            .unwrap()
            .unwrap()
            .ecosystem(),
        CORE
    );
    assert!(
        plugins
            .openapi_paths(PathsBuilder::new())
            .build()
            .paths
            .contains_key("/cache-fixture")
    );

    let (directory, meta, _) = store_and_config(&plugins);
    let mut local = AppState::new(meta, BlobStore::new(directory.path().join("blobs")), 60, Vec::new());
    plugins.register_activated_capabilities(&mut local.capability_install_context());
    plugins
        .install_drivers(&mut local.runtime_install_context().unwrap(), &HashMap::new())
        .unwrap();
    assert_eq!(local.driver_for(&CORE).unwrap().ecosystem(), CORE);

    let mut distributed = AppState::new(
        MetaStore::open(directory.path().join("distributed.redb")).unwrap(),
        BlobStore::new(directory.path().join("distributed-blobs")),
        60,
        Vec::new(),
    );
    plugins.register_activated_capabilities(&mut distributed.capability_install_context());
    plugins
        .install_distributed_drivers(&mut distributed.distributed_install_context().unwrap(), &HashMap::new())
        .unwrap();
    assert_eq!(distributed.driver_for(&CORE).unwrap().ecosystem(), CORE);

    let protocol = plugins.protocol(&CORE).unwrap();
    assert_eq!(
        protocol.absolute().unwrap().classify_route("resource"),
        RouteClass::Metadata
    );
    assert_eq!(
        PLUGIN.discover_index(
            IndexDescription {
                name: "main".to_owned(),
                route: "main".to_owned(),
                ecosystem: "core".to_owned(),
                kind: "hosted",
                layers: Vec::new(),
                precedence: Vec::new(),
                uploads: true,
                volatile_deletes: false,
                upload_to: None,
                upstream: None,
                hosted: None,
            },
            None,
        ),
        serde_json::Value::Null
    );
    assert_eq!(protocol.absolute().unwrap().prefixes(), &["/cache-fixture"]);
    assert_eq!(
        protocol
            .absolute()
            .unwrap()
            .serve(local.serving.clone(), Request::new(axum::body::Body::empty()))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

fn listed_resources(output: &[u8]) -> Vec<String> {
    String::from_utf8(output.to_vec())
        .unwrap()
        .lines()
        .skip(1)
        .map(|line| line.split('\t').nth(2).unwrap().to_owned())
        .collect()
}

fn output_counts(output: &[u8]) -> BTreeMap<String, u64> {
    String::from_utf8(output.to_vec())
        .unwrap()
        .lines()
        .map(|line| {
            let (label, value) = line.split_once('\t').unwrap();
            (label.to_owned(), value.parse().unwrap())
        })
        .collect()
}

fn page_args(
    index: Option<&str>,
    resource: Option<&str>,
    stale: bool,
    min_age_secs: Option<u64>,
    min_size_bytes: Option<u64>,
) -> CacheListArgs {
    CacheListArgs {
        runtime: RuntimeArgs::default(),
        index: index.map(str::to_owned),
        resource: resource.map(str::to_owned),
        digest: None,
        stale,
        min_age_secs,
        min_size_bytes,
    }
}

fn store_and_config(plugins: &PluginRegistry) -> (tempfile::TempDir, MetaStore, Config) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let config = Config {
        data_dir: directory.path().to_path_buf(),
        ..Config::with_plugins(plugins)
    };
    (directory, meta, config)
}

fn write_invalid_blob_path(root: &std::path::Path) {
    let path = root.join("blobs/sha256/aa/bb/not-a-digest");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"x").unwrap();
}

fn plugins() -> PluginRegistry {
    registry(&PLUGIN)
}

fn plugins_without_cache() -> PluginRegistry {
    registry(&PLUGIN_WITHOUT_CACHE)
}

fn plugins_without_names() -> PluginRegistry {
    registry(&PLUGIN_WITHOUT_NAMES)
}

fn registry(plugin: &'static CachePlugin) -> PluginRegistry {
    PluginRegistry::new(vec![PluginRegistration {
        registration: plugin,
        config: plugin,
        runtime: plugin,
        distributed_runtime: Some(plugin),
        rate_limit_principal: None,
        client_discovery: Some(plugin),
        openapi: plugin,
        auth: None,
        browse: None,
        snippets: None,
        metadata_migration: None,
        operator_jobs: &[],
        priority: 1,
    }])
    .unwrap()
}

static PLUGIN: CachePlugin = CachePlugin {
    cache: true,
    names: true,
};
static PLUGIN_WITHOUT_CACHE: CachePlugin = CachePlugin {
    cache: false,
    names: true,
};
static PLUGIN_WITHOUT_NAMES: CachePlugin = CachePlugin {
    cache: true,
    names: false,
};
static DEFAULT_INDEXES: [DefaultIndex; 1] = [DefaultIndex {
    name: "main",
    route: "main",
    ecosystem: Ecosystem::new("core"),
    kind: DefaultIndexKind::Hosted,
}];

#[derive(Clone)]
struct CachePlugin {
    cache: bool,
    names: bool,
}

impl EcosystemRegistration for CachePlugin {
    fn ecosystem(&self) -> Ecosystem {
        CORE
    }

    fn default_indexes(&self) -> &'static [DefaultIndex] {
        &DEFAULT_INDEXES
    }

    fn absolute_prefixes(&self) -> &'static [&'static str] {
        &["/cache-fixture"]
    }

    fn driver(&self) -> ProtocolDriver {
        ProtocolDriver::Absolute(Arc::new(self.clone()))
    }

    fn register_capabilities(&self, registrar: &mut dyn CapabilityRegistrar) {
        if self.names {
            registrar.register_name(CORE, Arc::new(self.clone()));
        }
        if self.cache {
            registrar.register_cache(CORE, Arc::new(self.clone()));
        }
    }
}

impl EcosystemConfig for CachePlugin {
    fn compile_index_settings(&self, _: &str, _: &toml::Table) -> Result<Option<CompiledEcosystemSettings>, String> {
        Ok(Some(CompiledEcosystemSettings::new(CORE, ())))
    }
}

impl EcosystemRuntime for CachePlugin {
    fn install(
        &self,
        context: &mut RuntimeInstallContext<'_>,
        _: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        context.register_protocol(self.driver(), default_indexer());
        Ok(())
    }
}

impl peryx_driver::serving::DistributedRuntime for CachePlugin {
    fn install(
        &self,
        context: &mut DistributedInstallContext<'_>,
        _: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        context.runtime().register_protocol(self.driver(), default_indexer());
        Ok(())
    }
}

impl EcosystemOpenApi for CachePlugin {
    fn paths(&self, paths: PathsBuilder) -> PathsBuilder {
        paths.path("/cache-fixture", PathItem::new(HttpMethod::Get, Operation::new()))
    }
}

impl EcosystemDriver for CachePlugin {
    fn ecosystem(&self) -> Ecosystem {
        CORE
    }
}

impl peryx_driver::serving::ClientDiscovery for CachePlugin {
    fn discover_index(&self, _: IndexDescription, _: Option<&BaseUrl>) -> serde_json::Value {
        serde_json::Value::Null
    }

    fn client_endpoint(&self, route: &str) -> String {
        format!("/{route}/")
    }
}

#[test]
fn cache_plugin_client_discovery_builds_the_route_endpoint() {
    assert_eq!(
        peryx_driver::serving::ClientDiscovery::client_endpoint(&PLUGIN, "cache"),
        "/cache/"
    );
}

impl NameDriver for CachePlugin {
    fn normalize_name(&self, name: &str) -> String {
        name.to_lowercase()
    }
}

impl CacheDriver for CachePlugin {
    fn purge_resource(&self, _: &MetaStore, _: &str, resource: &str, _: bool) -> Result<PurgeReport, String> {
        Ok(PurgeReport {
            resource: resource.to_owned(),
            categories: vec![("resource_records".to_owned(), 2)],
        })
    }

    fn cache_pages(&self, _: &MetaStore, _: &[&str]) -> Result<Vec<CachePage>, String> {
        Ok(vec![
            CachePage {
                index: "main".to_owned(),
                resource: "widget".to_owned(),
                fetched_at_unix: i64::MAX,
                fresh_secs: Some(30),
                body_bytes: 4,
                record_bytes: 11,
                key: "main/widget".to_owned(),
            },
            CachePage {
                index: "main".to_owned(),
                resource: "gadget".to_owned(),
                fetched_at_unix: 0,
                fresh_secs: Some(0),
                body_bytes: 6,
                record_bytes: 13,
                key: "main/gadget".to_owned(),
            },
        ])
    }

    fn cache_record_counts(&self, _: &MetaStore) -> Result<Vec<(String, u64)>, String> {
        Ok(vec![("resource_records".to_owned(), 3)])
    }
}

#[async_trait::async_trait]
impl AbsoluteProtocolDriver for CachePlugin {
    fn prefixes(&self) -> &'static [&'static str] {
        &["/cache-fixture"]
    }

    fn classify_route(&self, _: &str) -> RouteClass {
        RouteClass::Metadata
    }

    async fn serve(&self, _: Arc<ServingState>, _: Request) -> Response {
        StatusCode::NOT_FOUND.into_response()
    }
}
