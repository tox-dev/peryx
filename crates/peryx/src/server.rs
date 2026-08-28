use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read as _;
use std::sync::Arc;

use anyhow::{Context as _, bail, ensure};
use axum::Router;
use peryx_core::path;
use peryx_driver::state::RuntimeOptions;
use peryx_driver::{AppState, Index, IndexKind};
use peryx_events::webhook::{WebhookRuntime, WebhookTargetConfig};
use peryx_http::router;
use peryx_identity::{
    Action, LdapBindMode, LdapLoginService, LdapProvider, LdapProviderSettings, OidcLoginProvider, OidcLoginService,
    OidcProviderSettings, SessionSealer, Signer,
};
use peryx_policy::{Policy, PolicyCapabilities, PolicyDecisionRecorder, PolicyEvaluation};
use peryx_storage::blob::{BlobStorage, S3Config};
use peryx_storage::meta::{MetaStore, NewPolicyDecision};
use peryx_upstream::{
    Auth, CredentialError, CredentialFailure, CredentialProvider, CredentialRefresh, CredentialScope,
    ExecCredentialConfig, NamedUpstream, Netrc, UpstreamClient, UpstreamRouter, UpstreamTls, redact_url,
};

use crate::config::{
    AuthConfig, BlobStorageConfig, Config, CredentialFailureMode, CredentialRefreshConfig, IndexConfig,
    IndexKind as ConfigKind, LdapBindConfig, LdapProviderConfig, OidcProviderConfig, SecretSource, UpstreamTlsConfig,
    WebhookSecret,
};

/// The derived views a read must not outrun. A replica gates reads on whole-blob availability as well
/// as the search view, so its readable frontier holds at the slower of the metadata and blob views;
/// every other role gates on the search view alone.
fn required_views(config: &Config) -> Arc<[&'static str]> {
    let mut views = vec![peryx_driver::state::SEARCH_VIEW];
    if config.availability.is_replica_mode() {
        views.push(peryx_ha::AVAILABILITY_BLOB_VIEW);
    }
    views.into()
}

/// # Errors
/// Returns an error when the S3 backend configuration is invalid.
fn build_blob_storage(config: &Config) -> anyhow::Result<BlobStorage> {
    match &config.blob {
        BlobStorageConfig::Filesystem => Ok(BlobStorage::filesystem(config.data_dir.join("blobs"))),
        BlobStorageConfig::S3(s3) => {
            let s3_config = S3Config::new(s3.into()).context("build S3 blob backend configuration")?;
            Ok(BlobStorage::s3(s3_config, config.data_dir.join("blob-staging")))
        }
    }
}

/// Build the peryx router from configuration.
///
/// Opens the stores under the data directory and resolves the configured indexes (cached indexes, hosted
/// stores, and virtual indexes) into their runtime form. Does not bind a socket, so it is testable in
/// isolation.
///
/// # Errors
/// Returns an error if the data directory or stores cannot be opened, an upstream URL is invalid, or
/// a virtual index references an unknown or non-hosted index.
pub fn build_router(config: &Config) -> anyhow::Result<Router> {
    build_router_with_plugins(config, &crate::compiled_plugins())
}

/// # Errors
/// Returns an error if stores, indexes, upstreams, or distributed runtime configuration cannot be assembled.
pub fn build_router_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<Router> {
    let state = build_state_with_plugins(config, plugins)?;
    match config.availability {
        crate::config::AvailabilityConfig::None => Ok(router_for(state)),
        crate::config::AvailabilityConfig::Dc(_) | crate::config::AvailabilityConfig::Ha(_) => {
            let runtime = crate::replication::ReplicationRuntime::new(config, &state)?;
            Ok(runtime.mount(router_for(state)))
        }
    }
}

/// Validate a fully resolved configuration the way `serve` would accept it.
///
/// It opens no metadata store, binds no socket, and reaches no upstream: it runs the cross-field
/// [`Config::validate`] rules, the logging-sink check, and the full index assembly - topology,
/// policy compilation, secret reads, and upstream-client construction - so an operator can confirm a
/// config before a restart the way `nginx -t` confirms a server block.
///
/// # Errors
/// Returns the first configuration error the server would hit while assembling its state.
pub fn check_config(config: &Config) -> anyhow::Result<()> {
    check_config_with_plugins(config, &crate::compiled_plugins())
}

/// # Errors
/// Returns the first validation or assembly error in the supplied configuration.
pub fn check_config_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<()> {
    let plugins = activate_plugins(config, plugins)?;
    check_config_with_active_plugins(config, &plugins)
}

pub(crate) fn activate_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<peryx_plugin_registry::PluginRegistry> {
    plugins
        .activate(config.indexes.iter().map(|index| index.ecosystem.clone()))
        .context("activate configured ecosystems")
}

pub(crate) fn check_config_with_active_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<()> {
    config
        .validate_with_plugins(plugins)
        .context("validate configuration")?;
    resolve_signing_key(config)?;
    crate::logging::validate(&config.log).context("validate logging configuration")?;
    build_indexes_with_plugins(&config.indexes, &config.auth, config.offline, plugins)?;
    build_index_settings_with_plugins(&config.indexes, plugins)?;
    build_webhooks(&config.indexes)?;
    Ok(())
}

/// Open the stores and resolve the configured indexes into the shared application state, without
/// building routes, so the serve entrypoint can reach the upstream clients before traffic.
///
/// # Errors
/// Returns an error if the data directory or stores cannot be opened, an upstream URL is invalid,
/// or a virtual index references an unknown or non-hosted index.
pub fn build_state(config: &Config) -> anyhow::Result<Arc<AppState>> {
    build_state_with_plugins(config, &crate::compiled_plugins())
}

/// # Errors
/// Returns an error if validation fails or the configured stores, indexes, upstreams, or webhooks cannot be built.
pub fn build_state_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<Arc<AppState>> {
    let plugins = activate_plugins(config, plugins)?;
    build_state_with_active_plugins(config, &plugins)
}

pub(crate) fn build_state_with_active_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<Arc<AppState>> {
    build_state_with_active_backend_and_plugins(config, plugins)
}

fn build_state_with_active_backend_and_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<Arc<AppState>> {
    config
        .validate_with_plugins(plugins)
        .context("validate configuration")?;
    let signing_key = resolve_signing_key(config)?;
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("create data directory {}", config.data_dir.display()))?;
    let configured_replica = config.availability.is_replica_mode();
    let read_only = config.read_only || configured_replica;
    let meta_path = config.data_dir.join("peryx.redb");
    let meta = crate::metadata::open(&meta_path, plugins)?;
    if !read_only {
        loop {
            let report = meta
                .repair_abandoned_quota_reservations(i64::MAX, peryx_driver::jobs::QUOTA_REPAIR_BATCH)
                .context("repair abandoned quota reservations")?;
            if report.remaining == 0 {
                break;
            }
        }
    }
    if config.availability.replication().is_some() {
        if read_only {
            let active = meta.writer_identity().context("read metadata store writer identity")?;
            ensure!(
                active.as_deref() == config.writer_identity.as_deref(),
                "configured replica writer {:?} does not match metadata store writer {active:?}",
                config.writer_identity
            );
        } else if let Some(identity) = &config.writer_identity {
            meta.claim_writer_identity(identity)
                .with_context(|| format!("claim writer identity {identity:?}"))?;
        }
    }
    let blobs = build_blob_storage(config)?;
    let configs = replica_adjusted_configs(config, configured_replica);
    let netrc = config
        .netrc
        .as_deref()
        .map(Netrc::from_path)
        .transpose()
        .context("load upstream netrc")?;
    let credential_providers = build_credential_providers(&configs, netrc.as_ref())?;
    // A replica serves only its replicated cache, so it builds no upstream routes to fetch through.
    let upstream_routes = if configured_replica {
        Vec::new()
    } else {
        build_upstream_routes(&configs, &credential_providers, netrc.as_ref(), plugins)?
    };
    let mut indexes = build_indexes_with_providers(
        &configs,
        &config.auth,
        config.offline || read_only,
        &credential_providers,
        plugins,
    )?;
    if configured_replica {
        for index in &mut indexes {
            if let IndexKind::Virtual { write_target, .. } = &mut index.kind {
                *write_target = None;
            }
        }
    }
    attach_policy_decision_recorders(&meta, &mut indexes)?;
    if !read_only {
        crate::config::reconcile_configured_repositories(&meta, &configs);
    }
    let ecosystem_settings = build_index_settings_with_plugins(&configs, plugins)?;
    let webhooks = build_webhooks(&configs)?;
    let search_path = config.data_dir.join("search-v1");
    let mut state = AppState::with_search_path_and_runtime(
        meta,
        blobs,
        config.cache_ttl_secs,
        indexes,
        &search_path,
        RuntimeOptions {
            rate_limit: config.rate_limit.clone(),
            upstream_concurrency: upstream_concurrency(&config.indexes),
            upstream_routes,
            webhooks,
            hot_cache_bytes: config.hot_cache_bytes,
            max_stale_secs: config.max_stale_secs,
            usage_retention_days: config.usage_retention_days,
            required_views: required_views(config),
        },
    )
    .context(format!("open search index {}", search_path.display()))?;
    configure_state(
        &mut state,
        config,
        signing_key.as_deref(),
        &ecosystem_settings,
        read_only,
        plugins,
    )?;
    Ok(Arc::new(state))
}

fn resolve_signing_key(config: &Config) -> anyhow::Result<Option<String>> {
    const MIN_BYTES: usize = 32;
    let Some(source) = &config.auth.signing_key else {
        return Ok(None);
    };
    let key = source.read().context("read `auth.signing_key`")?;
    ensure!(!key.trim().is_empty(), "`auth.signing_key` must not be empty");
    ensure!(
        key.len() >= MIN_BYTES,
        "`auth.signing_key` must contain at least {MIN_BYTES} bytes"
    );
    Ok(Some(key))
}

fn configure_state(
    state: &mut AppState,
    config: &Config,
    signing_key: Option<&str>,
    ecosystem_settings: &HashMap<String, peryx_driver::serving::CompiledEcosystemSettings>,
    read_only: bool,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<()> {
    state
        .set_read_only(read_only)
        .map_err(anyhow::Error::msg)
        .context("configure read-only state")?;
    state
        .set_read_only_retry_after(match config.availability.replication() {
            Some(crate::config::ReplicationConfig::Replica { poll_interval, .. }) => Some(*poll_interval),
            Some(crate::config::ReplicationConfig::Primary { .. }) | None => None,
        })
        .map_err(anyhow::Error::msg)
        .context("configure read-only retry interval")?;
    plugins.register_activated_capabilities(&mut state.capability_install_context());
    match config.availability {
        crate::config::AvailabilityConfig::None => plugins.install_drivers(
            &mut state
                .runtime_install_context()
                .map_err(anyhow::Error::msg)
                .context("create local runtime install context")?,
            ecosystem_settings,
        ),
        crate::config::AvailabilityConfig::Dc(_) | crate::config::AvailabilityConfig::Ha(_) => plugins
            .install_distributed_drivers(
                &mut state
                    .distributed_install_context()
                    .map_err(anyhow::Error::msg)
                    .context("create distributed runtime install context")?,
                ecosystem_settings,
            ),
    }
    .map_err(anyhow::Error::msg)
    .context("install ecosystem runtime services")?;
    configure_availability(state, config, read_only)?;
    state
        .set_ldap_logins(ldap_logins(&config.auth.ldap_providers, &state.serving.meta)?)
        .map_err(anyhow::Error::msg)
        .context("install LDAP login services")?;
    state
        .set_oidc_logins(oidc_logins(&config.auth.oidc_providers, &state.serving.meta)?)
        .map_err(anyhow::Error::msg)
        .context("install OIDC login services")?;
    if let Some(key) = signing_key {
        state
            .set_session_sealer(SessionSealer::new(key.as_bytes()))
            .map_err(anyhow::Error::msg)
            .context("install session sealer")?;
        state
            .set_token_realm(
                Signer::new(key.as_bytes(), peryx_identity::TOKEN_AUDIENCE),
                config.auth.token_ttl_secs,
            )
            .map_err(anyhow::Error::msg)
            .context("install token realm")?;
    }
    plugins
        .install_auth_extensions(
            &mut state
                .auth_install_context()
                .map_err(anyhow::Error::msg)
                .context("create authentication install context")?,
            &config.auth.extensions,
        )
        .map_err(anyhow::Error::msg)
        .context("install ecosystem authentication extensions")?;
    state.set_openapi(crate::api::openapi_json_for_with_plugins(
        config.availability.mode().availability_resources(),
        plugins,
    ));
    Ok(())
}

fn configure_availability(state: &mut AppState, config: &Config, read_only: bool) -> anyhow::Result<()> {
    let Some(runtime) = crate::replication::runtime_config(config)? else {
        return Ok(());
    };
    peryx_ha_distributed::install_services(
        &peryx_ha_distributed::DistributedServiceConfig {
            runtime,
            read_only,
            write_ack_policy: config.write_ack.policy,
            write_ack_deadline: config.write_ack.deadline,
        },
        state,
    )
}

/// Close attempts interrupted by a prior process before this writer serves management traffic.
///
/// # Errors
/// Returns a metadata error when interrupted attempts cannot be read or updated.
pub fn recover_job_attempts(state: &AppState) -> Result<usize, peryx_storage::meta::MetaError> {
    if state.serving.read_only {
        Ok(0)
    } else {
        state.serving.job_attempts.recover_interrupted((state.serving.clock)())
    }
}

fn ldap_logins(configs: &[LdapProviderConfig], meta: &MetaStore) -> anyhow::Result<Vec<LdapLoginService<MetaStore>>> {
    configs
        .iter()
        .map(|config| {
            let bind = match &config.bind {
                LdapBindConfig::Direct { dn_attribute } => LdapBindMode::Direct {
                    dn_attribute: dn_attribute.clone(),
                },
                LdapBindConfig::Search {
                    username_attribute,
                    bind_dn,
                    bind_password,
                } => LdapBindMode::Search {
                    username_attribute: username_attribute.clone(),
                    bind_dn: bind_dn.clone(),
                    bind_password: bind_password
                        .read()
                        .with_context(|| format!("read LDAP provider {} bind password", config.id))?,
                },
            };
            let custom_ca_pem = config
                .ca_file
                .as_deref()
                .map(|path| read_ldap_ca(path).with_context(|| format!("read LDAP provider {} CA", config.id)))
                .transpose()?;
            let provider = LdapProvider::new(LdapProviderSettings {
                id: config.id.clone(),
                url: config.url.clone(),
                base_dn: config.base_dn.clone(),
                bind,
                subject_attribute: config.subject_attribute.clone(),
                display_name_attribute: config.display_name_attribute.clone(),
                group_attribute: config.group_attribute.clone(),
                custom_ca_pem,
                connect_timeout: config.connect_timeout,
                request_timeout: config.request_timeout,
                max_connections: config.max_connections,
            })
            .with_context(|| format!("configure LDAP provider {}", config.id))?;
            Ok(LdapLoginService::new(
                provider,
                meta.clone(),
                config.group_mappings.clone(),
            ))
        })
        .collect()
}

fn oidc_logins(configs: &[OidcProviderConfig], meta: &MetaStore) -> anyhow::Result<Vec<OidcLoginService<MetaStore>>> {
    configs
        .iter()
        .map(|config| {
            let client_secret = config
                .client_secret
                .as_ref()
                .map(|source| {
                    source
                        .read()
                        .with_context(|| format!("read OIDC provider {} client secret", config.id))
                })
                .transpose()?;
            let provider = OidcLoginProvider::new(OidcProviderSettings {
                id: config.id.clone(),
                issuer: config.issuer.clone(),
                client_id: config.client_id.clone(),
                client_secret,
                redirect_uri: config.redirect_uri.clone(),
                scopes: config.scopes.clone(),
                subject_claim: config.subject_claim.clone(),
                display_name_claim: config.display_name_claim.clone(),
                groups_claim: config.groups_claim.clone(),
                clock_skew: config.clock_skew,
                request_timeout: config.request_timeout,
            })
            .with_context(|| format!("configure OIDC provider {}", config.id))?;
            Ok(OidcLoginService::new(
                provider,
                meta.clone(),
                config.group_mappings.clone(),
            ))
        })
        .collect()
}

fn read_ldap_ca(path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    const MAX_CA_BYTES: u64 = 1 << 20;

    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_CA_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_CA_BYTES, "CA file exceeds 1048576 bytes");
    Ok(bytes)
}

#[derive(Debug)]
struct StoredPolicyDecisionRecorder {
    meta: MetaStore,
    repository: String,
}

impl PolicyDecisionRecorder for StoredPolicyDecisionRecorder {
    fn record(&self, evaluation: PolicyEvaluation<'_>) {
        if let Err(error) = self.meta.record_policy_decision(NewPolicyDecision {
            repository: &self.repository,
            resource: evaluation.resource,
            group: evaluation.group,
            artifact: evaluation.artifact,
            source: evaluation.source,
            action: evaluation.action,
            state: evaluation.state,
            rule: evaluation.rule,
            reason: evaluation.reason,
            evaluated_at_unix: time::OffsetDateTime::now_utc().unix_timestamp(),
            next_eligible_at_unix: evaluation.next_eligible_at_unix,
        }) {
            tracing::error!(repository = self.repository, %error, "failed to record policy decision");
        }
    }
}

fn attach_policy_decision_recorders(meta: &MetaStore, indexes: &mut [Index]) -> anyhow::Result<()> {
    for index in indexes {
        meta.advance_policy_generation(&index.name)
            .context(format!("advance policy generation for {}", index.name))?;
        index.policy =
            std::mem::take(&mut index.policy).with_decision_recorder(Arc::new(StoredPolicyDecisionRecorder {
                meta: meta.clone(),
                repository: index.name.clone(),
            }));
    }
    Ok(())
}

fn replica_adjusted_configs(config: &Config, configured_replica: bool) -> Cow<'_, [IndexConfig]> {
    if configured_replica {
        let mut configs = config.indexes.clone();
        make_replica_configs(&mut configs);
        Cow::Owned(configs)
    } else {
        Cow::Borrowed(config.indexes.as_slice())
    }
}

fn make_replica_configs(configs: &mut [IndexConfig]) {
    for index in configs {
        match &mut index.kind {
            ConfigKind::Cached { routing, offline, .. } => {
                for upstream in &mut routing.upstreams {
                    upstream.username = None;
                    upstream.password = None;
                    upstream.token = None;
                    upstream.credential_exec = None;
                    upstream.credential_refresh = None;
                    upstream.tls = UpstreamTlsConfig::default();
                }
                *offline = true;
            }
            ConfigKind::Hosted { .. } | ConfigKind::Virtual { .. } => {}
        }
        index.tokens.retain_mut(|token| {
            token.actions.retain(|action| *action == Action::Read);
            !token.actions.is_empty()
        });
        index.webhooks.clear();
    }
}

pub fn router_for(state: Arc<AppState>) -> Router {
    peryx_web::ssr::ui_router(state.clone()).merge(router(state))
}

type CredentialProviders = HashMap<(String, String), CredentialProvider>;

pub(crate) fn build_indexes_with_plugins(
    configs: &[IndexConfig],
    auth: &AuthConfig,
    offline: bool,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<Vec<Index>> {
    let plugins = plugins
        .activate(configs.iter().map(|config| config.ecosystem.clone()))
        .context("activate configured ecosystems")?;
    let credential_providers = build_credential_providers(configs, None)?;
    build_indexes_with_providers(configs, auth, offline, &credential_providers, &plugins)
}

fn build_indexes_with_providers(
    configs: &[IndexConfig],
    auth: &AuthConfig,
    offline: bool,
    credential_providers: &CredentialProviders,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<Vec<Index>> {
    let capabilities = plugins.drivers();
    let plugin_prefixes = plugins
        .route_prefixes()
        .map(|(ecosystem, prefix)| (prefix, ecosystem.to_string()))
        .collect::<Vec<_>>();
    let reserved_prefixes = || {
        path::CORE_ROUTE_PREFIXES
            .iter()
            .map(|prefix| (*prefix, "peryx core"))
            .chain(peryx_web::ROUTE_PATHS.iter().map(|prefix| (*prefix, "peryx UI")))
            .chain(plugin_prefixes.iter().map(|(prefix, owner)| (*prefix, owner.as_str())))
    };
    let mut positions = HashMap::with_capacity(configs.len());
    let mut routes = HashMap::with_capacity(configs.len());
    for (pos, index) in configs.iter().enumerate() {
        path::validate_path_segment("index name", &index.name)?;
        match path::validate_route(&index.route, reserved_prefixes()) {
            Ok(()) => {}
            Err(error @ path::PathSafetyError::ReservedRoute { .. }) => {
                bail!("invalid index route {}: {error}", index.route);
            }
            Err(error) => return Err(error).context(format!("invalid index route {}", index.route)),
        }
        if positions.insert(index.name.as_str(), pos).is_some() {
            bail!("duplicate index name {}", index.name);
        }
        if routes.insert(index.route.as_str(), pos).is_some() {
            bail!("duplicate index route {}", index.route);
        }
    }
    validate_index_composition(configs, &positions)?;
    configs
        .iter()
        .map(|index| {
            let policy = plugins.drivers().get_policy(&index.ecosystem);
            let rules = match policy {
                Some(driver) => driver.compile_policy(&index.ecosystem_policy),
                None if index.ecosystem_policy.is_empty() => Ok(PolicyCapabilities::default()),
                None => Err(format!(
                    "the {} ecosystem does not support artifact policy",
                    index.ecosystem
                )),
            }
            .map_err(anyhow::Error::msg)
            .context(format!("compile policy for {}", index.name))?;
            Ok(Index {
                name: index.name.clone(),
                route: index.route.clone(),
                ecosystem: index.ecosystem.clone(),
                kind: build_kind(index, configs, &positions, offline, credential_providers)?,
                policy: Policy::compile(&index.policy, |name| {
                    capabilities
                        .get_name(&index.ecosystem)
                        .map_or_else(|| name.to_owned(), |driver| driver.normalize_name(name))
                })
                .with_rules(rules),
                acl: index
                    .acl(auth)
                    .context(format!("read the access rules of index {}", index.name))?,
            })
        })
        .collect()
}

fn validate_index_composition(configs: &[IndexConfig], positions: &HashMap<&str, usize>) -> anyhow::Result<()> {
    let mut visits = vec![IndexVisit::New; configs.len()];
    let mut path = Vec::new();
    for position in 0..configs.len() {
        visit_index(position, configs, positions, &mut visits, &mut path)?;
    }
    Ok(())
}

fn visit_index(
    position: usize,
    configs: &[IndexConfig],
    positions: &HashMap<&str, usize>,
    visits: &mut [IndexVisit],
    path: &mut Vec<usize>,
) -> anyhow::Result<()> {
    match visits[position] {
        IndexVisit::Complete => return Ok(()),
        IndexVisit::Active => {
            let start = path
                .iter()
                .position(|&candidate| candidate == position)
                .expect("an active index is on the traversal path");
            let cycle = path[start..]
                .iter()
                .chain(std::iter::once(&position))
                .map(|&candidate| configs[candidate].name.as_str())
                .collect::<Vec<_>>()
                .join(" -> ");
            bail!("virtual index composition cycle: {cycle}");
        }
        IndexVisit::New => {}
    }
    visits[position] = IndexVisit::Active;
    path.push(position);
    if let ConfigKind::Virtual { layers, .. } = &configs[position].kind {
        for layer in layers {
            visit_index(
                resolve_name(&configs[position].name, layer, positions)?,
                configs,
                positions,
                visits,
                path,
            )?;
        }
    }
    path.pop();
    visits[position] = IndexVisit::Complete;
    Ok(())
}

#[derive(Clone, Copy)]
enum IndexVisit {
    New,
    Active,
    Complete,
}

/// Compile each index's `[index.settings]` table against the ecosystem it serves, keyed by index name.
///
/// Settings belong to their adapter, so the table travels raw through neutral config and is compiled here, in the one
/// crate that names ecosystems. An ecosystem with no settings of its own claims no key, so a key on
/// one of its indexes is configuration that would otherwise be silently ignored.
fn build_index_settings_with_plugins(
    configs: &[IndexConfig],
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<HashMap<String, peryx_driver::serving::CompiledEcosystemSettings>> {
    let mut settings = HashMap::new();
    for index in configs {
        if let Some(compiled) = plugins
            .compile_index_settings(&index.ecosystem, &index.name, &index.ecosystem_settings)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("compile ecosystem settings for {}", index.name))?
        {
            settings.insert(index.name.clone(), compiled);
        }
    }
    Ok(settings)
}

fn build_webhooks(configs: &[IndexConfig]) -> anyhow::Result<WebhookRuntime> {
    let mut targets = Vec::new();
    for index in configs {
        for webhook in &index.webhooks {
            targets.push(WebhookTargetConfig {
                index: index.name.clone(),
                name: webhook.name.clone(),
                url: webhook.url.clone(),
                secret: webhook_secret(&webhook.secret, &webhook.name)?,
                events: webhook.events.clone(),
            });
        }
    }
    WebhookRuntime::new(targets).context("build webhook targets")
}

#[derive(Clone)]
struct UpstreamCredentials {
    username: Option<String>,
    password: Option<SecretSource>,
    token: Option<SecretSource>,
    exec: Option<ExecCredentialConfig>,
    refresh: Option<CredentialRefreshConfig>,
}

fn webhook_secret(secret: &WebhookSecret, name: &str) -> anyhow::Result<String> {
    match secret {
        WebhookSecret::Literal(secret) => Ok(secret.clone()),
        WebhookSecret::Env(var) => {
            std::env::var(var).with_context(|| format!("read webhook secret env var {var} for target {name}"))
        }
    }
}

fn build_kind(
    index: &IndexConfig,
    configs: &[IndexConfig],
    positions: &HashMap<&str, usize>,
    global_offline: bool,
    credential_providers: &CredentialProviders,
) -> anyhow::Result<IndexKind> {
    match &index.kind {
        ConfigKind::Cached { routing, offline, .. } => {
            let primary = &routing.upstreams[0];
            Ok(IndexKind::Cached {
                client: build_upstream_client(
                    &index.name,
                    &primary.url,
                    credential_provider(credential_providers, &index.name, &primary.name),
                    &load_upstream_tls(&index.name, &primary.tls)?,
                    &primary.url,
                )?,
                offline: global_offline || *offline,
            })
        }
        ConfigKind::Hosted { volatile, .. } => Ok(IndexKind::Hosted { volatile: *volatile }),
        ConfigKind::Virtual { layers, write_target } => {
            let layer_positions = layers
                .iter()
                .map(|name| resolve_name(&index.name, name, positions))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let write_target =
                resolve_write_target(index, write_target.as_deref(), &layer_positions, configs, positions)?;
            Ok(IndexKind::Virtual {
                layers: layer_positions,
                write_target,
            })
        }
    }
}

fn build_credential_providers(configs: &[IndexConfig], netrc: Option<&Netrc>) -> anyhow::Result<CredentialProviders> {
    let mut providers = HashMap::new();
    for index in configs {
        let ConfigKind::Cached { routing, .. } = &index.kind else {
            continue;
        };
        for upstream in &routing.upstreams {
            providers.insert(
                (index.name.clone(), upstream.name.clone()),
                build_credential_provider(
                    &index.name,
                    &upstream.url,
                    UpstreamCredentials {
                        username: upstream.username.clone(),
                        password: upstream.password.clone(),
                        token: upstream.token.clone(),
                        exec: upstream.credential_exec.clone(),
                        refresh: upstream.credential_refresh,
                    },
                    netrc,
                )?,
            );
        }
    }
    Ok(providers)
}

fn build_credential_provider(
    index: &str,
    upstream: &str,
    credentials: UpstreamCredentials,
    netrc: Option<&Netrc>,
) -> anyhow::Result<CredentialProvider> {
    if let Some(exec) = credentials.exec {
        return exec
            .provider(upstream, CredentialScope::Read)
            .context(format!("configure the credential helper of index {index}"));
    }
    let mut auth =
        resolve_upstream_auth(&credentials).context(format!("read the upstream credentials of index {index}"))?;
    if auth == Auth::None
        && let Some(netrc) = netrc
    {
        auth = netrc
            .auth_for_str(upstream)
            .context(format!("match netrc credentials for {}", redact_url(upstream)))?;
    }
    let Some(refresh) = credentials.refresh else {
        return Ok(CredentialProvider::fixed(auth));
    };
    let credentials = Arc::new(credentials);
    let index = Arc::<str>::from(index);
    Ok(CredentialProvider::refreshing(
        auth,
        CredentialRefresh {
            interval: refresh.interval,
            on_unauthorized: refresh.on_unauthorized,
            failure: match refresh.failure {
                CredentialFailureMode::Fail => CredentialFailure::Fail,
                CredentialFailureMode::Anonymous => CredentialFailure::Anonymous,
            },
        },
        move || {
            let credentials = credentials.clone();
            let index = index.clone();
            async move {
                let task_index = index.clone();
                tokio::task::spawn_blocking(move || {
                    resolve_upstream_auth(&credentials)
                        .map_err(|error| CredentialError::new(format!("index {task_index}: {error:#}")))
                })
                .await
                .expect("a directly awaited credential task cannot be cancelled")
            }
        },
    ))
}

fn resolve_upstream_auth(credentials: &UpstreamCredentials) -> anyhow::Result<Auth> {
    let read = |source: Option<&SecretSource>| source.map(SecretSource::read).transpose();
    let (token, password) = (read(credentials.token.as_ref())?, read(credentials.password.as_ref())?);
    Ok(upstream_auth(
        token.as_deref(),
        credentials.username.as_deref(),
        password.as_deref(),
    ))
}

fn credential_provider(providers: &CredentialProviders, index: &str, upstream: &str) -> CredentialProvider {
    providers[&(index.to_owned(), upstream.to_owned())].clone()
}

fn build_upstream_routes(
    configs: &[IndexConfig],
    credential_providers: &CredentialProviders,
    netrc: Option<&Netrc>,
    plugins: &peryx_plugin_registry::PluginRegistry,
) -> anyhow::Result<Vec<(String, UpstreamRouter)>> {
    configs
        .iter()
        .filter_map(|index| match &index.kind {
            ConfigKind::Cached { routing, .. } => Some((index, routing)),
            ConfigKind::Hosted { .. } | ConfigKind::Virtual { .. } => None,
        })
        .map(|(index, routing)| {
            let upstreams = routing
                .upstreams
                .iter()
                .map(|upstream| {
                    let tls = load_upstream_tls(&index.name, &upstream.tls)?;
                    let client = build_upstream_client(
                        &index.name,
                        &upstream.url,
                        credential_provider(credential_providers, &index.name, &upstream.name),
                        &tls,
                        &upstream.url,
                    )?;
                    let named = NamedUpstream::new(&upstream.name, client);
                    let Some(artifact_url) = &upstream.artifact_url else {
                        return Ok(named);
                    };
                    let credentials = if upstream.credential_exec.is_some()
                        || upstream.token.is_some()
                        || upstream.username.is_some() && upstream.password.is_some()
                    {
                        credential_provider(credential_providers, &index.name, &upstream.name)
                    } else {
                        build_credential_provider(
                            &index.name,
                            artifact_url,
                            UpstreamCredentials {
                                username: upstream.username.clone(),
                                password: upstream.password.clone(),
                                token: upstream.token.clone(),
                                exec: None,
                                refresh: None,
                            },
                            netrc,
                        )?
                    };
                    let mirror = build_upstream_client(&index.name, artifact_url, credentials, &tls, &upstream.url)?;
                    Ok(named.with_artifact_mirror(mirror, routing.fallback))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let normalize = |name: &str| {
                plugins
                    .drivers()
                    .get_name(&index.ecosystem)
                    .map_or_else(|| name.to_owned(), |driver| driver.normalize_name(name))
            };
            let mut router = UpstreamRouter::new(upstreams)?.with_fallback(routing.fallback);
            for resource in &routing.protected {
                router = router.protect(normalize(resource))?;
            }
            for (resource, upstream) in &routing.pins {
                router = router.pin(normalize(resource), upstream)?;
            }
            Ok((index.name.clone(), router))
        })
        .collect()
}

fn build_upstream_client(
    index: &str,
    upstream: &str,
    credentials: CredentialProvider,
    tls: &UpstreamTls,
    identity_origin: &str,
) -> anyhow::Result<UpstreamClient> {
    UpstreamClient::with_credentials_and_tls_for_origin(upstream, credentials, tls, identity_origin, &[])
        .with_context(|| format!("build cached index {index} with upstream {}", redact_url(upstream)))
}

fn load_upstream_tls(index: &str, config: &UpstreamTlsConfig) -> anyhow::Result<UpstreamTls> {
    let identity = match (config.client_cert_file.as_deref(), config.client_key_file.as_deref()) {
        (Some(certificate), Some(key)) => Some((certificate, key)),
        (None, None) => None,
        _ => bail!("index {index} requires both upstream client certificate and private key files"),
    };
    UpstreamTls::from_paths(config.ca_file.as_deref(), identity)
        .with_context(|| format!("load upstream TLS material for index {index}"))
}

fn upstream_concurrency(configs: &[IndexConfig]) -> Vec<(String, usize)> {
    configs
        .iter()
        .filter_map(|index| match &index.kind {
            ConfigKind::Cached {
                upstream_concurrency, ..
            } => Some((index.name.clone(), *upstream_concurrency)),
            ConfigKind::Hosted { .. } | ConfigKind::Virtual { .. } => None,
        })
        .collect()
}

fn resolve_name(virtual_route: &str, name: &str, positions: &HashMap<&str, usize>) -> anyhow::Result<usize> {
    positions
        .get(name)
        .copied()
        .with_context(|| format!("virtual index {virtual_route} references unknown index {name}"))
}

fn resolve_write_target(
    index: &IndexConfig,
    write_target: Option<&str>,
    layers: &[usize],
    configs: &[IndexConfig],
    positions: &HashMap<&str, usize>,
) -> anyhow::Result<Option<usize>> {
    match write_target {
        Some(name) => {
            let pos = resolve_name(&index.name, name, positions)?;
            if !matches!(configs[pos].kind, ConfigKind::Hosted { .. }) {
                bail!("virtual index {} write target {name} is not a hosted index", index.name);
            }
            Ok(Some(pos))
        }
        None => Ok(layers
            .iter()
            .copied()
            .find(|&pos| matches!(configs[pos].kind, ConfigKind::Hosted { .. }))),
    }
}

/// Derive upstream authentication: a bearer token takes precedence over a username/password pair;
/// otherwise the upstream is anonymous.
fn upstream_auth(token: Option<&str>, username: Option<&str>, password: Option<&str>) -> Auth {
    match (token, username, password) {
        (Some(token), _, _) => Auth::Bearer(token.to_owned()),
        (None, Some(username), Some(password)) => Auth::Basic {
            username: username.to_owned(),
            password: password.to_owned(),
        },
        _ => Auth::None,
    }
}
