use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use peryx_core::Ecosystem;
use peryx_driver::authz::AuthorizationService;
use peryx_driver::serving::{CacheInspectDriver, CachePage, FsckDriver, NameDriver};
use peryx_driver::state::{AppState, Index, IndexKind, ServingState};
use peryx_driver::users::UserService;
use peryx_identity::{GrantScope, IndexAcl, PasswordPolicy, Role};
use peryx_policy::Policy;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use tower::ServiceExt as _;

const ADMIN_PASSWORD: &str = "administrator password";
const OPERATOR_PASSWORD: &str = "operator password";
const ECOSYSTEM: Ecosystem = Ecosystem::new("example");

#[derive(Clone, Copy)]
enum Failure {
    Pages,
    Counts,
    Fsck,
    Panic,
}

struct Inspector {
    failure: Option<Failure>,
}

impl CacheInspectDriver for Inspector {
    fn served_cache_pages(&self, _state: &ServingState, index_names: &[&str]) -> Result<Vec<CachePage>, String> {
        match self.failure {
            Some(Failure::Pages) => return Err("cannot read pages".to_owned()),
            Some(Failure::Panic) => panic!("cache inspector panicked"),
            Some(Failure::Counts | Failure::Fsck) | None => {}
        }
        Ok(vec![CachePage {
            index: index_names.first().unwrap().to_string(),
            resource: "flask".to_owned(),
            fetched_at_unix: 900,
            fresh_secs: Some(60),
            body_bytes: 11,
            record_bytes: 17,
            key: "pypi/flask".to_owned(),
        }])
    }

    fn served_cache_record_counts(&self, _state: &ServingState) -> Result<Vec<(String, u64)>, String> {
        if matches!(self.failure, Some(Failure::Counts)) {
            return Err("cannot count records".to_owned());
        }
        Ok(vec![("project_records".to_owned(), 1)])
    }
}

impl NameDriver for Inspector {
    fn normalize_name(&self, name: &str) -> String {
        name.to_lowercase()
    }
}

impl FsckDriver for Inspector {
    fn fsck_metadata(
        &self,
        _meta: &MetaStore,
        _blobs: &BlobStorage,
        out: &mut dyn std::io::Write,
    ) -> Result<u64, String> {
        if matches!(self.failure, Some(Failure::Fsck)) {
            return Err("cannot check metadata".to_owned());
        }
        writeln!(out, "metadata\texample\tinvalid row").map_err(|error| error.to_string())?;
        Ok(1)
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    app: axum::Router,
    blob_path: std::path::PathBuf,
    digest: Digest,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_failure(None).await
    }

    async fn failing(failure: Failure) -> Self {
        Self::with_failure(Some(failure)).await
    }

    async fn with_failure(failure: Option<Failure>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
        let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
        let authorization = AuthorizationService::new(meta.clone());
        for (name, password, role) in [
            ("Alice", ADMIN_PASSWORD, Role::Administrator),
            ("Olivia", OPERATOR_PASSWORD, Role::Operator),
        ] {
            let account = users.create(name).unwrap().id;
            users.set_password(&account, password).await.unwrap();
            authorization.grant(&account, role, GrantScope::Server).unwrap();
        }
        let blobs = BlobStorage::filesystem(directory.path().join("blobs"));
        let digest = blobs.blocking().put_bytes(b"payload").unwrap();
        let blob_path = blobs.filesystem_store().unwrap().path_for(&digest);
        let mut state = AppState::with_clock(
            meta,
            blobs,
            60,
            vec![Index {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                ecosystem: ECOSYSTEM,
                kind: IndexKind::Hosted { volatile: false },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            }],
            Arc::new(|| 1000),
        );
        let serving = Arc::get_mut(&mut state.serving).unwrap();
        serving.users = users;
        serving.authorization = authorization;
        let driver = Arc::new(Inspector { failure });
        state.register_capabilities(|registrar| {
            registrar.register_name(ECOSYSTEM, driver.clone());
            registrar.register_cache_inspect(ECOSYSTEM, driver.clone());
            registrar.register_fsck(ECOSYSTEM, driver);
        });
        Self {
            directory,
            app: crate::router(Arc::new(state)),
            blob_path,
            digest,
        }
    }

    async fn get(&self, uri: &str, credential: Option<(&str, &str)>) -> (StatusCode, String) {
        let mut request = Request::builder().method(Method::GET).uri(uri);
        if let Some((user, password)) = credential {
            request = request.header(
                header::AUTHORIZATION,
                format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
            );
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let status = response.status();
        if status == StatusCode::OK {
            assert_eq!(response.headers()[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        }
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }
}

/// The whole point of the endpoints: redb admits one holder, so an offline `peryx cache` run cannot
/// open the store the server is serving from, and only the in-process read answers.
#[tokio::test]
async fn test_cache_inspection_reads_a_store_no_second_process_can_open() {
    let fixture = Fixture::new().await;

    let refused = MetaStore::open(fixture.directory.path().join("peryx.redb")).unwrap_err();

    assert!(refused.is_database_already_open());
    for path in ["/+cache", "/+cache/size", "/+cache/fsck"] {
        let (status, _) = fixture.get(path, Some(("Alice", ADMIN_PASSWORD))).await;
        assert_eq!(status, StatusCode::OK);
    }
}

#[tokio::test]
async fn test_cache_list_reports_live_pages_and_blobs() {
    let fixture = Fixture::new().await;

    let (status, body) = fixture.get("/+cache", Some(("Alice", ADMIN_PASSWORD))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        format!(
            "kind\tindex\tresource\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n\
             index\tpypi\tflask\t\t100\t60\ttrue\t11\tpypi/flask\n\
             blob\t\t\t{}\t-\t-\t-\t7\t{}\n",
            fixture.digest.as_str(),
            fixture.blob_path.display()
        )
    );
}

#[tokio::test]
async fn test_cache_list_normalizes_the_resource_filter() {
    let fixture = Fixture::new().await;

    let (status, body) = fixture
        .get("/+cache?resource=Flask", Some(("Alice", ADMIN_PASSWORD)))
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        "kind\tindex\tresource\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n\
         index\tpypi\tflask\t\t100\t60\ttrue\t11\tpypi/flask\n"
    );
}

/// A digest names a blob, so the index pages cannot match it and are not read at all.
#[tokio::test]
async fn test_cache_list_filtered_by_digest_skips_index_pages() {
    let fixture = Fixture::new().await;

    let (status, body) = fixture
        .get(
            &format!("/+cache?digest={}", fixture.digest.as_str()),
            Some(("Alice", ADMIN_PASSWORD)),
        )
        .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        format!(
            "kind\tindex\tresource\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n\
             blob\t\t\t{}\t-\t-\t-\t7\t{}\n",
            fixture.digest.as_str(),
            fixture.blob_path.display()
        )
    );
}

#[tokio::test]
async fn test_cache_size_reports_live_counts() {
    let fixture = Fixture::new().await;

    let (status, body) = fixture.get("/+cache/size", Some(("Alice", ADMIN_PASSWORD))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        "index_pages\t1\nstale_index_pages\t1\nindex_bytes\t17\nblob_files\t1\nblob_bytes\t7\n\
         invalid_blob_paths\t0\nstage_files\t0\nstage_bytes\t0\nproject_records\t1\n"
    );
}

#[tokio::test]
async fn test_cache_fsck_reports_live_findings() {
    let fixture = Fixture::new().await;

    let (status, body) = fixture.get("/+cache/fsck", Some(("Alice", ADMIN_PASSWORD))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "metadata\texample\tinvalid row\nproblems\t1\n");
}

#[rstest]
#[case::missing(None, StatusCode::UNAUTHORIZED)]
#[case::operator(Some(("Olivia", OPERATOR_PASSWORD)), StatusCode::NOT_FOUND)]
#[tokio::test]
async fn test_cache_inspection_requires_administration_read(
    #[case] credential: Option<(&str, &str)>,
    #[case] expected: StatusCode,
) {
    let fixture = Fixture::new().await;

    for path in ["/+cache", "/+cache/size", "/+cache/fsck"] {
        let (status, _) = fixture.get(path, credential).await;
        assert_eq!(status, expected);
    }
}

#[tokio::test]
async fn test_cache_list_rejects_unknown_filters() {
    let fixture = Fixture::new().await;

    let (status, body) = fixture
        .get("/+cache?unknown=true", Some(("Alice", ADMIN_PASSWORD)))
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, r#"{"error":"invalid cache list query"}"#);
}

#[rstest]
#[case::list(
    Failure::Pages,
    "/+cache",
    "cache list failed: scan cached index pages: cannot read pages"
)]
#[case::size_pages(
    Failure::Pages,
    "/+cache/size",
    "cache size failed: scan cached index pages: cannot read pages"
)]
#[case::size_counts(Failure::Counts, "/+cache/size", "cache size failed: cannot count records")]
#[case::fsck(
    Failure::Fsck,
    "/+cache/fsck",
    "cache fsck failed: fsck ecosystem metadata: cannot check metadata"
)]
#[tokio::test]
async fn test_cache_inspection_surfaces_read_failures(
    #[case] failure: Failure,
    #[case] path: &str,
    #[case] expected: &str,
) {
    let fixture = Fixture::failing(failure).await;

    let (status, body) = fixture.get(path, Some(("Alice", ADMIN_PASSWORD))).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, serde_json::json!({"error": expected}).to_string());
}

#[tokio::test]
async fn test_cache_inspection_contains_worker_panics() {
    let fixture = Fixture::failing(Failure::Panic).await;

    let (status, body) = fixture.get("/+cache", Some(("Alice", ADMIN_PASSWORD))).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, r#"{"error":"cache inspection task failed"}"#);
}
