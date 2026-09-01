use super::*;
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};
use std::sync::RwLock;

pub struct Harness {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) server: MockServer,
    pub(crate) state: Arc<AppState>,
    pub(crate) clock: Arc<AtomicI64>,
}
pub async fn harness_with(token: bool, volatile: bool) -> Harness {
    harness_with_policies(token, volatile, Policy::default(), Policy::default(), Policy::default()).await
}
pub async fn harness_with_policies(
    token: bool,
    volatile: bool,
    mirror_policy: Policy,
    local_policy: Policy,
    overlay_policy: Policy,
) -> Harness {
    harness_with_stale(
        token,
        volatile,
        mirror_policy,
        local_policy,
        overlay_policy,
        DEFAULT_MAX_STALE_SECS,
    )
    .await
}
pub async fn harness_with_stale(
    token: bool,
    volatile: bool,
    mirror_policy: Policy,
    local_policy: Policy,
    overlay_policy: Policy,
    max_stale_secs: i64,
) -> Harness {
    harness_with_options(
        token,
        volatile,
        mirror_policy,
        local_policy,
        overlay_policy,
        HarnessOptions {
            max_stale_secs,
            ..HarnessOptions::default()
        },
    )
    .await
}
pub async fn offline_harness(mirror_policy: Policy) -> Harness {
    harness_with_options(
        true,
        true,
        mirror_policy,
        Policy::default(),
        Policy::default(),
        HarnessOptions {
            offline: true,
            ..HarnessOptions::default()
        },
    )
    .await
}
/// The overlay reaches `pypi` through an intermediate virtual repository, so the cache is a leaf two
/// levels down rather than a direct member of the overlay.
pub async fn nested_harness(overlay_policy: Policy) -> Harness {
    harness_with_options(
        true,
        true,
        Policy::default(),
        Policy::default(),
        overlay_policy,
        HarnessOptions {
            nested: true,
            ..HarnessOptions::default()
        },
    )
    .await
}
pub async fn harness_with_upstream_concurrency(
    mirror_policy: Policy,
    overlay_policy: Policy,
    upstream_concurrency: usize,
) -> Harness {
    harness_with_options(
        true,
        true,
        mirror_policy,
        Policy::default(),
        overlay_policy,
        HarnessOptions {
            upstream_concurrency,
            ..HarnessOptions::default()
        },
    )
    .await
}
async fn harness_with_options(
    token: bool,
    volatile: bool,
    mirror_policy: Policy,
    local_policy: Policy,
    overlay_policy: Policy,
    options: HarnessOptions,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    if options.distributed {
        meta.initialize_distributed_state().unwrap();
    }
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let ticks = clock.clone();
    let mut indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: upstream,
                offline: options.offline,
            },
            policy: mirror_policy,
            acl: IndexAcl::default(),
        },
        Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            policy: local_policy,
            acl: if token {
                crate::tests::writer_acl("s3cret")
            } else {
                IndexAcl::default()
            },
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile },
        },
        Index {
            name: "root-pypi".to_owned(),
            route: "root/pypi".to_owned(),
            policy: overlay_policy,
            acl: IndexAcl::default(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: if options.nested { vec![1, 3] } else { vec![1, 0] },
                write_target: Some(1),
            },
        },
    ];
    if options.nested {
        indexes.push(Index {
            name: "inner".to_owned(),
            route: "inner".to_owned(),
            policy: Policy::default(),
            acl: IndexAcl::default(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: vec![0],
                write_target: None,
            },
        });
    }
    let mut state = AppState::with_limits(
        meta,
        blobs,
        60,
        indexes,
        Arc::new(move || ticks.load(Ordering::Relaxed)),
        peryx_driver::rate_limit::RateLimitConfig {
            trusted_proxies: options
                .trusted_proxies
                .iter()
                .map(|network| network.parse().unwrap())
                .collect(),
            ..peryx_driver::rate_limit::RateLimitConfig::default()
        },
        [("pypi".to_owned(), options.upstream_concurrency)],
    );
    Arc::get_mut(&mut state.serving).unwrap().max_stale_secs = options.max_stale_secs;
    let state = if options.distributed {
        install_distributed_services(&mut state);
        wire(state, true)
    } else {
        wire(state, false)
    };
    Harness {
        dir,
        server,
        state,
        clock,
    }
}

struct HarnessOptions {
    max_stale_secs: i64,
    offline: bool,
    upstream_concurrency: usize,
    distributed: bool,
    nested: bool,
    trusted_proxies: &'static [&'static str],
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            max_stale_secs: DEFAULT_MAX_STALE_SECS,
            offline: false,
            upstream_concurrency: peryx_driver::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
            distributed: false,
            nested: false,
            trusted_proxies: &[],
        }
    }
}
pub async fn harness() -> Harness {
    harness_with(true, true).await
}

pub async fn proxied_harness() -> Harness {
    harness_with_options(
        true,
        true,
        Policy::default(),
        Policy::default(),
        Policy::default(),
        HarnessOptions {
            trusted_proxies: &["10.0.0.0/8"],
            ..HarnessOptions::default()
        },
    )
    .await
}

pub async fn authority_harness() -> Harness {
    harness_with_options(
        true,
        true,
        Policy::default(),
        Policy::default(),
        Policy::default(),
        HarnessOptions {
            distributed: true,
            ..HarnessOptions::default()
        },
    )
    .await
}

fn clone_index(index: &Index) -> Index {
    Index {
        name: index.name.clone(),
        route: index.route.clone(),
        ecosystem: index.ecosystem.clone(),
        kind: match &index.kind {
            IndexKind::Cached { client, offline } => IndexKind::Cached {
                client: client.clone(),
                offline: *offline,
            },
            IndexKind::Hosted { volatile } => IndexKind::Hosted { volatile: *volatile },
            IndexKind::Virtual { layers, write_target } => IndexKind::Virtual {
                layers: layers.clone(),
                write_target: *write_target,
            },
        },
        policy: index.policy.clone(),
        acl: index.acl.clone(),
    }
}

pub fn restarted_state(harness: &Harness) -> Arc<AppState> {
    restarted_state_with_ttl(harness, harness.state.serving.ttl_secs)
}

pub fn restarted_state_with_ttl(harness: &Harness, ttl_secs: i64) -> Arc<AppState> {
    let clock = harness.clock.clone();
    let mut state = AppState::with_clock(
        harness.state.serving.meta.clone(),
        harness.state.serving.blobs.clone(),
        ttl_secs,
        harness.state.serving.indexes.iter().map(clone_index).collect(),
        Arc::new(move || clock.load(Ordering::Relaxed)),
    );
    Arc::get_mut(&mut state.serving).unwrap().max_stale_secs = harness.state.serving.max_stale_secs;
    if crate::replication_enabled(&harness.state.serving) {
        wire(state, true)
    } else {
        wire(state, false)
    }
}

pub fn reopened_read_only_harness(harness: Harness) -> Harness {
    let Harness {
        dir,
        server,
        state,
        clock,
    } = harness;
    let blobs = state.serving.blobs.clone();
    let ttl_secs = state.serving.ttl_secs;
    let max_stale_secs = state.serving.max_stale_secs;
    let indexes = state.serving.indexes.iter().map(clone_index).collect();
    drop(state);

    let ticks = clock.clone();
    let mut state = AppState::with_clock(
        MetaStore::open_existing_read_only(dir.path().join("peryx.redb")).unwrap(),
        blobs,
        ttl_secs,
        indexes,
        Arc::new(move || ticks.load(Ordering::Relaxed)),
    );
    Arc::get_mut(&mut state.serving).unwrap().max_stale_secs = max_stale_secs;
    install_distributed_services(&mut state);
    Harness {
        dir,
        server,
        state: wire(state, true),
        clock,
    }
}

pub fn replica_state(harness: &Harness) -> Arc<AppState> {
    let clock = harness.clock.clone();
    let mut state = AppState::with_clock(
        harness.state.serving.meta.clone(),
        harness.state.serving.blobs.clone(),
        harness.state.serving.ttl_secs,
        harness.state.serving.indexes.iter().map(clone_index).collect(),
        Arc::new(move || clock.load(Ordering::Relaxed)),
    );
    Arc::get_mut(&mut state.serving).unwrap().max_stale_secs = harness.state.serving.max_stale_secs;
    state.set_read_only(true).unwrap();
    wire(state, true)
}

pub async fn placement_harness() -> Harness {
    let harness = harness().await;
    initialize_distributed_schema(&harness.state);
    harness
}

pub fn initialize_distributed_schema(state: &AppState) {
    state.serving.meta.initialize_distributed_state().unwrap();
}

pub fn routed_state(dir: &tempfile::TempDir, primary: UpstreamClient, router: UpstreamRouter) -> Arc<AppState> {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let mut state = AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: primary,
                offline: false,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    Arc::get_mut(&mut state.serving)
        .unwrap()
        .upstream_routes
        .insert("pypi".to_owned(), router);
    wire(state, false)
}

pub async fn authority_promotion_harness() -> Harness {
    promotion_harness_with(true).await
}

pub async fn promotion_harness() -> Harness {
    promotion_harness_with(false).await
}

async fn promotion_harness_with(distributed: bool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let ticks = clock.clone();
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
        Index {
            name: "staging".to_owned(),
            route: "staging".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: crate::tests::writer_acl("s3cret".to_owned()),
        },
        Index {
            name: "prod".to_owned(),
            route: "prod".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: crate::tests::writer_acl("s3cret".to_owned()),
        },
        Index {
            name: "release".to_owned(),
            route: "release".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: vec![2, 0],
                write_target: Some(2),
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
    ];
    let mut state = AppState::with_clock(
        meta,
        blobs,
        60,
        indexes,
        Arc::new(move || ticks.load(Ordering::Relaxed)),
    );
    let state = if distributed {
        install_distributed_services(&mut state);
        wire(state, true)
    } else {
        wire(state, false)
    };
    Harness {
        dir,
        server,
        state,
        clock,
    }
}
/// Promotion where source read and target write are held by different credentials.
///
/// `s3cret` uploads to `staging` and writes `prod` but reads nothing; `pr0m0te` reads `staging` and
/// reads and writes `prod`. The two virtual routes share `staging` as their write target while
/// carrying the opposite `anonymous_read` to it, so a test can tell the named route's ACL from its
/// layer's. Every index here is closed to anonymous reads, so a test that inspects the target has to
/// present `pr0m0te` and a `404` from it means the release is absent rather than hidden.
pub async fn private_promotion_harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let clock = Arc::new(AtomicI64::new(1000));
    let ticks = clock.clone();
    let indexes = vec![
        Index {
            name: "internal".to_owned(),
            route: "internal".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap(),
                offline: false,
            },
            policy: Policy::default(),
            acl: sealed_acl(),
        },
        Index {
            name: "staging".to_owned(),
            route: "staging".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: IndexAcl {
                anonymous_read: false,
                tokens: vec![
                    named_token("writer", "s3cret", [Action::Write, Action::Delete]),
                    named_token("promoter", "pr0m0te", [Action::Read]),
                ],
            },
        },
        Index {
            name: "prod".to_owned(),
            route: "prod".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: IndexAcl {
                anonymous_read: false,
                tokens: vec![
                    named_token("writer", "s3cret", [Action::Write, Action::Delete]),
                    named_token("promoter", "pr0m0te", [Action::Read, Action::Write, Action::Delete]),
                ],
            },
        },
        Index {
            name: "sealed".to_owned(),
            route: "sealed".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: vec![1, 0],
                write_target: Some(1),
            },
            policy: Policy::default(),
            acl: sealed_acl(),
        },
        Index {
            name: "open".to_owned(),
            route: "open".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: vec![1, 0],
                write_target: Some(1),
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
    ];
    let state = AppState::with_clock(
        meta,
        blobs,
        60,
        indexes,
        Arc::new(move || ticks.load(Ordering::Relaxed)),
    );
    Harness {
        dir,
        server,
        state: wire(state, false),
        clock,
    }
}

fn sealed_acl() -> IndexAcl {
    IndexAcl {
        anonymous_read: false,
        tokens: Vec::new(),
    }
}

fn named_token(name: &str, secret: &str, actions: impl IntoIterator<Item = Action>) -> NamedToken {
    NamedToken {
        name: name.to_owned(),
        secret: secret.to_owned(),
        grants: vec![Grant {
            resources: vec![Glob::new("*")],
            actions: actions.into_iter().collect(),
        }],
        expires_at: None,
    }
}

pub fn policy(configure: impl FnOnce(&mut PolicyConfig, &mut PypiPolicyConfig)) -> Policy {
    let mut neutral = PolicyConfig::default();
    let mut pypi = PypiPolicyConfig::default();
    configure(&mut neutral, &mut pypi);
    Policy::compile(&neutral, crate::normalize_name).with_capabilities(compile_capabilities(&pypi).unwrap())
}
pub fn put_raw_project_status(path: &Path, key: &str, value: &[u8]) {
    let db = redb::Database::create(path).unwrap();
    let table: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("driver_kv");
    let namespaced = format!("pypi\u{0}s\u{0}{key}");
    let txn = db.begin_write().unwrap();
    txn.open_table(table)
        .unwrap()
        .insert(namespaced.as_str(), value)
        .unwrap();
    txn.commit().unwrap();
}
pub async fn stale_page_harness(max_stale_secs: i64, fetched_at: i64) -> Harness {
    let h = harness_with_stale(
        true,
        true,
        Policy::default(),
        Policy::default(),
        Policy::default(),
        max_stale_secs,
    )
    .await;
    let body = crate::to_json(&crate::ProjectDetail {
        meta: crate::Meta::default(),
        name: "flask".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![],
    });
    h.state
        .serving
        .meta
        .put_index(
            "pypi/flask",
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: fetched_at,
                content_type: None,
                fresh_secs: None,
                body: body.into_bytes(),
            },
        )
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    h
}

pub struct AuthorityDouble {
    pub committed: u64,
    pub current: u64,
    pub lease_expires_at_unix: i64,
    pub finish_available: bool,
}

impl Default for AuthorityDouble {
    fn default() -> Self {
        Self {
            committed: 0,
            current: 0,
            lease_expires_at_unix: i64::MAX,
            finish_available: true,
        }
    }
}

#[async_trait::async_trait]
impl peryx_driver::state::OwnershipAuthority for AuthorityDouble {
    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.committed
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        self.current != 0 && presented == self.current
    }

    async fn begin_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<peryx_ha::AuthorityWriteLease>, peryx_ha::OwnershipError> {
        Ok(
            (self.current != 0 && presented == self.current).then(|| peryx_ha::AuthorityWriteLease {
                authority: authority.to_owned(),
                epoch: presented,
                id: "test-write".to_owned(),
                expires_at_unix: self.lease_expires_at_unix,
            }),
        )
    }

    async fn finish_epoch_write(&self, _lease: &peryx_ha::AuthorityWriteLease) -> Result<(), peryx_ha::OwnershipError> {
        if self.finish_available {
            Ok(())
        } else {
            Err(peryx_ha::OwnershipError::Unavailable("quorum unavailable".to_owned()))
        }
    }

    async fn claim_home(
        &self,
        _authority: &str,
    ) -> Result<peryx_driver::state::HomeClaim, peryx_driver::state::OwnershipError> {
        Ok(peryx_driver::state::HomeClaim {
            home: "local".to_owned(),
            epoch: self.committed,
        })
    }

    fn cluster_status(&self) -> peryx_driver::state::ClusterStatus {
        peryx_driver::state::ClusterStatus {
            leader: None,
            term: self.current,
            voters: Vec::new(),
        }
    }

    async fn transfer_home(
        &self,
        authority: &str,
        new_home: &str,
    ) -> Result<Option<peryx_driver::state::TransferOutcome>, peryx_driver::state::OwnershipError> {
        Ok(Some(peryx_driver::state::TransferOutcome {
            from: authority.to_owned(),
            to: new_home.to_owned(),
            epoch: self.current + 1,
        }))
    }
}

pub fn bind_ownership_authority(state: &Arc<AppState>, authority: Arc<dyn peryx_ha::OwnershipAuthority>) {
    state
        .serving
        .plugin_service::<TestOwnership>()
        .expect("distributed test ownership is installed")
        .bind(authority);
}

pub fn install_authority(state: &Arc<AppState>, authority: AuthorityDouble) {
    bind_ownership_authority(state, Arc::new(authority));
}

pub fn install_distributed_services(state: &mut AppState) {
    let ownership = Arc::new(TestOwnership::default());
    state
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: peryx_core::NodeRole::Writer,
            topology: local_topology(),
            blobs: peryx_ha::BlobServices::new(None, Arc::new(LocalDurability)),
            analytics: Arc::new(UnavailableCompleteness),
            capabilities: peryx_ha::AvailabilityCapabilities {
                ownership: Some(ownership.clone()),
                ..Default::default()
            },
            authority_drainer: None,
            operations: None,
        })
        .unwrap();
    state.register_plugin_service(ownership).unwrap();
}

fn local_topology() -> peryx_core::TopologyConfig {
    peryx_core::TopologyConfig {
        mode: peryx_core::TopologyMode::Ha,
        group: Some("test".to_owned()),
        members: vec![peryx_core::TopologyMember {
            node: "writer".to_owned(),
            dc: "local".to_owned(),
            address: "http://127.0.0.1".to_owned(),
            role: peryx_core::NodeRole::Writer,
        }],
        local_node: Some("writer".to_owned()),
    }
}

fn wire(mut state: AppState, distributed: bool) -> Arc<AppState> {
    let registry = peryx_plugin_registry::PluginRegistry::new(vec![crate::registration()])
        .unwrap()
        .activate([crate::ECOSYSTEM])
        .unwrap();
    registry.register_activated_capabilities(&mut state.capability_install_context());
    if distributed {
        registry
            .install_distributed_drivers(
                &mut state.distributed_install_context().unwrap(),
                &std::collections::HashMap::new(),
            )
            .unwrap();
    } else {
        registry
            .install_drivers(
                &mut state.runtime_install_context().unwrap(),
                &std::collections::HashMap::new(),
            )
            .unwrap();
    }
    Arc::new(state)
}

#[derive(Default)]
struct TestOwnership(RwLock<Option<Arc<dyn peryx_ha::OwnershipAuthority>>>);

impl TestOwnership {
    fn bind(&self, ownership: Arc<dyn peryx_ha::OwnershipAuthority>) {
        *self.0.write().unwrap() = Some(ownership);
    }

    fn ownership(&self) -> Option<Arc<dyn peryx_ha::OwnershipAuthority>> {
        self.0.read().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl peryx_ha::OwnershipAuthority for TestOwnership {
    async fn claim_home(&self, authority: &str) -> Result<peryx_ha::HomeClaim, peryx_ha::OwnershipError> {
        match self.ownership() {
            Some(owner) => owner.claim_home(authority).await,
            None => Ok(peryx_ha::HomeClaim {
                home: "local".to_owned(),
                epoch: 0,
            }),
        }
    }

    fn cluster_status(&self) -> peryx_ha::ClusterStatus {
        self.ownership().map_or(
            peryx_ha::ClusterStatus {
                leader: None,
                term: 0,
                voters: Vec::new(),
            },
            |owner| owner.cluster_status(),
        )
    }

    async fn committed_epoch(&self, authority: &str) -> u64 {
        match self.ownership() {
            Some(owner) => owner.committed_epoch(authority).await,
            None => 0,
        }
    }

    async fn admit_epoch(&self, authority: &str, presented: u64) -> bool {
        match self.ownership() {
            Some(owner) => owner.admit_epoch(authority, presented).await,
            None => true,
        }
    }

    async fn begin_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<peryx_ha::AuthorityWriteLease>, peryx_ha::OwnershipError> {
        match self.ownership() {
            Some(owner) => owner.begin_epoch_write(authority, presented).await,
            None => Ok(Some(peryx_ha::AuthorityWriteLease {
                authority: authority.to_owned(),
                epoch: presented,
                id: "local-write".to_owned(),
                expires_at_unix: i64::MAX,
            })),
        }
    }

    async fn finish_epoch_write(&self, lease: &peryx_ha::AuthorityWriteLease) -> Result<(), peryx_ha::OwnershipError> {
        match self.ownership() {
            Some(owner) => owner.finish_epoch_write(lease).await,
            None => Ok(()),
        }
    }

    async fn transfer_home(
        &self,
        authority: &str,
        new_home: &str,
    ) -> Result<Option<peryx_ha::TransferOutcome>, peryx_ha::OwnershipError> {
        match self.ownership() {
            Some(owner) => owner.transfer_home(authority, new_home).await,
            None => Ok(None),
        }
    }
}

pub struct LocalDurability;

#[async_trait::async_trait]
impl peryx_ha::BlobWriteDurability for LocalDurability {
    async fn confirm(&self, write: peryx_ha::CommittedBlob<'_>) -> peryx_ha::WriteDurability {
        peryx_ha::WriteDurability::Confirmed {
            scope: write.evidence().scope(),
        }
    }
}

pub struct UnavailableCompleteness;

impl peryx_ha::AnalyticsCompleteness for UnavailableCompleteness {
    fn assess(
        &self,
        _meta: &dyn peryx_ha::AnalyticsSnapshotStore,
        _expected: &[peryx_ha::ExpectedProducer],
        _query: &peryx_ha::CompletenessQuery,
    ) -> Result<peryx_ha::CompletenessReport, peryx_ha::CompletenessError> {
        Err(peryx_ha::CompletenessError)
    }
}

#[tokio::test]
async fn test_ownership_uses_local_defaults_until_bound() {
    use peryx_driver::state::OwnershipAuthority as _;

    let ownership = TestOwnership::default();
    assert_eq!(
        (
            ownership.cluster_status(),
            ownership.transfer_home("flask", "west").await.unwrap(),
        ),
        (
            peryx_driver::state::ClusterStatus {
                leader: None,
                term: 0,
                voters: Vec::new(),
            },
            None,
        ),
    );
    ownership.bind(Arc::new(AuthorityDouble {
        committed: 7,
        current: 8,
        ..AuthorityDouble::default()
    }));

    assert_eq!(ownership.committed_epoch("flask").await, 7);
    assert!(ownership.admit_epoch("flask", 8).await);
    assert_eq!(
        ownership.claim_home("flask").await.unwrap(),
        peryx_driver::state::HomeClaim {
            home: "local".to_owned(),
            epoch: 7,
        }
    );
    assert_eq!(
        ownership.cluster_status(),
        peryx_driver::state::ClusterStatus {
            leader: None,
            term: 8,
            voters: Vec::new(),
        }
    );
    assert_eq!(
        ownership.transfer_home("flask", "west").await.unwrap(),
        Some(peryx_driver::state::TransferOutcome {
            from: "flask".to_owned(),
            to: "west".to_owned(),
            epoch: 9,
        }),
    );
}

#[tokio::test]
async fn test_local_durability_confirms_the_committed_scope() {
    use peryx_ha::BlobWriteDurability as _;

    let digest = Digest::of(b"content");
    assert_eq!(
        LocalDurability
            .confirm(peryx_ha::CommittedBlob::new(
                &digest,
                b"content".len() as u64,
                "pypi",
                peryx_ha::AuthorityEpoch(7),
                None,
                peryx_storage::blob::WriteEvidence::NodeLocal,
            ))
            .await,
        peryx_ha::WriteDurability::Confirmed {
            scope: peryx_storage::blob::BlobDurability::Filesystem,
        }
    );
}

#[test]
fn test_unavailable_completeness_rejects_queries() {
    use peryx_ha::AnalyticsCompleteness as _;

    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    assert_eq!(
        UnavailableCompleteness.assess(
            &meta,
            &[],
            &peryx_ha::CompletenessQuery {
                from_day: 1,
                to_day: 2,
                today: 3,
                repository: Some("pypi".to_owned()),
            },
        ),
        Err(peryx_ha::CompletenessError)
    );
}
