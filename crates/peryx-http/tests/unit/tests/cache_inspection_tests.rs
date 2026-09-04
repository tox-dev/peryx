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
/// Registered alongside [`ECOSYSTEM`] out of sorted order. Four of them, not two, because the
/// registry is a hash map: with one other ecosystem an unsorted report lands in the right order half
/// the time by luck, and the test would pass against a missing sort.
const OTHER_ECOSYSTEMS: [(Ecosystem, &str); 3] = [
    (Ecosystem::new("zulu"), "requests"),
    (Ecosystem::new("alpha"), "numpy"),
    (Ecosystem::new("earlier"), "django"),
];

#[derive(Clone, Copy)]
enum Failure {
    /// Not a driver failure: the fixture roots the blob store at a regular file instead.
    BlobScan,
    Pages,
    Counts,
    Fsck,
    Panic,
}

struct Inspector {
    failure: Option<Failure>,
    resource: &'static str,
}

impl CacheInspectDriver for Inspector {
    fn served_cache_pages(&self, _state: &ServingState, index_names: &[&str]) -> Result<Vec<CachePage>, String> {
        match self.failure {
            Some(Failure::Pages) => return Err("cannot read pages".to_owned()),
            Some(Failure::Panic) => panic!("cache inspector panicked"),
            Some(Failure::BlobScan | Failure::Counts | Failure::Fsck) | None => {}
        }
        Ok(vec![CachePage {
            index: index_names.first().unwrap().to_string(),
            resource: self.resource.to_owned(),
            fetched_at_unix: 900,
            fresh_secs: Some(60),
            body_bytes: 11,
            record_bytes: 17,
            key: format!("pypi/{}", self.resource),
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
        _indexes: &[peryx_driver::Index],
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
        Self::build(failure, false).await
    }

    /// A second index whose name is longer, and a second ecosystem registered ahead of the one that
    /// sorts first, so both orderings the report promises have something to reorder.
    async fn two_ecosystems() -> Self {
        Self::build(None, true).await
    }

    async fn build(failure: Option<Failure>, second: bool) -> Self {
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
        if matches!(failure, Some(Failure::BlobScan)) {
            // A blob scan skips a root that does not exist and reports no blobs, so breaking the
            // root would be silently ignored. A regular file where the digest directory belongs is
            // a root that exists and cannot be read.
            let digests = directory.path().join("blobs").join("sha256");
            std::fs::remove_dir_all(&digests).unwrap();
            std::fs::write(&digests, b"not a directory").unwrap();
        }
        let index = |name: &str, ecosystem| Index {
            name: name.to_owned(),
            route: name.to_owned(),
            ecosystem,
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        };
        let mut indexes = vec![index("pypi", ECOSYSTEM)];
        if second {
            indexes.push(index("pypi-mirror", OTHER_ECOSYSTEMS[0].0.clone()));
        }
        let mut state = AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 1000));
        let serving = Arc::get_mut(&mut state.serving).unwrap();
        serving.users = users;
        serving.authorization = authorization;
        let driver = Arc::new(Inspector {
            failure,
            resource: "flask",
        });
        state.register_capabilities(|registrar| {
            registrar.register_name(ECOSYSTEM, driver.clone());
            registrar.register_cache_inspect(ECOSYSTEM, driver.clone());
            registrar.register_fsck(ECOSYSTEM, driver);
            if second {
                for (ecosystem, resource) in OTHER_ECOSYSTEMS {
                    let other = Arc::new(Inspector {
                        failure: None,
                        resource,
                    });
                    registrar.register_name(ecosystem.clone(), other.clone());
                    registrar.register_cache_inspect(ecosystem, other);
                }
            }
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

/// Longest first: a key belongs to the most specific index whose name prefixes it, so a page lands
/// under `pypi-mirror` and not under the shorter `pypi` that prefixes the same key.
#[tokio::test]
async fn test_cache_list_attributes_pages_to_the_longest_index_name() {
    let fixture = Fixture::two_ecosystems().await;

    let (status, body) = fixture.get("/+cache", Some(("Alice", ADMIN_PASSWORD))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(indexed_column(&body, 1), ["pypi-mirror"; 4], "{body}");
}

/// The driver registry is a hash map, so without an order of its own the two ecosystems swap places
/// between runs and an operator diffing yesterday's report against today's reads the swap as change.
#[tokio::test]
async fn test_cache_list_orders_ecosystems_by_name() {
    let fixture = Fixture::two_ecosystems().await;

    let (status, body) = fixture.get("/+cache", Some(("Alice", ADMIN_PASSWORD))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        indexed_column(&body, 2),
        ["numpy", "django", "flask", "requests"],
        "{body}"
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
#[case::list_blobs(Failure::BlobScan, "/+cache", "cache list failed: scan blob files")]
#[case::size_blobs(Failure::BlobScan, "/+cache/size", "cache size failed: scan blob files")]
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

fn indexed_column(body: &str, column: usize) -> Vec<&str> {
    body.lines()
        .filter(|line| line.starts_with("index\t"))
        .map(|line| line.split('\t').nth(column).unwrap())
        .collect()
}
