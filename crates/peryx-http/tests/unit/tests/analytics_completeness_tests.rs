use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::{Ecosystem, NodeRole, TopologyConfig, TopologyMember};
use peryx_driver::authz::AuthorizationService;
use peryx_driver::state::{AppState, Index, IndexKind};
use peryx_driver::users::UserService;
use peryx_events::metrics::{Clock, Metrics};
use peryx_ha::{AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, AuthorityEpoch, IntervalId, ProducerId};
use peryx_ha_distributed::{AnalyticsReceiver, DEFAULT_APPLY_LIMITS, DistributedAnalyticsCompleteness};
use peryx_identity::{Action, Glob, Grant, GrantScope, IndexAcl, NamedToken, PasswordPolicy, Role};
use peryx_policy::Policy;
use peryx_storage::meta::MetaStore;
use rstest::rstest;
use serde_json::Value;
use tower::ServiceExt as _;

const ADMIN_SECRET: &str = "admin-secret";
const READER_SECRET: &str = "reader-secret";
const USER_PASSWORD: &str = "local password";
const SECONDS_PER_DAY: i64 = 86_400;
const TODAY: i64 = 20_000;

fn fixed_clock() -> Clock {
    let unix = TODAY * SECONDS_PER_DAY;
    Arc::new(move || unix)
}

fn writer(node: &str, dc: &str) -> TopologyMember {
    TopologyMember {
        node: node.to_owned(),
        dc: dc.to_owned(),
        address: String::new(),
        role: NodeRole::Writer,
    }
}

fn replica(node: &str, dc: &str) -> TopologyMember {
    TopologyMember {
        role: NodeRole::Replica,
        ..writer(node, dc)
    }
}

/// One producer's sealed-day batch: `day` is both the UTC day and the interval sequence, matching the
/// producer's own day-as-sequence stamping.
fn batch(producer: &str, epoch: u64, day: i64, rows: &[(&str, u64, u64)]) -> AnalyticsBatch {
    AnalyticsBatch {
        interval: IntervalId {
            producer: ProducerId(producer.to_owned()),
            epoch: AuthorityEpoch(epoch),
            sequence: u64::try_from(day).unwrap(),
        },
        rows: rows
            .iter()
            .map(|(repository, downloads, bytes)| AggregateRow {
                key: AggregateKey {
                    day,
                    repository: (*repository).to_owned(),
                    project: "flask".to_owned(),
                    version: String::new(),
                    source: String::new(),
                },
                delta: AggregateDelta {
                    downloads: *downloads,
                    bytes: *bytes,
                },
            })
            .collect(),
    }
}

fn seeded(batches: &[AnalyticsBatch]) -> Vec<u8> {
    let mut receiver = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);
    for batch in batches {
        receiver.apply(batch).unwrap();
    }
    receiver.encode()
}

/// What to persist under the receiver apply key before the query runs.
enum Snapshot<'a> {
    /// Nothing persisted: a store that has never applied a pull.
    Absent,
    /// A valid converged receiver folded from these batches.
    Batches(&'a [AnalyticsBatch]),
    /// Bytes the current build cannot restore.
    Malformed,
    /// A store whose analytics table is the wrong shape, so the read itself fails.
    Corrupt,
}

async fn app(members: Vec<TopologyMember>, snapshot: Snapshot<'_>) -> (tempfile::TempDir, Arc<AppState>) {
    app_with_completeness(members, snapshot, true).await
}

async fn app_with_completeness(
    members: Vec<TopologyMember>,
    snapshot: Snapshot<'_>,
    enabled: bool,
) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    let authorization = AuthorizationService::new(meta.clone());
    for (name, role, scope) in [
        ("Alice", Role::Administrator, GrantScope::Server),
        ("Olivia", Role::Operator, GrantScope::Server),
        (
            "Rita",
            Role::RepositoryReader,
            GrantScope::Repository {
                name: "private".to_owned(),
            },
        ),
    ] {
        let user = users.create(name).unwrap();
        users.set_password(&user.id, USER_PASSWORD).await.unwrap();
        authorization.grant(&user.id, role, scope).unwrap();
    }
    if let Snapshot::Batches(batches) = snapshot {
        meta.analytics().save_apply(&seeded(batches)).unwrap();
    }
    if matches!(snapshot, Snapshot::Malformed) {
        meta.analytics().save_apply(b"not a receiver snapshot").unwrap();
    }
    drop(authorization);
    drop(users);
    drop(meta);
    if matches!(snapshot, Snapshot::Corrupt) {
        let database = redb::Database::open(&path).unwrap();
        let transaction = database.begin_write().unwrap();
        transaction
            .delete_table(redb::TableDefinition::<&str, &[u8]>::new("analytics"))
            .unwrap();
        transaction
            .open_table(redb::TableDefinition::<&str, u64>::new("analytics"))
            .unwrap();
        transaction.commit().unwrap();
    }
    let meta = MetaStore::open_existing(path).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta.clone(), blobs, 60, indexes());
    state.users = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    state.metrics = Metrics::start_durable(meta.analytics(), None, fixed_clock());
    state.set_availability_topology(TopologyConfig {
        members,
        ..TopologyConfig::default()
    });
    if enabled {
        state.set_analytics_completeness(Arc::new(DistributedAnalyticsCompleteness));
    }
    (dir, Arc::new(state))
}

#[tokio::test]
async fn test_completeness_is_unavailable_without_distributed_analytics() {
    let (_dir, state) = app_with_completeness(Vec::new(), Snapshot::Absent, false).await;

    let (status, _, _) = get(
        &state,
        &format!("/+analytics/completeness?{}", window()),
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

fn indexes() -> Vec<Index> {
    vec![
        index(
            "private",
            vec![
                token("reader", READER_SECRET, Action::Read),
                token("admin", ADMIN_SECRET, Action::Write),
            ],
        ),
        index("other", Vec::new()),
    ]
}

fn index(route: &str, tokens: Vec<NamedToken>) -> Index {
    Index {
        name: route.to_owned(),
        route: route.to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: false,
            tokens,
        },
    }
}

fn token(name: &str, secret: &str, action: Action) -> NamedToken {
    NamedToken {
        name: name.to_owned(),
        secret: secret.to_owned(),
        grants: vec![Grant {
            projects: vec![Glob::new("*")],
            actions: BTreeSet::from([action]),
        }],
        expires_at: None,
    }
}

async fn get(
    state: &Arc<AppState>,
    uri: &str,
    credential: Option<(&str, &str)>,
) -> (StatusCode, header::HeaderMap, Value) {
    let mut request = Request::builder().uri(uri);
    if let Some((user, password)) = credential {
        request = request.header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("{user}:{password}"))),
        );
    }
    let response = crate::router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn window() -> String {
    format!("from={}&to={}", (TODAY - 30) * SECONDS_PER_DAY, TODAY * SECONDS_PER_DAY)
}

fn two_writers() -> Vec<TopologyMember> {
    vec![writer("east", "east-dc"), writer("west", "west-dc")]
}

/// East folded days 19998 and 19999; west folded day 19999. Both reach the cluster frontier at 19999.
fn caught_up() -> Vec<AnalyticsBatch> {
    vec![
        batch("east", 1, TODAY - 2, &[("private", 2, 20)]),
        batch("east", 1, TODAY - 1, &[("private", 3, 30)]),
        batch("west", 1, TODAY - 1, &[("other", 5, 50)]),
    ]
}

#[tokio::test]
async fn test_operator_sees_a_complete_picture_with_producer_frontiers() {
    let (_dir, state) = app(two_writers(), Snapshot::Batches(&caught_up())).await;

    let (status, headers, body) = get(
        &state,
        &format!("/+analytics/completeness?{}", window()),
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(body["completeness"], "complete");
    assert_eq!(body["frontier_day"], TODAY - 1);
    assert_eq!(body["required_day"], TODAY - 1);
    assert_eq!(body["lag_days"], 1);
    assert_eq!(body["totals"], serde_json::json!({"downloads": 10, "bytes": 100}));
    assert_eq!(
        body["buckets"],
        serde_json::json!([
            {"day": TODAY - 2, "start_unix": (TODAY - 2) * SECONDS_PER_DAY, "end_unix": (TODAY - 1) * SECONDS_PER_DAY, "downloads": 2, "bytes": 20},
            {"day": TODAY - 1, "start_unix": (TODAY - 1) * SECONDS_PER_DAY, "end_unix": TODAY * SECONDS_PER_DAY, "downloads": 8, "bytes": 80},
        ])
    );
    let verdicts: Vec<(&str, &str)> = body["producers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|producer| {
            (
                producer["producer"].as_str().unwrap(),
                producer["state"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(verdicts, [("east", "complete"), ("west", "complete")]);
}

#[tokio::test]
async fn test_a_trailing_producer_marks_the_range_delayed() {
    let batches = [
        batch("east", 1, TODAY - 1, &[("private", 3, 30)]),
        batch("west", 1, TODAY - 4, &[("other", 5, 50)]),
    ];
    let (_dir, state) = app(two_writers(), Snapshot::Batches(&batches)).await;

    let (status, _, body) = get(
        &state,
        &format!("/+analytics/completeness?{}", window()),
        Some(("Alice", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["completeness"], "delayed");
    let west = body["producers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["producer"] == "west")
        .unwrap();
    assert_eq!(west["state"], "delayed");
    assert_eq!(west["accepted_day"], TODAY - 4);
    assert_eq!(west["accepted_epoch"], 1);
    assert_eq!(west["dc"], "west-dc");
}

#[tokio::test]
async fn test_a_silent_writer_marks_the_range_unavailable() {
    let members = vec![writer("east", "east-dc"), writer("south", "south-dc")];
    let batches = [batch("east", 1, TODAY - 1, &[("private", 3, 30)])];
    let (_dir, state) = app(members, Snapshot::Batches(&batches)).await;

    let (status, _, body) = get(
        &state,
        &format!("/+analytics/completeness?{}", window()),
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["completeness"], "unavailable");
    let south = body["producers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["producer"] == "south")
        .unwrap();
    assert_eq!(south["state"], "unavailable");
    assert_eq!(south["accepted_day"], Value::Null);
    assert_eq!(south["accepted_epoch"], Value::Null);
}

#[tokio::test]
async fn test_replica_members_are_not_expected_producers() {
    let members = vec![writer("east", "east-dc"), replica("mirror", "east-dc")];
    let batches = [batch("east", 1, TODAY - 1, &[("private", 3, 30)])];
    let (_dir, state) = app(members, Snapshot::Batches(&batches)).await;

    let (_, _, body) = get(
        &state,
        &format!("/+analytics/completeness?{}", window()),
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;

    assert_eq!(body["completeness"], "complete");
    let producers: Vec<&str> = body["producers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["producer"].as_str().unwrap())
        .collect();
    assert_eq!(producers, ["east"]);
}

#[tokio::test]
async fn test_no_configured_writer_is_unavailable_but_still_reports_totals() {
    let (_dir, state) = app(Vec::new(), Snapshot::Batches(&caught_up())).await;

    let (status, _, body) = get(
        &state,
        &format!("/+analytics/completeness?{}", window()),
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["completeness"], "unavailable");
    assert_eq!(body["frontier_day"], Value::Null);
    assert_eq!(body["lag_days"], Value::Null);
    assert_eq!(body["producers"], serde_json::json!([]));
    assert_eq!(body["totals"], serde_json::json!({"downloads": 10, "bytes": 100}));
}

#[tokio::test]
async fn test_absent_snapshot_reads_as_an_empty_picture() {
    let (_dir, state) = app(two_writers(), Snapshot::Absent).await;

    let (status, _, body) = get(
        &state,
        &format!("/+analytics/completeness?{}", window()),
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["completeness"], "unavailable");
    assert_eq!(body["buckets"], serde_json::json!([]));
    assert_eq!(body["totals"], serde_json::json!({"downloads": 0, "bytes": 0}));
}

#[tokio::test]
async fn test_a_repository_reader_sees_scoped_totals_without_the_producer_frontier() {
    let (_dir, state) = app(two_writers(), Snapshot::Batches(&caught_up())).await;

    let (status, _, body) = get(
        &state,
        &format!("/+analytics/completeness?repository=private&{}", window()),
        Some(("Rita", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["completeness"], "complete");
    assert_eq!(body["totals"], serde_json::json!({"downloads": 5, "bytes": 50}));
    assert!(body.get("producers").is_none());
    assert!(body.get("frontier_day").is_none());
    assert!(body.get("lag_days").is_none());
}

#[tokio::test]
async fn test_an_operator_may_narrow_to_a_repository_and_keep_the_frontier() {
    let (_dir, state) = app(two_writers(), Snapshot::Batches(&caught_up())).await;

    let (status, _, body) = get(
        &state,
        &format!("/+analytics/completeness?repository=other&{}", window()),
        Some(("Alice", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["totals"], serde_json::json!({"downloads": 5, "bytes": 50}));
    assert!(body.get("producers").is_some());
}

#[tokio::test]
async fn test_a_repository_upload_token_reads_its_own_repository() {
    let (_dir, state) = app(two_writers(), Snapshot::Batches(&caught_up())).await;

    let (status, _, body) = get(
        &state,
        &format!("/+analytics/completeness?repository=private&{}", window()),
        Some(("__token__", READER_SECRET)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["totals"], serde_json::json!({"downloads": 5, "bytes": 50}));
    assert!(body.get("producers").is_none());
}

#[rstest]
#[case::anonymous(None, "", StatusCode::UNAUTHORIZED)]
#[case::repo_reader_operator_wide(Some(("Rita", USER_PASSWORD)), "", StatusCode::NOT_FOUND)]
#[case::repo_reader_forbidden_repository(Some(("Rita", USER_PASSWORD)), "repository=other&", StatusCode::NOT_FOUND)]
#[case::operator_unknown_repository(Some(("Olivia", USER_PASSWORD)), "repository=ghost&", StatusCode::NOT_FOUND)]
#[case::token_without_repository(Some(("__token__", READER_SECRET)), "", StatusCode::UNAUTHORIZED)]
#[case::token_without_read(Some(("__token__", ADMIN_SECRET)), "repository=private&", StatusCode::FORBIDDEN)]
#[tokio::test]
async fn test_authorization_is_enforced(
    #[case] credential: Option<(&str, &str)>,
    #[case] prefix: &str,
    #[case] expected: StatusCode,
) {
    let (_dir, state) = app(two_writers(), Snapshot::Batches(&caught_up())).await;

    let (status, _, _) = get(
        &state,
        &format!("/+analytics/completeness?{prefix}{}", window()),
        credential,
    )
    .await;

    assert_eq!(status, expected);
}

#[tokio::test]
async fn test_the_bucket_page_paginates_over_an_opaque_cursor() {
    let batches: Vec<AnalyticsBatch> = (0..4)
        .map(|offset| batch("east", 1, TODAY - 1 - offset, &[("private", 1, 10)]))
        .collect();
    let (_dir, state) = app(vec![writer("east", "east-dc")], Snapshot::Batches(&batches)).await;

    let (status, _, page1) = get(
        &state,
        &format!("/+analytics/completeness?limit=2&{}", window()),
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let days: Vec<i64> = page1["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["day"].as_i64().unwrap())
        .collect();
    assert_eq!(days, [TODAY - 4, TODAY - 3]);

    let cursor = page1["next_cursor"].as_str().unwrap();
    let (_, _, page2) = get(
        &state,
        &format!("/+analytics/completeness?limit=2&cursor={cursor}&{}", window()),
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;
    let days: Vec<i64> = page2["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["day"].as_i64().unwrap())
        .collect();
    assert_eq!(days, [TODAY - 2, TODAY - 1]);
    assert_eq!(page2["next_cursor"], Value::Null);
}

#[tokio::test]
async fn test_an_invalid_limit_is_rejected() {
    let (_dir, state) = app(two_writers(), Snapshot::Batches(&caught_up())).await;

    let (status, _, body) = get(
        &state,
        &format!("/+analytics/completeness?limit=0&{}", window()),
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "limit must be between 1 and 100");
}

#[tokio::test]
async fn test_a_malformed_query_string_is_rejected() {
    let (_dir, state) = app(two_writers(), Snapshot::Batches(&caught_up())).await;

    let (status, _, body) = get(
        &state,
        "/+analytics/completeness?from=not-a-timestamp",
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid analytics query");
}

#[tokio::test]
async fn test_a_malformed_snapshot_is_unavailable() {
    let (_dir, state) = app(two_writers(), Snapshot::Malformed).await;

    let (status, _, _) = get(
        &state,
        &format!("/+analytics/completeness?{}", window()),
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_an_unreadable_analytics_store_is_unavailable() {
    let (_dir, state) = app(two_writers(), Snapshot::Corrupt).await;

    let (status, _, _) = get(
        &state,
        &format!("/+analytics/completeness?{}", window()),
        Some(("Olivia", USER_PASSWORD)),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
