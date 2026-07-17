use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::Ecosystem;
use peryx_driver::state::{AppState, Index, IndexKind};
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};
use peryx_policy::{Policy, PolicyAction, PolicyDecisionState};
use peryx_storage::meta::{MetaError, MetaStore, NewPolicyDecision, PolicyDecisionQueryError};
use rstest::rstest;
use tower::ServiceExt as _;

const ADMIN_SECRET: &str = "admin-secret";
const READER_SECRET: &str = "reader-secret";

fn app() -> (tempfile::TempDir, MetaStore, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let state = AppState::new(
        meta.clone(),
        blobs,
        60,
        vec![Index {
            name: "private".to_owned(),
            route: "private".to_owned(),
            ecosystem: Ecosystem::Pypi,
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl {
                anonymous_read: true,
                tokens: vec![
                    NamedToken {
                        name: "admin".to_owned(),
                        secret: ADMIN_SECRET.to_owned(),
                        grants: vec![Grant {
                            projects: vec![Glob::new("*")],
                            actions: BTreeSet::from([Action::Write]),
                        }],
                        expires_at: None,
                    },
                    NamedToken {
                        name: "reader".to_owned(),
                        secret: READER_SECRET.to_owned(),
                        grants: vec![Grant {
                            projects: vec![Glob::new("*")],
                            actions: BTreeSet::from([Action::Read]),
                        }],
                        expires_at: None,
                    },
                ],
            },
        }],
    );
    (dir, meta, crate::router(Arc::new(state)))
}

fn decision(project: &str, state: PolicyDecisionState, evaluated_at_unix: i64) -> NewPolicyDecision<'_> {
    NewPolicyDecision {
        repository: "private",
        project,
        version: Some("1.0"),
        filename: Some("package-1.0.whl"),
        source: Some("pypi"),
        action: PolicyAction::Serve,
        state,
        rule: (state == PolicyDecisionState::Deny).then_some("blocked-project"),
        reason: (state == PolicyDecisionState::Deny).then_some("project is blocked"),
        evaluated_at_unix,
        next_eligible_at_unix: None,
    }
}

async fn get(app: &axum::Router, uri: &str, secret: Option<&str>) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().uri(uri);
    if let Some(secret) = secret {
        request = request.header(
            header::AUTHORIZATION,
            format!("Basic {}", STANDARD.encode(format!("user:{secret}"))),
        );
    }
    let response = app.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null))
}

#[tokio::test]
async fn test_policy_decisions_filters_authorized_repository_history() {
    let (_dir, meta, app) = app();
    meta.record_policy_decision(decision("alpha", PolicyDecisionState::Allow, 10))
        .unwrap();
    let denied = meta
        .record_policy_decision(decision("beta", PolicyDecisionState::Deny, 20))
        .unwrap();
    let mut other = decision("gamma", PolicyDecisionState::Deny, 21);
    other.repository = "other";
    meta.record_policy_decision(other).unwrap();

    let (status, document) = get(
        &app,
        "/+policy/decisions?repository=private&state=deny&rule=blocked-project&source=pypi&from=15&to=25",
        Some(ADMIN_SECRET),
    )
    .await;

    assert_eq!(
        (status, document),
        (
            StatusCode::OK,
            serde_json::json!({
                "decisions": [{
                    "id": denied.id,
                    "repository": "private",
                    "project": "beta",
                    "version": "1.0",
                    "filename": "package-1.0.whl",
                    "source": "pypi",
                    "action": "serve",
                    "state": "deny",
                    "rule": "blocked-project",
                    "reason": "project is blocked",
                    "evaluated_at_unix": 20,
                    "input_generation": {"repository": 0, "catalog": 0, "policy": 0},
                    "next_eligible_at_unix": null,
                    "fresh": true
                }],
                "next_cursor": null
            }),
        )
    );
}

#[rstest]
#[case::anonymous("/+policy/decisions?repository=private", None, StatusCode::UNAUTHORIZED)]
#[case::reader("/+policy/decisions?repository=private", Some(READER_SECRET), StatusCode::FORBIDDEN)]
#[case::unknown("/+policy/decisions?repository=missing", Some(ADMIN_SECRET), StatusCode::NOT_FOUND)]
#[tokio::test]
async fn test_policy_decisions_enforces_repository_authorization(
    #[case] uri: &str,
    #[case] secret: Option<&str>,
    #[case] expected: StatusCode,
) {
    let (_dir, _meta, app) = app();

    assert_eq!(get(&app, uri, secret).await.0, expected);
}

#[rstest]
#[case::limit("/+policy/decisions?repository=private&limit=0", "limit must be between 1 and 100")]
#[case::cursor("/+policy/decisions?repository=private&cursor=bad", "invalid policy decision cursor")]
#[tokio::test]
async fn test_policy_decisions_rejects_invalid_pagination(#[case] uri: &str, #[case] error: &str) {
    let (_dir, _meta, app) = app();

    assert_eq!(
        get(&app, uri, Some(ADMIN_SECRET)).await,
        (StatusCode::BAD_REQUEST, serde_json::json!({"error": error}))
    );
}

#[tokio::test]
async fn test_policy_decision_error_response_hides_store_failures() {
    let response = crate::handlers::policy_decision_error_response(&PolicyDecisionQueryError::Store(
        MetaError::DriverPrecondition("sensitive detail".to_owned()),
    ));
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();

    assert_eq!(
        (status, serde_json::from_slice::<serde_json::Value>(&body).unwrap()),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"error": "policy decision query failed"}),
        )
    );
}
