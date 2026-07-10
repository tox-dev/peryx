//! A wired state over temporary stores and a mock upstream, in the shapes the tests need.

use super::*;

pub struct Harness {
    pub(crate) _dir: tempfile::TempDir,
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
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let ticks = clock.clone();
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: mirror_policy,
        },
        Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            policy: local_policy,
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted {
                upload_token: token.then(|| "s3cret".to_owned()),
                volatile,
            },
        },
        Index {
            name: "root/pypi".to_owned(),
            route: "root/pypi".to_owned(),
            policy: overlay_policy,
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Virtual {
                layers: vec![1, 0],
                upload: Some(1),
            },
        },
    ];
    let mut state = AppState::with_clock(
        meta,
        blobs,
        60,
        indexes,
        Arc::new(move || ticks.load(Ordering::Relaxed)),
    );
    state.max_stale_secs = max_stale_secs;
    let state = crate::tests::wired(state);
    Harness {
        _dir: dir,
        server,
        state,
        clock,
    }
}
pub async fn harness() -> Harness {
    harness_with(true, true).await
}
pub async fn promotion_harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let ticks = clock.clone();
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
        },
        Index {
            name: "staging".to_owned(),
            route: "staging".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted {
                upload_token: Some("s3cret".to_owned()),
                volatile: true,
            },
            policy: Policy::default(),
        },
        Index {
            name: "prod".to_owned(),
            route: "prod".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted {
                upload_token: Some("s3cret".to_owned()),
                volatile: true,
            },
            policy: Policy::default(),
        },
        Index {
            name: "release".to_owned(),
            route: "release".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Virtual {
                layers: vec![2, 0],
                upload: Some(2),
            },
            policy: Policy::default(),
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
        _dir: dir,
        server,
        state,
        clock,
    }
}
pub fn policy(configure: impl FnOnce(&mut PolicyConfig, &mut PypiPolicyConfig)) -> Policy {
    let mut neutral = PolicyConfig::default();
    let mut pypi = PypiPolicyConfig::default();
    configure(&mut neutral, &mut pypi);
    Policy::compile(&neutral).with_rules(compile_rules(&pypi).unwrap())
}
pub fn put_raw_project_status(path: &Path, key: &str, value: &[u8]) {
    let db = redb::Database::create(path).unwrap();
    let table: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("project_status");
    let txn = db.begin_write().unwrap();
    txn.open_table(table).unwrap().insert(key, value).unwrap();
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
