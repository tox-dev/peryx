//! Assembling the HTTP server from configuration.

use std::borrow::Cow;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail, ensure};
use axum::Router;
use peryx_core::path;
use peryx_driver::state::RuntimeOptions;
use peryx_driver::{AppState, DriverSet, Index, IndexKind};
use peryx_events::webhook::{WebhookRuntime, WebhookTargetConfig};
use peryx_ha_distributed::{
    FrontierReply, HttpReceiptSource, HttpRemoteFrontierSource, MetadataFrontierProvider, ReceiptSource,
    RemoteFrontierSource, frontier_router, receipt_router,
};
use peryx_http::router;
use peryx_identity::{
    Action, LdapBindMode, LdapLoginService, LdapProvider, LdapProviderSettings, OidcLoginProvider, OidcLoginService,
    OidcProviderSettings, SessionSealer, Signer,
};
use peryx_plugin_registry as ecosystem_registry;
use peryx_policy::{Policy, PolicyDecisionRecorder, PolicyEvaluation};
use peryx_storage::blob::{BlobStorage, S3Config};
use peryx_storage::meta::{MetaStore, NewPolicyDecision};
use peryx_upstream::{
    Auth, CredentialError, CredentialFailure, CredentialProvider, CredentialRefresh, CredentialScope,
    ExecCredentialConfig, NamedUpstream, Netrc, UpstreamClient, UpstreamRouter, UpstreamTls, redact_url,
};

use crate::config::{
    AuthConfig, AvailabilityConfig, AvailabilityMode, BlobStorageConfig, Config, CredentialFailureMode,
    CredentialRefreshConfig, DcRole, IndexConfig, IndexKind as ConfigKind, LdapBindConfig, LdapProviderConfig,
    OidcProviderConfig, ReplicationConfig, SecretSource, UpstreamTlsConfig, WebhookSecret,
};

/// The derived views a read must not outrun. A replica gates reads on whole-blob availability as well
/// as the search view, so its readable frontier holds at the slower of the metadata and blob views;
/// every other role gates on the search view alone.
fn required_views(config: &Config) -> Arc<[&'static str]> {
    if config.availability.is_replica_mode() {
        Arc::from([peryx_driver::state::SEARCH_VIEW, peryx_ha_distributed::BLOB_VIEW])
    } else {
        Arc::from([peryx_driver::state::SEARCH_VIEW])
    }
}

/// Leave S3 credential resolution with the SDK provider chain.
///
/// # Errors
/// Returns an error when the S3 backend configuration is invalid.
pub(crate) fn build_blob_storage(config: &Config) -> anyhow::Result<BlobStorage> {
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
    let state = build_state(config)?;
    let router = router_for(state.clone());
    Ok(
        match crate::replication::ReplicationRuntime::from_config(config, &state)? {
            Some(replication) => replication.mount(router),
            None => router,
        },
    )
}

/// Validate a fully resolved configuration the way `serve` would accept it.
///
/// It opens no metadata store, binds no socket, and reaches no upstream: it runs the cross-field
/// [`Config::validate`] rules, the logging-sink check, and the full index assembly — topology,
/// policy compilation, secret reads, and upstream-client construction — so an operator can confirm a
/// config before a restart the way `nginx -t` confirms a server block.
///
/// # Errors
/// Returns the first configuration error the server would hit while assembling its state.
pub fn check_config(config: &Config) -> anyhow::Result<()> {
    config.validate().context("validate configuration")?;
    crate::logging::validate(&config.log).context("validate logging configuration")?;
    build_indexes(&config.indexes, &config.auth, config.offline)?;
    build_index_settings(&config.indexes)?;
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
    config.validate().context("validate configuration")?;
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("create data directory {}", config.data_dir.display()))?;
    let meta_path = config.data_dir.join("peryx.redb");
    let meta = MetaStore::open(&meta_path).with_context(|| format!("open metadata store {}", meta_path.display()))?;
    let configured_replica = config.availability.is_replica_mode();
    let read_only = config.read_only || configured_replica;
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
        build_upstream_routes(&configs, &credential_providers, netrc.as_ref())?
    };
    let mut indexes = build_indexes_with_providers(
        &configs,
        &config.auth,
        config.offline || read_only,
        &credential_providers,
    )?;
    if configured_replica {
        for index in &mut indexes {
            if let IndexKind::Virtual { upload, .. } = &mut index.kind {
                *upload = None;
            }
        }
    }
    attach_policy_decision_recorders(&meta, &mut indexes)?;
    if !read_only {
        crate::config::reconcile_configured_repositories(&meta, &configs);
    }
    let ecosystem_settings = build_index_settings(&configs)?;
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
    ecosystem_registry::install_drivers(
        &mut state,
        &ecosystem_settings,
        config.availability.is_distributed_mode(),
    )
    .map_err(anyhow::Error::msg)?;
    state.set_ldap_logins(ldap_logins(&config.auth.ldap_providers, &state.meta)?);
    let oidc_logins = oidc_logins(&config.auth.oidc_providers, &state.meta)?;
    state.set_oidc_logins(oidc_logins);
    state.read_only = read_only;
    configure_availability(&mut state, config, read_only)?;
    if let Some(source) = &config.auth.signing_key {
        let key = source.read().context("read the token realm signing key")?;
        if key.trim().is_empty() {
            bail!("token realm signing key must not be empty");
        }
        state.set_session_sealer(SessionSealer::new(key.as_bytes()));
        let signer = Signer::new(key.as_bytes(), peryx_identity::TOKEN_AUDIENCE);
        if let Some(runtime) = trusted_publishing(config, signer.clone())? {
            state.set_trusted_publishing(runtime);
        }
        state.set_token_realm(signer, config.auth.token_ttl_secs);
    }
    state.set_openapi(crate::api::openapi_json());
    let state = Arc::new(state);
    if config.availability.is_distributed_mode() {
        crate::replication::install_read_through(config, &state)?;
    }
    if !state.read_only && !state.webhooks.is_empty() {
        peryx_events::webhook::kick(state.serving.clone());
    }
    Ok(state)
}

/// The authority role this node holds, from its configured replication role rather than its read-only
/// posture. A configured primary writes even when it serves read-only, and a `none` node is a lone
/// writer, so only a configured replica reports [`Replica`](peryx_core::NodeRole::Replica). This keeps
/// the topology snapshot's self-role aligned with the replication and control surfaces, which read the
/// same configured role.
const fn availability_role(config: &Config) -> peryx_core::NodeRole {
    if config.availability.is_replica_mode() {
        peryx_core::NodeRole::Replica
    } else {
        peryx_core::NodeRole::Writer
    }
}

/// Project the resolved configuration into the fixed availability topology the snapshot endpoint serves.
///
/// A writer knows its own roster identity, so it marks itself in the roster; a replica does not carry its
/// identity, so it leaves the local mark unset and reports its own live status through the snapshot's
/// dedicated local node instead.
/// Install the resolved availability posture on the state: the authority role, the topology snapshot the
/// process serves, and the write-ack quorum and deadline hosted writes are acknowledged against.
fn configure_availability(state: &mut AppState, config: &Config, read_only: bool) -> anyhow::Result<()> {
    if matches!(config.availability, AvailabilityConfig::None) {
        return Ok(());
    }
    state.set_availability_role(availability_role(config));
    state.set_availability_topology(availability_topology(config, read_only));
    state.set_write_ack(config.write_ack.policy, config.write_ack.deadline);
    state.set_receipt_sources(receipt_sources(config)?);
    state.set_remote_frontier_sources(remote_frontier_sources(config)?);
    state.set_analytics_completeness(Arc::new(peryx_ha_distributed::DistributedAnalyticsCompleteness));
    Ok(())
}

/// How long a same-datacenter receipt query waits on one peer before it is treated as a peer that did not
/// contribute this round. Bounded well under a typical client write deadline so the gather re-polls the
/// remaining peers within the window.
const RECEIPT_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// The replication bearer token a `dc` or `ha` role carries, which same-datacenter peers accept for
/// receipt queries — the same credential their blob endpoints accept.
const fn receipt_token(replication: &ReplicationConfig) -> &SecretSource {
    match replication {
        ReplicationConfig::Primary { token, .. } | ReplicationConfig::Replica { token, .. } => token,
    }
}

/// The same-datacenter peers an ingress write gathers placement receipts from: every rostered member in
/// the local node's datacenter other than itself, each behind the shared replication credential.
///
/// Yields an empty set when the node gathers from no peer — no membership, no node identity, no
/// replication token, this node absent from the roster, or no same-datacenter peer — so a single-node or
/// single-member-per-DC deployment proves its quorum from the local receipt alone and runs no gather.
///
/// The local datacenter is the one holding the roster member this node names through `node_identity`;
/// `writer_identity` is the shared writer every node claims and is the same across the group, so it
/// cannot single out this node's datacenter. It falls back to `writer_identity` for a single-writer
/// group that configures no distinct node identity.
///
/// # Errors
/// Returns an error when the replication token cannot be read or a peer address is not a usable base.
pub(crate) fn receipt_sources(config: &Config) -> anyhow::Result<Vec<Arc<dyn ReceiptSource + Send + Sync>>> {
    let (Some(membership), Some(identity), Some(replication)) = (
        config.dc_membership.as_ref(),
        config.node_identity.as_deref().or(config.writer_identity.as_deref()),
        config.availability.replication(),
    ) else {
        return Ok(Vec::new());
    };
    let Some(local) = membership.members.iter().find(|member| member.node == identity) else {
        return Ok(Vec::new());
    };
    let token = receipt_token(replication)
        .read()
        .context("read the replication token for same-datacenter receipt gathering")?;
    let mut sources: Vec<Arc<dyn ReceiptSource + Send + Sync>> = Vec::new();
    for member in &membership.members {
        if member.dc != local.dc || member.node == identity {
            continue;
        }
        let source = HttpReceiptSource::new(
            &member.address,
            member.node.clone(),
            token.clone(),
            RECEIPT_FETCH_TIMEOUT,
        )
        .with_context(|| format!("build receipt transport for same-datacenter peer {}", member.node))?;
        sources.push(Arc::new(source));
    }
    Ok(sources)
}

/// The peer-receipt endpoint this node serves to its same-datacenter peers.
///
/// Returns `None` when the node runs no replication and so has no peer to answer. Rides the token-gated
/// replication surface alongside the blob endpoint, so a peer reaches it at the same rostered address and
/// credential.
///
/// # Errors
/// Returns an error when the replication token cannot be read or the endpoint credential is empty.
pub fn receipt_endpoint_router(config: &Config, blobs: &BlobStorage) -> anyhow::Result<Option<Router>> {
    let Some(replication) = config.availability.replication() else {
        return Ok(None);
    };
    let token = receipt_token(replication)
        .read()
        .context("read the replication token for the peer receipt endpoint")?;
    let router = receipt_router(token, blobs.clone()).context("build peer receipt routes")?;
    Ok(Some(router))
}

/// Interpret a rostered member address as an HTTP base URL, defaulting to `http` when it carries no
/// scheme, so an internal `host:port` address still builds a usable transport rather than failing startup.
fn member_base_url(address: &str) -> String {
    if address.starts_with("http://") || address.starts_with("https://") {
        address.to_owned()
    } else {
        format!("http://{address}")
    }
}

/// One address per remote datacenter to query for its metadata frontier, preferring the datacenter's
/// writer since it issues the authoritative serials. The local datacenter is never its own remote.
pub(crate) fn remote_dc_roster(membership: &crate::config::DcMembership, local_dc: &str) -> BTreeMap<String, String> {
    let mut roster: BTreeMap<String, String> = BTreeMap::new();
    for member in &membership.members {
        if member.dc == local_dc {
            continue;
        }
        match roster.entry(member.dc.clone()) {
            Entry::Vacant(slot) => {
                slot.insert(member.address.clone());
            }
            Entry::Occupied(mut slot) if member.role == DcRole::Writer => {
                slot.insert(member.address.clone());
            }
            Entry::Occupied(_) => {}
        }
    }
    roster
}

/// The eligible remote datacenters an `ha` write gathers metadata acknowledgements from: one address per
/// remote datacenter, writer-preferred, behind the shared replication credential.
///
/// Yields an empty set outside `ha` mode and whenever the node waits on no remote — no membership, no
/// writer identity, this node absent from the roster, or a single-datacenter group — so a `none`/`dc`
/// node and a degenerate single-DC `ha` node run no remote gather.
///
/// # Errors
/// Returns an error when the replication token cannot be read or a remote address is not a usable base.
pub(crate) fn remote_frontier_sources(
    config: &Config,
) -> anyhow::Result<Vec<Arc<dyn RemoteFrontierSource + Send + Sync>>> {
    if config.availability.mode() != AvailabilityMode::Ha {
        return Ok(Vec::new());
    }
    let (Some(membership), Some(identity), Some(replication)) = (
        config.dc_membership.as_ref(),
        config.writer_identity.as_deref(),
        config.availability.replication(),
    ) else {
        return Ok(Vec::new());
    };
    let Some(local) = membership.members.iter().find(|member| member.node == identity) else {
        return Ok(Vec::new());
    };
    let roster = remote_dc_roster(membership, &local.dc);
    if roster.is_empty() {
        return Ok(Vec::new());
    }
    if roster.len() == 1 {
        // The sole remote datacenter holds every write's only remote durable copy, so removing it drops
        // remote durability for the whole group; that removal must be an explicit administrator action.
        // Named outside the macro so the lookup runs whether or not a warn-level subscriber is installed.
        let sole = roster.keys().next().expect("one remote datacenter is present");
        tracing::warn!(
            datacenter = %sole,
            "ha configured with a single remote datacenter; it is the sole remote durable copy for every write",
        );
    }
    let token = receipt_token(replication)
        .read()
        .context("read the replication token for remote metadata-frontier gathering")?;
    let mut sources: Vec<Arc<dyn RemoteFrontierSource + Send + Sync>> = Vec::new();
    for (datacenter, address) in &roster {
        let source = HttpRemoteFrontierSource::new(
            &member_base_url(address),
            datacenter.clone(),
            token.clone(),
            RECEIPT_FETCH_TIMEOUT,
        )
        .with_context(|| format!("build remote metadata-frontier transport for datacenter {datacenter}"))?;
        sources.push(Arc::new(source));
    }
    Ok(sources)
}

/// Reports this node's metadata frontier for an authority to a remote datacenter, reading the epoch from
/// the ownership authority and the applied frontier from the metadata journal.
struct AppStateFrontierProvider(Arc<AppState>);

#[async_trait::async_trait]
impl MetadataFrontierProvider for AppStateFrontierProvider {
    async fn frontier(&self, authority: &str) -> Option<FrontierReply> {
        let applied_frontier = self.0.serving.meta.current_serial().ok()?;
        let epoch = self.0.committed_authority_epoch(authority).await;
        Some(FrontierReply {
            epoch,
            applied_frontier,
        })
    }
}

/// The remote-frontier endpoint an `ha` node serves to its remote datacenters, reporting how far it has
/// durably applied an authority's metadata.
///
/// Returns `None` outside `ha` mode, since only a remote datacenter queries it. Rides the token-gated
/// replication surface alongside the receipt endpoint.
///
/// # Errors
/// Returns an error when the replication token cannot be read or the endpoint credential is empty.
pub fn frontier_endpoint_router(config: &Config, state: &Arc<AppState>) -> anyhow::Result<Option<Router>> {
    // Only an `ha` node has a remote datacenter to answer; a `none` or `dc` node serves no frontier.
    let AvailabilityConfig::Ha(replication) = &config.availability else {
        return Ok(None);
    };
    let token = receipt_token(replication)
        .read()
        .context("read the replication token for the remote frontier endpoint")?;
    let provider: Arc<dyn MetadataFrontierProvider> = Arc::new(AppStateFrontierProvider(state.clone()));
    let router = frontier_router(token, provider).context("build remote frontier routes")?;
    Ok(Some(router))
}

fn availability_topology(config: &Config, read_only: bool) -> peryx_core::TopologyConfig {
    let mode = match config.availability.mode() {
        AvailabilityMode::None => peryx_core::TopologyMode::None,
        AvailabilityMode::Dc => peryx_core::TopologyMode::Dc,
        AvailabilityMode::Ha => peryx_core::TopologyMode::Ha,
    };
    let (group, members) = config.dc_membership.as_ref().map_or_else(
        || (None, Vec::new()),
        |membership| {
            let members = membership
                .members
                .iter()
                .map(|member| peryx_core::TopologyMember {
                    node: member.node.clone(),
                    dc: member.dc.clone(),
                    address: member.address.clone(),
                    role: match member.role {
                        DcRole::Writer => peryx_core::NodeRole::Writer,
                        DcRole::Replica => peryx_core::NodeRole::Replica,
                    },
                })
                .collect();
            (Some(membership.group.clone()), members)
        },
    );
    peryx_core::TopologyConfig {
        mode,
        group,
        members,
        local_node: (!read_only).then(|| config.writer_identity.clone()).flatten(),
    }
}

/// Close attempts interrupted by a prior process before this writer serves management traffic.
///
/// # Errors
/// Returns a metadata error when interrupted attempts cannot be read or updated.
pub fn recover_job_attempts(state: &AppState) -> Result<usize, peryx_storage::meta::MetaError> {
    if state.read_only {
        Ok(0)
    } else {
        state.job_attempts.recover_interrupted((state.clock)())
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

fn trusted_publishing(config: &Config, signer: Signer) -> anyhow::Result<Option<peryx_identity::OidcRuntime>> {
    if config.auth.trusted_publishers.is_empty() {
        return Ok(None);
    }
    let repositories = config
        .indexes
        .iter()
        .map(|index| (index.name.as_str(), index))
        .collect::<HashMap<_, _>>();
    let bindings = config
        .auth
        .trusted_publishers
        .iter()
        .map(|publisher| {
            let repository = repositories[publisher.repository.as_str()];
            peryx_identity::PublisherBinding {
                id: publisher.id.clone(),
                repository: repository.route.clone(),
                publisher: peryx_identity::TrustedPublisher {
                    issuer: publisher.issuer.clone(),
                    audience: config.auth.oidc_audience.clone(),
                    subject: peryx_identity::Glob::new(&publisher.subject),
                    claims: publisher.claims.clone(),
                    projects: publisher.projects.iter().map(peryx_identity::Glob::new).collect(),
                },
            }
        })
        .collect();
    peryx_identity::OidcRuntime::new(bindings, signer, config.auth.token_ttl_secs)
        .map(Some)
        .context("configure trusted publishers")
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
            project: evaluation.project,
            version: evaluation.version,
            filename: evaluation.filename,
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

/// The full router over prepared state. The web UI mounts first: its routes (`/`, `/browse`,
/// `/pkg`) are all outside the index namespace, and everything else falls through to the API's
/// catch-all.
pub fn router_for(state: Arc<AppState>) -> Router {
    peryx_web::ssr::ui_router(state.clone()).merge(router(state))
}

/// The ecosystem drivers this build of peryx ships, named once here at the composition root. The
/// config-build and admin paths dispatch through it by an index's ecosystem, so no neutral code
/// names an ecosystem.
pub(crate) fn drivers() -> &'static DriverSet {
    ecosystem_registry::drivers()
}

type CredentialProviders = HashMap<(String, String), CredentialProvider>;

/// Resolve configured indexes into their runtime form, mapping virtual-index member names to positions,
/// building each cached index's authenticated upstream client, and reading each index's access rules
/// (which is where a secret kept in a file is read).
pub(crate) fn build_indexes(configs: &[IndexConfig], auth: &AuthConfig, offline: bool) -> anyhow::Result<Vec<Index>> {
    let credential_providers = build_credential_providers(configs, None)?;
    build_indexes_with_providers(configs, auth, offline, &credential_providers)
}

fn build_indexes_with_providers(
    configs: &[IndexConfig],
    auth: &AuthConfig,
    offline: bool,
    credential_providers: &CredentialProviders,
) -> anyhow::Result<Vec<Index>> {
    let mut positions = HashMap::with_capacity(configs.len());
    let mut routes = HashMap::with_capacity(configs.len());
    for (pos, index) in configs.iter().enumerate() {
        path::validate_route(&index.route).with_context(|| format!("invalid index route {}", index.route))?;
        if positions.insert(index.name.as_str(), pos).is_some() {
            bail!("duplicate index name {}", index.name);
        }
        if routes.insert(index.route.as_str(), pos).is_some() {
            bail!("duplicate index route {}", index.route);
        }
    }
    configs
        .iter()
        .map(|index| {
            let driver = drivers()
                .get(index.ecosystem)
                .expect("every configured ecosystem has a registered driver");
            let rules = driver
                .compile_policy(&index.ecosystem_policy)
                .map_err(|reason| anyhow::anyhow!("compile policy for {}: {reason}", index.name))?;
            Ok(Index {
                name: index.name.clone(),
                route: index.route.clone(),
                ecosystem: index.ecosystem,
                kind: build_kind(index, configs, &positions, offline, credential_providers)?,
                policy: Policy::compile(&index.policy, |name| driver.normalize_name(name)).with_rules(rules),
                acl: index
                    .acl(auth)
                    .with_context(|| format!("read the access rules of index {}", index.name))?,
            })
        })
        .collect()
}

/// Compile each index's `[index.settings]` table against the ecosystem it serves, keyed by index name.
///
/// The settings vocabulary is a format's own — an OCI cache's `library_prefix` means nothing to a
/// `PyPI` index — so the table travels raw through the neutral config and is compiled here, in the one
/// crate that names ecosystems. An ecosystem with no settings of its own claims no key, so a key on
/// one of its indexes is configuration that would otherwise be silently ignored.
pub(crate) fn build_index_settings(
    configs: &[IndexConfig],
) -> anyhow::Result<HashMap<String, peryx_driver::serving::CompiledEcosystemSettings>> {
    let mut settings = HashMap::new();
    for index in configs {
        if let Some(compiled) =
            ecosystem_registry::compile_index_settings(index.ecosystem, &index.name, &index.ecosystem_settings)
                .map_err(anyhow::Error::msg)?
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
        ConfigKind::Virtual { layers, upload } => {
            let layer_positions = layers
                .iter()
                .map(|name| resolve_name(&index.name, name, positions))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let upload_pos = resolve_upload(index, upload.as_deref(), &layer_positions, configs, positions)?;
            Ok(IndexKind::Virtual {
                layers: layer_positions,
                upload: upload_pos,
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
    let mut auth = resolve_upstream_auth(&credentials)
        .with_context(|| format!("read the upstream credentials of index {index}"))?;
    if auth == Auth::None
        && let Some(netrc) = netrc
    {
        auth = netrc
            .auth_for_str(upstream)
            .with_context(|| format!("match netrc credentials for {}", redact_url(upstream)))?;
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
                tokio::task::spawn_blocking(move || {
                    resolve_upstream_auth(&credentials)
                        .map_err(|error| CredentialError::new(format!("index {index}: {error:#}")))
                })
                .await
                .expect("secret resolution has no panic path")
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
            let driver = drivers()
                .get(index.ecosystem)
                .expect("every configured ecosystem has a registered driver");
            let mut router = UpstreamRouter::new(upstreams)?.with_fallback(routing.fallback);
            for project in &routing.protected {
                router = router.protect(driver.normalize_name(project))?;
            }
            for (project, upstream) in &routing.pins {
                router = router.pin(driver.normalize_name(project), upstream)?;
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

/// The virtual index's upload target: the named hosted index, or (when unset) the first hosted layer.
fn resolve_upload(
    index: &IndexConfig,
    upload: Option<&str>,
    layers: &[usize],
    configs: &[IndexConfig],
    positions: &HashMap<&str, usize>,
) -> anyhow::Result<Option<usize>> {
    match upload {
        Some(name) => {
            let pos = resolve_name(&index.name, name, positions)?;
            if !matches!(configs[pos].kind, ConfigKind::Hosted { .. }) {
                bail!(
                    "virtual index {} upload target {name} is not a hosted index",
                    index.name
                );
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
pub(crate) fn upstream_auth(token: Option<&str>, username: Option<&str>, password: Option<&str>) -> Auth {
    match (token, username, password) {
        (Some(token), _, _) => Auth::Bearer(token.to_owned()),
        (None, Some(username), Some(password)) => Auth::Basic {
            username: username.to_owned(),
            password: password.to_owned(),
        },
        _ => Auth::None,
    }
}
