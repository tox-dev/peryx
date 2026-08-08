//! A wired state over temporary stores and a mock upstream, in the shapes the tests need.

use super::*;
use peryx_identity::IndexAcl;

pub struct Harness {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) server: MockServer,
    pub(crate) state: Arc<AppState>,
    pub(crate) clock: Arc<AtomicI64>,
}
/// A cache (`pypi`) of the mock, a hosted store (`hosted`), and a virtual index (`root/pypi`) that
/// layers the hosted store in front of the cache. `token`/`volatile` tune the hosted store.
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
/// A harness whose stale-on-error bound the caller chooses; `0` serves stale without limit.
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
                upload: Some(1),
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
    state.max_stale_secs = options.max_stale_secs;
    let state = crate::tests::wired(state);
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
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            max_stale_secs: DEFAULT_MAX_STALE_SECS,
            offline: false,
            upstream_concurrency: peryx_driver::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
        }
    }
}
pub async fn harness() -> Harness {
    harness_with(true, true).await
}

fn clone_index(index: &Index) -> Index {
    Index {
        name: index.name.clone(),
        route: index.route.clone(),
        ecosystem: index.ecosystem,
        kind: match &index.kind {
            IndexKind::Cached { client, offline } => IndexKind::Cached {
                client: client.clone(),
                offline: *offline,
            },
            IndexKind::Hosted { volatile } => IndexKind::Hosted { volatile: *volatile },
            IndexKind::Virtual { layers, upload } => IndexKind::Virtual {
                layers: layers.clone(),
                upload: *upload,
            },
        },
        policy: index.policy.clone(),
        acl: index.acl.clone(),
    }
}

pub fn restarted_state(harness: &Harness) -> Arc<AppState> {
    restarted_state_with_ttl(harness, harness.state.ttl_secs)
}

pub fn restarted_state_with_ttl(harness: &Harness, ttl_secs: i64) -> Arc<AppState> {
    let clock = harness.clock.clone();
    let mut state = AppState::with_clock(
        harness.state.meta.clone(),
        harness.state.blobs.clone(),
        ttl_secs,
        harness.state.indexes.iter().map(clone_index).collect(),
        Arc::new(move || clock.load(Ordering::Relaxed)),
    );
    state.max_stale_secs = harness.state.max_stale_secs;
    crate::tests::wired(state)
}

/// A read-only replica over the harness's committed store, for the derived-view frontier gating a
/// replica applies. It shares the store, so it serves what the primary published, but its read-only
/// posture is what holds a read behind the readable frontier.
pub fn replica_state(harness: &Harness) -> Arc<AppState> {
    let clock = harness.clock.clone();
    let mut state = AppState::with_clock(
        harness.state.meta.clone(),
        harness.state.blobs.clone(),
        harness.state.ttl_secs,
        harness.state.indexes.iter().map(clone_index).collect(),
        Arc::new(move || clock.load(Ordering::Relaxed)),
    );
    state.max_stale_secs = harness.state.max_stale_secs;
    state.read_only = true;
    crate::tests::wired(state)
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
    state.upstream_routes.insert("pypi".to_owned(), router);
    crate::tests::wired(state)
}

pub async fn promotion_harness() -> Harness {
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
                upload: Some(2),
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
    ];
    let state = crate::tests::wired(AppState::with_clock(
        meta,
        blobs,
        60,
        indexes,
        Arc::new(move || ticks.load(Ordering::Relaxed)),
    ));
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
    Policy::compile(&neutral, crate::normalize_name).with_rules(compile_rules(&pypi).unwrap())
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
/// Build a mirror harness whose cached flask page was fetched at `fetched_at`, and whose upstream
/// is unreachable, so the only question is whether the stale copy may still answer.
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

/// A stand-in ownership authority for control-routing tests. `committed` is the epoch a control reads
/// as its lease (the possibly-lagging local view); `current` is the authoritative epoch the fence
/// admits against. When the two differ, a control that leased `committed` is fenced, modelling a home
/// transfer that advanced the authority after the control started.
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
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<peryx_driver::state::TransferOutcome>, peryx_driver::state::OwnershipError> {
        Ok(None)
    }
}

/// Install `authority` as the process ownership group, so control mutations route through its epoch fence.
pub fn install_authority(state: &Arc<AppState>, authority: AuthorityDouble) {
    state.set_ownership_authority(Arc::new(authority));
}
