//! The OCI/Docker registry driver: the distribution-spec `/v2/` API served over peryx's
//! content-addressed blob store and metadata store.
//!
//! An OCI request is `/v2/<name>/(manifests|blobs|tags)/...`; `<name>` (which may contain slashes)
//! resolves to a configured `oci`-ecosystem index by longest route prefix, the same rule peryx
//! resolves any index route by. Blobs are `sha256`-addressed and map straight onto
//! [`peryx_storage::blob::BlobStorage`]; manifests are stored byte-for-byte so their digest is stable.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, Request};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::FutureExt as _;
use peryx_core::{Ecosystem, Lexicon};
use peryx_driver::AppState;
use peryx_driver::access::ReadAccess;
use peryx_driver::discovery::BaseUrl;
use peryx_driver::serving::{
    AuthInstallContext, BrowseDriver, CapabilityRegistrar, ClientDiscovery, CompiledEcosystemSettings,
    DistributedInstallContext, DistributedRuntime, EcosystemAuth, EcosystemBrowse, EcosystemConfig, EcosystemOpenApi,
    EcosystemRegistration, EcosystemRuntime, MirrorAction, MirrorDriver, MirrorRequest, PluginAuthConfig,
    ProtocolDriver, RateLimitPrincipal, RuntimeInstallContext,
};
use peryx_identity::Denial;

/// Stable identity of the OCI distribution ecosystem.
pub const ECOSYSTEM: Ecosystem = Ecosystem::new("oci");
const MIRROR_REPORT_HEADER: &str = "kind\tindex\tproject\tfilename\tdigest\turl\tbytes\tstatus\treason\n";

pub const DEFAULT_INDEXES: &[peryx_core::DefaultIndex] = &[
    peryx_core::DefaultIndex {
        name: "dockerhub",
        route: "dockerhub",
        ecosystem: ECOSYSTEM,
        kind: peryx_core::DefaultIndexKind::Cached {
            upstream: "https://registry-1.docker.io",
        },
    },
    peryx_core::DefaultIndex {
        name: "images",
        route: "images",
        ecosystem: ECOSYSTEM,
        kind: peryx_core::DefaultIndexKind::Hosted,
    },
    peryx_core::DefaultIndex {
        name: "root-oci",
        route: "root/oci",
        ecosystem: ECOSYSTEM,
        kind: peryx_core::DefaultIndexKind::Virtual {
            layers: &["images", "dockerhub"],
            write_target: "images",
        },
    },
];

#[derive(Debug, Clone, Copy, Default)]
pub struct OciPlugin;

/// The container ecosystem's user-facing words for peryx's neutral concepts.
pub const OCI_LEXICON: Lexicon = Lexicon {
    repository: "registry",
    resource: "repository",
    resources: "repositories",
    resource_kind: "image",
    group: "tag",
    groups: "tags",
    artifact: "blob",
    artifacts: "blobs",
    read: "pull",
    write: "push",
};

/// The audience named by this registry's Bearer challenges and tokens.
pub const TOKEN_SERVICE: &str = peryx_identity::TOKEN_AUDIENCE;

mod discovery;
mod error;
mod mirror;
mod name;
pub mod openapi;
mod outbox;
mod policy;
mod quota;
pub(crate) mod registry;
mod search_oci;
mod settings;
mod store;
mod upstream;
mod web;
mod webhook;

#[cfg(test)]
#[path = "../tests/unit/tests/mod.rs"]
mod tests;

pub use error::{ErrorCode, error_response, gateway_error};
pub use mirror::{MirrorMode, MirrorRow, mirror};
pub use quota::quota_reservation;
pub use registry::OciRegistry;
#[doc(hidden)]
pub use registry::OciRegistryWithHasher;
pub use search_oci::OciIndexer;
pub use settings::{IndexSettings, LibraryPrefix};
pub use store::referenced_blob_digests;

pub struct OciInstaller {
    settings: HashMap<String, IndexSettings>,
    journal_outbox: outbox::Outbox,
}

impl OciInstaller {
    pub fn new(settings: impl IntoIterator<Item = (String, IndexSettings)>, journal_outbox: outbox::Outbox) -> Self {
        Self {
            settings: settings.into_iter().collect(),
            journal_outbox,
        }
    }
}

impl OciInstaller {
    fn register_driver(&self, context: &mut RuntimeInstallContext<'_>) {
        if !context.has_ecosystem(&ECOSYSTEM) {
            return;
        }
        let driver = Arc::new(OciRegistry::new(
            self.settings.iter().map(|(name, settings)| (name.clone(), *settings)),
            self.journal_outbox,
        ));
        context.register_service(driver.clone());
        context.register_protocol(ProtocolDriver::Absolute(driver.clone()), Arc::new(OciIndexer));
        context.register_browse(ECOSYSTEM, driver.clone());
        context.register_idle_reclaimer(ECOSYSTEM, driver.clone());
        context.register_mirror(ECOSYSTEM, driver);
        context.register_lexicon(ECOSYSTEM, &OCI_LEXICON);
    }
}

impl EcosystemRegistration for OciPlugin {
    fn ecosystem(&self) -> Ecosystem {
        ECOSYSTEM
    }

    fn default_indexes(&self) -> &'static [peryx_core::DefaultIndex] {
        DEFAULT_INDEXES
    }

    fn absolute_prefixes(&self) -> &'static [&'static str] {
        registry::ABSOLUTE_PREFIXES
    }

    fn driver(&self) -> ProtocolDriver {
        ProtocolDriver::Absolute(Arc::new(OciRegistry::default()))
    }

    fn register_capabilities(&self, registrar: &mut dyn CapabilityRegistrar) {
        register_capabilities(registrar, Arc::new(OciRegistry::default()));
    }
}

impl EcosystemConfig for OciPlugin {
    fn compile_index_settings(
        &self,
        name: &str,
        settings: &toml::Table,
    ) -> Result<Option<CompiledEcosystemSettings>, String> {
        IndexSettings::compile(settings)
            .map(|settings| Some(CompiledEcosystemSettings::new(ECOSYSTEM, settings)))
            .map_err(|reason| format!("compile settings for {name}: {reason}"))
    }
}

impl EcosystemAuth for OciPlugin {
    fn validate(&self, config: PluginAuthConfig<'_>) -> Result<(), String> {
        if config.signing_key_configured
            && config.token_ttl_secs < 60
            && config.indexes.iter().any(|index| index.ecosystem == ECOSYSTEM)
        {
            return Err(
                "auth: `token_ttl_secs` must be at least 60 when a signing key and OCI index enable token authentication"
                    .to_owned(),
            );
        }
        Ok(())
    }

    fn install(&self, _context: &mut AuthInstallContext<'_>, _values: &toml::Table) -> Result<(), String> {
        Ok(())
    }
}

impl EcosystemRuntime for OciPlugin {
    fn install(
        &self,
        context: &mut RuntimeInstallContext<'_>,
        settings: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        install_compiled(context, settings, false)
    }
}

impl DistributedRuntime for OciPlugin {
    fn install(
        &self,
        context: &mut DistributedInstallContext<'_>,
        settings: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        install_compiled(context.runtime(), settings, true)
    }
}

impl RateLimitPrincipal for OciPlugin {
    fn resolve(
        &self,
        state: &peryx_driver::ServingState,
        _position: Option<usize>,
        headers: &axum::http::HeaderMap,
    ) -> peryx_identity::Principal {
        registry::rate_limit_principal(state, headers)
    }
}

impl ClientDiscovery for OciPlugin {
    fn discover_index(
        &self,
        index: peryx_driver::state::IndexDescription,
        base: Option<&peryx_driver::discovery::BaseUrl>,
    ) -> serde_json::Value {
        discovery::index_entry(index, base)
    }

    fn client_endpoint(&self, route: &str) -> String {
        let mut url = "/v2/".to_owned();
        peryx_core::url_encoding::push_path(&mut url, route);
        url.push('/');
        url
    }
}

#[async_trait::async_trait]
impl EcosystemBrowse for OciPlugin {
    fn paths(&self) -> &'static [&'static str] {
        &[
            "/+ui/browse",
            "/+ui/projects",
            "/+ui/project",
            "/+ui/manifest",
            "/+ui/members",
            "/+ui/member",
        ]
    }

    async fn dispatch(&self, state: Arc<AppState>, request: Request) -> Response {
        browse_http(state, request).boxed().await
    }
}

impl EcosystemOpenApi for OciPlugin {
    fn paths(&self, paths: utoipa::openapi::PathsBuilder) -> utoipa::openapi::PathsBuilder {
        openapi::openapi_paths(paths)
    }
}

#[must_use]
pub fn registration() -> peryx_plugin_registry::PluginRegistration {
    peryx_plugin_registry::PluginRegistration {
        registration: &OciPlugin,
        config: &OciPlugin,
        runtime: &OciPlugin,
        distributed_runtime: Some(&OciPlugin),
        rate_limit_principal: Some(&OciPlugin),
        client_discovery: Some(&OciPlugin),
        openapi: &OciPlugin,
        auth: Some(peryx_plugin_registry::PluginAuthRegistration::Shared(&OciPlugin)),
        browse: Some(&OciPlugin),
        snippets: None,
        metadata_migration: None,
        operator_jobs: &[],
        priority: 1,
    }
}

async fn browse_http(state: Arc<AppState>, request: Request) -> Response {
    let raw_query = request.uri().query().unwrap_or_default();
    let values = browse_values(raw_query);
    let Some(route) = values.get("index") else {
        return (StatusCode::BAD_REQUEST, "missing index").into_response();
    };
    let Some(position) = state
        .serving
        .indexes
        .iter()
        .position(|index| index.route == *route && index.ecosystem == ECOSYSTEM)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let query = match registry::BrowseQuery::parse(raw_query) {
        Ok(query) => query,
        Err(error) => return (StatusCode::BAD_REQUEST, error).into_response(),
    };
    let access = ReadAccess::from_headers(&state.serving, request.headers());
    let index_access = access.for_index(state.serving.index_at(position));
    if let Err(denial) = query.project.as_deref().map_or_else(
        || index_access.authorize_any_resource(),
        |project| index_access.authorize_resource(peryx_identity::ResourceMatch::Pattern(project)),
    ) {
        return browse_denial(denial);
    }
    let Some(driver) = state.serving.plugin_service::<OciRegistry>() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "OCI driver not installed").into_response();
    };
    let base = BaseUrl::from_request(
        request.headers(),
        request.uri(),
        request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .is_some_and(|ConnectInfo(address)| state.serving.rate_limits.trusts_proxy(address.ip())),
    );
    browse_resource_response(
        &state,
        driver,
        position,
        BrowseHttpRequest {
            path: request.uri().path(),
            query,
            raw_query,
            base: base.as_ref(),
            index_access: &index_access,
        },
    )
    .boxed()
    .await
    .unwrap_or_else(std::convert::identity)
}

struct BrowseHttpRequest<'a> {
    path: &'a str,
    query: registry::BrowseQuery,
    raw_query: &'a str,
    base: Option<&'a BaseUrl>,
    index_access: &'a peryx_driver::access::IndexReadAccess<'a>,
}

async fn browse_resource_response(
    state: &Arc<AppState>,
    driver: &OciRegistry,
    position: usize,
    request: BrowseHttpRequest<'_>,
) -> Result<Response, Response> {
    match request.path {
        "/+ui/browse" => driver
            .browse(
                state.serving.clone(),
                position,
                request.raw_query.to_owned(),
                request.base,
            )
            .await
            .map(|page| {
                page.map_or_else(
                    || StatusCode::NOT_FOUND.into_response(),
                    |page| Json(page).into_response(),
                )
            })
            .map_err(browse_error),
        "/+ui/projects" => registry::repositories(&state.serving, state.serving.index_at(position))
            .map(|mut repositories| {
                repositories.retain(|repository| {
                    request
                        .index_access
                        .authorize_resource(peryx_identity::ResourceMatch::Pattern(repository))
                        .is_ok()
                });
                Json(repositories).into_response()
            })
            .map_err(registry::ServeError::from)
            .map_err(browse_error),
        "/+ui/project" => {
            let Some(repository) = request.query.project else {
                return Ok((StatusCode::BAD_REQUEST, "missing project").into_response());
            };
            driver
                .repository_tags(&state.serving, state.serving.index_at(position), &repository)
                .await
                .map(|names| Json(web::RepositoryContent::References { names }).into_response())
                .map_err(browse_error)
        }
        "/+ui/manifest" => {
            let (Some(repository), Some(reference)) = (request.query.project, request.query.reference) else {
                return Ok((StatusCode::BAD_REQUEST, "missing project or ref").into_response());
            };
            driver
                .manifest_content(state.serving.clone(), position, repository, reference)
                .await
                .map(|manifest| {
                    manifest.map_or_else(
                        || StatusCode::NOT_FOUND.into_response(),
                        |manifest| Json(manifest).into_response(),
                    )
                })
                .map_err(browse_error)
        }
        "/+ui/members" => {
            let (Some(repository), Some(digest)) = (request.query.project, request.query.layer) else {
                return Ok((StatusCode::BAD_REQUEST, "missing project or digest").into_response());
            };
            driver
                .layer_members(state.serving.clone(), position, repository, digest)
                .boxed()
                .await
                .map(|members| Json(members).into_response())
                .map_err(browse_error)
        }
        "/+ui/member" => {
            let (Some(repository), Some(digest), Some(member)) =
                (request.query.project, request.query.layer, request.query.member)
            else {
                return Ok((StatusCode::BAD_REQUEST, "missing project, digest, or member").into_response());
            };
            driver
                .layer_member_chunk(
                    state.serving.clone(),
                    position,
                    repository,
                    digest,
                    member,
                    request.query.offset,
                )
                .boxed()
                .await
                .map(|chunk| Json(chunk).into_response())
                .map_err(browse_error)
        }
        _ => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

fn browse_values(raw_query: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(raw_query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

fn browse_denial(denial: Denial) -> Response {
    match denial {
        Denial::Forbidden => StatusCode::FORBIDDEN.into_response(),
        Denial::Unavailable | Denial::Unauthenticated => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx\"")],
        )
            .into_response(),
    }
}

fn browse_error(error: impl Into<String>) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, error.into()).into_response()
}

fn install_compiled(
    context: &mut RuntimeInstallContext<'_>,
    settings: &[(&str, &CompiledEcosystemSettings)],
    journal_outbox: outbox::Outbox,
) -> Result<(), String> {
    let mut compiled = Vec::with_capacity(settings.len());
    for (name, settings) in settings {
        let Some(settings) = settings.value::<IndexSettings>().copied() else {
            return Err(format!("compiled settings for {name} have the wrong type"));
        };
        compiled.push(((*name).to_owned(), settings));
    }
    OciInstaller::new(compiled, journal_outbox).register_driver(context);
    Ok(())
}

fn register_capabilities(registrar: &mut dyn CapabilityRegistrar, driver: Arc<OciRegistry>) {
    registrar.register_metrics(ECOSYSTEM, driver.clone());
    registrar.register_policy(ECOSYSTEM, driver.clone());
    registrar.register_blob_references(ECOSYSTEM, driver.clone());
    registrar.register_fsck(ECOSYSTEM, driver.clone());
    registrar.register_trash(ECOSYSTEM, driver.clone());
    registrar.register_browse(ECOSYSTEM, driver);
}

#[async_trait::async_trait]
impl<S: std::hash::BuildHasher + Send + Sync> MirrorDriver for registry::OciRegistryWithHasher<S> {
    fn validate_options(&self, configured: &toml::Table, overrides: &toml::Table) -> Result<(), String> {
        validate_mirror_options(configured, &["images", "packages"])?;
        validate_mirror_options(overrides, &["images"])
    }

    async fn mirror(
        &self,
        state: Arc<AppState>,
        request: MirrorRequest<'_>,
        output: &mut (dyn std::io::Write + Send),
    ) -> Result<(), String> {
        let index = state
            .serving
            .indexes
            .iter()
            .find(|index| index.name == request.index || index.route == request.index)
            .ok_or_else(|| format!("unknown OCI index {:?}", request.index))?;
        let mut images = table_strings(request.configured, "images")?;
        if images.is_empty() {
            images = table_strings(request.configured, "packages")?;
        }
        images.extend(table_strings(request.overrides, "images")?);
        if images.is_empty() {
            return Err(
                "mirroring an OCI index needs at least one image (--image or [index.prefetch] packages)".to_owned(),
            );
        }
        output
            .write_all(MIRROR_REPORT_HEADER.as_bytes())
            .map_err(error_message)?;
        let mode = match request.action {
            MirrorAction::Plan => {
                for image in &images {
                    write_mirror_row(output, &MirrorRow::selected(&index.name, image))?;
                }
                write_mirror_row(output, &MirrorRow::count(&index.name, "images", images.len() as u64))?;
                return Ok(());
            }
            MirrorAction::Sync => MirrorMode::Sync,
            MirrorAction::Verify => MirrorMode::Verify,
        };
        let settings = IndexSettings::compile(request.settings)?;
        let rows = mirror(&state.serving, index, settings, &images, mode)
            .boxed()
            .await
            .map_err(error_message)?;
        let mut errors = 0_u64;
        for row in rows {
            errors += u64::from(row.status == "error");
            write_mirror_row(output, &row)?;
        }
        if errors == 0 {
            Ok(())
        } else {
            Err(format!("mirror found {errors} error(s)"))
        }
    }
}

fn validate_mirror_options(options: &toml::Table, supported: &[&str]) -> Result<(), String> {
    if let Some(key) = options.keys().find(|key| !supported.contains(&key.as_str())) {
        return Err(format!("prefetch option {key:?} is not supported by oci"));
    }
    Ok(())
}

fn write_mirror_row(output: &mut (dyn std::io::Write + Send), row: &MirrorRow) -> Result<(), String> {
    writeln!(
        output,
        "{}\t{}\t{}\t{}\t{}\t\t{}\t{}\t{}",
        row.kind, row.index, row.repo, row.reference, row.digest, row.bytes, row.status, row.reason
    )
    .map_err(error_message)
}

fn table_strings(table: &toml::Table, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = table.get(key) else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err(format!("{key} must be an array"));
    };
    let mut strings = Vec::with_capacity(values.len());
    for value in values {
        let Some(value) = value.as_str() else {
            return Err(format!("{key} entries must be strings"));
        };
        strings.push(value.to_owned());
    }
    Ok(strings)
}

fn error_message(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(feature = "bench")]
pub mod bench;
mod upload_session;
