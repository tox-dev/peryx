#![recursion_limit = "152"]

#[cfg(feature = "serving")]
use std::sync::Arc;

use peryx_core::{Ecosystem, Lexicon};
#[cfg(feature = "serving")]
use peryx_driver::serving::{
    AuthInstallContext, CapabilityRegistrar, ClientDiscovery, CompiledEcosystemSettings, DistributedInstallContext,
    DistributedRuntime, EcosystemAuth, EcosystemBrowse, EcosystemConfig, EcosystemOpenApi, EcosystemRegistration,
    EcosystemRuntime, EcosystemSnippet, ProtocolDriver, RateLimitPrincipal, RuntimeInstallContext,
};

/// Stable identity of the Python package ecosystem.
pub const ECOSYSTEM: Ecosystem = Ecosystem::new("pypi");

pub const PYPI_LEXICON: Lexicon = Lexicon {
    repository: "index",
    resource: "project",
    resources: "projects",
    resource_kind: "package",
    group: "version",
    groups: "versions",
    artifact: "file",
    artifacts: "files",
    read: "download",
    write: "upload",
};

pub const DEFAULT_INDEXES: &[peryx_core::DefaultIndex] = &[
    peryx_core::DefaultIndex {
        name: "pypi",
        route: "pypi",
        ecosystem: ECOSYSTEM,
        kind: peryx_core::DefaultIndexKind::Cached {
            upstream: "https://pypi.org/simple/",
        },
    },
    peryx_core::DefaultIndex {
        name: "hosted",
        route: "hosted",
        ecosystem: ECOSYSTEM,
        kind: peryx_core::DefaultIndexKind::Hosted,
    },
    peryx_core::DefaultIndex {
        name: "root-pypi",
        route: "root/pypi",
        ecosystem: ECOSYSTEM,
        kind: peryx_core::DefaultIndexKind::Virtual {
            layers: &["hosted", "pypi"],
            write_target: "hosted",
        },
    },
];

#[derive(Debug, Clone, Copy, Default)]
pub struct PypiPlugin;

#[cfg(feature = "serving")]
mod admin;
#[cfg(feature = "serving")]
pub mod archive;
#[cfg(feature = "serving")]
pub mod attestation;
#[cfg(feature = "bench")]
pub mod bench;
#[cfg(feature = "serving")]
pub mod cache;
#[cfg(feature = "serving")]
pub mod catalog;
#[cfg(feature = "serving")]
mod catalog_job;
mod changelog;
#[cfg(feature = "serving")]
mod classifier;
#[cfg(feature = "serving")]
mod contact;
#[cfg(feature = "serving")]
mod description;
#[cfg(feature = "serving")]
pub mod discovery;
mod filename;
mod html;
#[cfg(feature = "serving")]
mod import;
mod legacy_json;
#[cfg(feature = "serving")]
mod license;
mod metadata;
#[cfg(feature = "serving")]
mod migration;
#[cfg(feature = "serving")]
mod mirror;
mod name;
#[cfg(feature = "serving")]
pub mod openapi;
#[cfg(feature = "serving")]
pub mod policy;
#[cfg(feature = "serving")]
mod quota;
#[cfg(feature = "serving")]
mod requirement;
#[cfg(feature = "serving")]
pub mod retention;
#[cfg(feature = "serving")]
pub mod search_pypi;
mod serial;
#[cfg(feature = "serving")]
pub mod serving;
#[cfg(feature = "serving")]
mod shadow;
mod simple;
#[cfg(feature = "serving")]
mod simple_client;
#[cfg(feature = "serving")]
mod source_policy;
#[cfg(feature = "serving")]
pub mod store;
#[cfg(feature = "serving")]
pub mod stream;
#[cfg(feature = "serving")]
mod sync_lock;
#[cfg(feature = "serving")]
pub mod trash;
#[cfg(feature = "serving")]
mod trusted_publishing;
#[cfg(feature = "serving")]
pub mod upload;
mod version;
pub mod view;
#[cfg(feature = "serving")]
mod webhook;

#[cfg(feature = "serving")]
pub use catalog_job::{
    default_concurrency as default_job_concurrency, default_project_limit as default_job_project_limit,
    default_timeout_secs as default_job_timeout_secs, scheduled_from_options as scheduled_job_from_options,
};
#[cfg(feature = "serving")]
pub use quota::quota_reservation;
#[cfg(feature = "serving")]
pub use retention::evaluate_retention;
#[cfg(feature = "serving")]
pub use search_pypi::PypiIndexer;
#[cfg(feature = "serving")]
pub use serving::PypiServing;
#[cfg(feature = "serving")]
pub use serving::finalize::{
    Finalization, FinalizeDescriptor, FinalizeError, FinalizeFailure, finalize_admitted_upload,
};
#[cfg(feature = "serving")]
pub use simple_client::{
    ACCEPT_SIMPLE, CachedValidators, SimpleClientExt, SimpleHead, SimpleResponse, UpstreamProtocol,
};

pub use changelog::{
    CHANGELOG_PAGE_SIZE, ChangelogEntry, ChangelogRequest, ChangelogRequestError, dispatch_changelog_request,
    parse_changelog_request, render_changelog_fault, render_changelog_response, render_last_serial_response,
};
pub use filename::{
    DistributionFilename, DistributionFilenameError, DistributionKind, distribution_name_segment,
    distribution_version_segment, parse_distribution_filename,
};
pub use html::{parse_detail_html, parse_index_html};
pub use legacy_json::render_legacy_json;
#[cfg(feature = "serving")]
pub use metadata::ui_project_from_detail;
pub use metadata::{CoreMetadataDoc, MetadataError, parse_metadata, ui_meta};
pub use name::{
    PackageName, authority_key, file_matches_version, is_valid_name, normalize_name, normalize_name_cow,
    project_of_filename,
};
pub use serial::{
    ChangelogPage, ChangelogPageError, SerialStamp, UpstreamSerialError, compose_serial_watermarks,
    validate_upstream_serial,
};
pub use simple::{
    API_VERSION, API_VERSION_BASE, CoreMetadata, DetailSink, File, Meta, ParsedDetail, ProjectDetail, ProjectList,
    ProjectListEntry, ProjectStatus, Provenance, SimpleError, StreamedDetail, Yanked, parse_detail, parse_index,
    parse_meta, render_detail_html, render_index_html, stream_detail_json, to_json,
};
pub use version::{Version, VersionSpecifiers, parse_version, parse_version_specifiers, sorted_desc, versions_match};

#[cfg(feature = "serving")]
#[must_use]
pub fn trusted_publishing_enabled(state: &peryx_driver::AppState) -> bool {
    trusted_publishing::enabled(&state.serving)
}

#[cfg(feature = "serving")]
impl EcosystemRegistration for PypiPlugin {
    fn ecosystem(&self) -> Ecosystem {
        ECOSYSTEM
    }

    fn default_indexes(&self) -> &'static [peryx_core::DefaultIndex] {
        DEFAULT_INDEXES
    }

    fn absolute_prefixes(&self) -> &'static [&'static str] {
        &[]
    }

    fn webhook_events(&self) -> &'static [&'static str] {
        webhook::EVENTS
    }

    fn driver(&self) -> ProtocolDriver {
        ProtocolDriver::Indexed(Arc::new(PypiServing))
    }

    fn register_capabilities(&self, registrar: &mut dyn CapabilityRegistrar) {
        register_capabilities(registrar, Arc::new(PypiServing));
    }
}

#[cfg(feature = "serving")]
impl EcosystemAuth for PypiPlugin {
    fn validate(&self, config: peryx_driver::serving::PluginAuthConfig<'_>) -> Result<(), String> {
        trusted_publishing::validate(config)
    }

    fn install(&self, context: &mut AuthInstallContext<'_>, values: &toml::Table) -> Result<(), String> {
        trusted_publishing::install(context, values)
    }
}

#[cfg(feature = "serving")]
impl EcosystemConfig for PypiPlugin {
    fn compile_index_settings(
        &self,
        name: &str,
        settings: &toml::Table,
    ) -> Result<Option<CompiledEcosystemSettings>, String> {
        settings.keys().next().map_or(Ok(None), |key| {
            Err(format!(
                "compile settings for {name}: unknown field `{key}` in `[index.settings]`"
            ))
        })
    }
}

#[cfg(feature = "serving")]
impl EcosystemRuntime for PypiPlugin {
    fn install(
        &self,
        context: &mut RuntimeInstallContext<'_>,
        _: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        install_runtime(context, false, Arc::new(PypiServing));
        Ok(())
    }
}

#[cfg(feature = "serving")]
impl DistributedRuntime for PypiPlugin {
    fn install(
        &self,
        context: &mut DistributedInstallContext<'_>,
        _: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        let driver = Arc::new(PypiServing);
        install_runtime(context.runtime(), true, driver.clone());
        context.register_replicated_apply(ECOSYSTEM, driver);
        Ok(())
    }
}

#[cfg(feature = "serving")]
fn install_runtime(context: &mut RuntimeInstallContext<'_>, distributed: bool, driver: Arc<PypiServing>) {
    cache::install_runtime_services(context);
    if distributed {
        context.register_service(Arc::new(PypiDistributedRuntime));
    }
    context.register_protocol(ProtocolDriver::Indexed(driver.clone()), Arc::new(PypiIndexer));
    context.register_intent_finalizer(ECOSYSTEM, driver.clone());
    context.register_cache_refresher(ECOSYSTEM, driver.clone());
    context.register_mirror(ECOSYSTEM, driver);
    context.register_lexicon(ECOSYSTEM, &PYPI_LEXICON);
    context.register_routes(Arc::new(shadow::ShadowRoutes));
}

#[cfg(feature = "serving")]
fn register_capabilities(registrar: &mut dyn CapabilityRegistrar, driver: Arc<PypiServing>) {
    registrar.register_job(ECOSYSTEM, driver.clone());
    registrar.register_metrics(ECOSYSTEM, driver.clone());
    registrar.register_name(ECOSYSTEM, driver.clone());
    registrar.register_policy(ECOSYSTEM, driver.clone());
    registrar.register_policy_dry_run(ECOSYSTEM, driver.clone());
    registrar.register_blob_references(ECOSYSTEM, driver.clone());
    registrar.register_fsck(ECOSYSTEM, driver.clone());
    registrar.register_metadata_repair(ECOSYSTEM, driver.clone());
    registrar.register_retention(ECOSYSTEM, driver.clone());
    registrar.register_cache(ECOSYSTEM, driver.clone());
    registrar.register_cache_inspect(ECOSYSTEM, driver.clone());
    registrar.register_cache_purge(ECOSYSTEM, driver.clone());
    registrar.register_index_summary(ECOSYSTEM, driver.clone());
    registrar.register_trash(ECOSYSTEM, driver.clone());
    registrar.register_import(ECOSYSTEM, driver.clone());
    registrar.register_service(ECOSYSTEM, driver.clone());
    registrar.register_browse(ECOSYSTEM, driver.clone());
    registrar.register_index_credentials(ECOSYSTEM, driver);
}

#[cfg(feature = "serving")]
struct PypiDistributedRuntime;

#[cfg(feature = "serving")]
pub(crate) fn replication_enabled(state: &peryx_driver::ServingState) -> bool {
    state.plugin_service::<PypiDistributedRuntime>().is_some()
}

#[cfg(feature = "serving")]
impl RateLimitPrincipal for PypiPlugin {
    fn resolve(
        &self,
        state: &peryx_driver::ServingState,
        position: Option<usize>,
        headers: &axum::http::HeaderMap,
    ) -> peryx_identity::Principal {
        serving::rate_limit_principal(state, position, headers)
    }
}

#[cfg(feature = "serving")]
impl ClientDiscovery for PypiPlugin {
    fn discover_index(
        &self,
        index: peryx_driver::state::IndexDescription,
        base: Option<&peryx_driver::discovery::BaseUrl>,
    ) -> serde_json::Value {
        discovery::index_entry(index, base)
    }

    fn client_endpoint(&self, route: &str) -> String {
        let mut url = String::with_capacity(route.len() + 9);
        url.push('/');
        peryx_core::url_encoding::push_path(&mut url, route);
        url.push_str("/simple/");
        url
    }
}

#[cfg(feature = "serving")]
#[async_trait::async_trait]
impl EcosystemBrowse for PypiPlugin {
    fn paths(&self) -> &'static [&'static str] {
        serving::BROWSE_PATHS
    }

    async fn dispatch(
        &self,
        state: Arc<peryx_driver::AppState>,
        request: axum::extract::Request,
    ) -> axum::response::Response {
        serving::browse_http(state, request).await
    }
}

#[cfg(feature = "serving")]
impl EcosystemOpenApi for PypiPlugin {
    fn paths(
        &self,
        paths: utoipa::openapi::PathsBuilder,
        reads: peryx_driver::route_auth::ReadExposure,
    ) -> utoipa::openapi::PathsBuilder {
        openapi::openapi_paths(paths, reads)
    }
}

#[cfg(feature = "serving")]
impl EcosystemSnippet for PypiPlugin {
    fn text(
        &self,
        base: &peryx_driver::discovery::BaseUrl,
        route: &str,
        uploads: bool,
        format: &str,
    ) -> Result<Option<String>, String> {
        let kind = match format {
            "pip.conf" => discovery::SnippetKind::PipConf,
            "uv.toml" => discovery::SnippetKind::UvToml,
            ".pypirc" => discovery::SnippetKind::Pypirc,
            _ => return Err(format!("unknown snippet format {format:?}")),
        };
        Ok(discovery::snippet_text(base, route, uploads, kind))
    }
}

#[cfg(feature = "serving")]
static OPERATOR_JOBS: [&dyn peryx_plugin_registry::OperatorJob; 1] = [&catalog_job::OPERATOR_JOB];

#[cfg(feature = "serving")]
#[must_use]
pub fn registration() -> peryx_plugin_registry::PluginRegistration {
    peryx_plugin_registry::PluginRegistration {
        registration: &PypiPlugin,
        config: &PypiPlugin,
        runtime: &PypiPlugin,
        distributed_runtime: Some(&PypiPlugin),
        rate_limit_principal: Some(&PypiPlugin),
        client_discovery: Some(&PypiPlugin),
        openapi: &PypiPlugin,
        auth: Some(peryx_plugin_registry::PluginAuthRegistration::Extension {
            auth: &PypiPlugin,
            fields: trusted_publishing::AUTH_FIELDS,
            defaults: trusted_publishing::auth_defaults,
        }),
        browse: Some(&PypiPlugin),
        snippets: Some(&PypiPlugin),
        metadata_migration: Some(Arc::new(PypiPlugin)),
        operator_jobs: &OPERATOR_JOBS,
        priority: 0,
    }
}

#[cfg(test)]
#[cfg(feature = "serving")]
#[path = "../tests/unit/plugin_contract_tests.rs"]
mod plugin_contract_tests;

/// Render any error as the user-visible message a driver method returns, so the many `?`-adjacent
/// store and io failures map through one function instead of a per-site `|err| err.to_string()`
/// closure that never runs in the happy path.
#[cfg(feature = "serving")]
pub(crate) fn error_message<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

#[cfg(test)]
#[cfg(feature = "serving")]
#[path = "../tests/unit/error_message_tests.rs"]
mod error_message_tests;

#[cfg(test)]
#[cfg(feature = "serving")]
#[path = "../tests/unit/tests/mod.rs"]
mod tests;
