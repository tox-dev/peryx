use super::*;
use peryx_identity::IndexAcl;
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
    let indexes = vec![
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
            name: "root/pypi".to_owned(),
            route: "root/pypi".to_owned(),
            policy: overlay_policy,
            acl: IndexAcl::default(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: vec![1, 0],
                write_target: Some(1),
            },
        },
    ];
    let mut state = AppState::with_limits(
        meta,
        blobs,
        60,
        indexes,
        Arc::new(move || ticks.load(Ordering::Relaxed)),
        peryx_driver::rate_limit::RateLimitConfig::default(),
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
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            max_stale_secs: DEFAULT_MAX_STALE_SECS,
            offline: false,
            upstream_concurrency: peryx_driver::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
            distributed: false,
        }
    }
}
pub async fn harness() -> Harness {
    harness_with(true, true).await
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
    pub homed: bool,
}

#[async_trait::async_trait]
impl peryx_driver::state::OwnershipAuthority for AuthorityDouble {
    async fn has_home(&self, _authority: &str) -> bool {
        self.homed
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.committed
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        self.current != 0 && presented == self.current
    }

    async fn claim_home(
        &self,
        _authority: &str,
    ) -> Result<peryx_driver::state::HomeClaim, peryx_driver::state::OwnershipError> {
        Ok(peryx_driver::state::HomeClaim::AlreadyHomed)
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
            topology: peryx_core::TopologyConfig::default(),
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
    async fn has_home(&self, authority: &str) -> bool {
        match self.ownership() {
            Some(owner) => owner.has_home(authority).await,
            None => true,
        }
    }

    async fn claim_home(&self, authority: &str) -> Result<peryx_ha::HomeClaim, peryx_ha::OwnershipError> {
        match self.ownership() {
            Some(owner) => owner.claim_home(authority).await,
            None => Ok(peryx_ha::HomeClaim::AlreadyHomed),
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
            scope: write.local_durability(),
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
        homed: true,
    }));

    assert!(ownership.has_home("flask").await);
    assert_eq!(ownership.committed_epoch("flask").await, 7);
    assert!(ownership.admit_epoch("flask", 8).await);
    assert_eq!(
        ownership.claim_home("flask").await.unwrap(),
        peryx_driver::state::HomeClaim::AlreadyHomed
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
                "pypi",
                peryx_ha::AuthorityEpoch(7),
                None,
                peryx_storage::blob::BlobDurability::Filesystem,
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
