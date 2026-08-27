use peryx_storage::meta::MetaError;

use super::*;

#[test]
fn test_upload_status_response_maps_policy_and_store_errors() {
    assert!(upload_status_response(Ok(ProjectStatus::Active), "root/pypi", "flask").is_none());
    let archived = upload_status_response(Ok(ProjectStatus::Archived), "root/pypi", "flask").unwrap();
    assert_eq!(archived.response.status(), StatusCode::FORBIDDEN);
    assert_eq!(archived.result, "denied");
    assert_eq!(archived.reason, "project \"flask\" is archived; uploads are disabled");

    let failure = upload_status_response(Err(CacheError::Meta(meta_error())), "root/pypi", "flask").unwrap();
    assert_eq!(failure.response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(failure.result, "failure");
    assert!(failure.reason.contains("metadata store error"));
}

#[test]
fn test_upload_quota_failure_preserves_the_storage_fault() {
    let failure = upload_quota_result::<(), _>(Err(CacheError::Meta(meta_error())), "root/pypi", "flask").unwrap_err();

    assert_eq!(failure.response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(failure.result, "failure");
    assert!(failure.reason.contains("metadata store error"));
}

#[test]
fn test_upload_quota_failure_describes_the_accounting_fault() {
    let failure = upload_quota_result::<(), _>(
        Err(peryx_storage::meta::QuotaError::Empty { field: "project" }),
        "root/pypi",
        "flask",
    )
    .unwrap_err();

    assert_eq!(failure.response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(failure.result, "failure");
    assert_eq!(failure.reason, "quota accounting error: project must not be empty");
}

fn meta_error() -> MetaError {
    MetaError::Decode(serde_json::from_str::<serde_json::Value>("not json").unwrap_err())
}

fn record(state: OperationState) -> peryx_storage::meta::OperationOutcomeRecord {
    peryx_storage::meta::OperationOutcomeRecord {
        state,
        response: b"upload accepted".to_vec(),
        expiry_unix: None,
        updated_at_unix: 0,
    }
}

#[test]
fn test_claim_short_circuit_replays_a_published_operation() {
    let response = claim_short_circuit(Ok(OperationClaim::Existing(record(OperationState::Published))))
        .expect("a published operation replays");
    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn test_claim_short_circuit_proceeds_for_a_fresh_or_still_running_claim() {
    assert!(claim_short_circuit(Ok(OperationClaim::Admitted)).is_none());
    assert!(claim_short_circuit(Ok(OperationClaim::Existing(record(OperationState::Pending)))).is_none());
}

#[test]
fn test_claim_short_circuit_fails_closed_when_the_claim_cannot_be_read() {
    let response = claim_short_circuit(Err(meta_error())).expect("an unreadable claim fails closed");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

fn audit(headers: &HeaderMap) -> UploadAudit<'_> {
    UploadAudit {
        headers,
        actor: None,
        request_id: None,
        created_at_unix: 0,
        index: "root-pypi",
        route: "root/pypi",
        hosted: "hosted",
        project: "flask",
        version: "1.0",
        filename: "flask-1.0-py3-none-any.whl",
        digest: "aa",
    }
}

#[test]
fn test_upload_store_error_response_reports_a_content_collision_as_bad_request() {
    let headers = HeaderMap::new();
    let response = upload_store_error_response(&audit(&headers), CacheError::FileExists("flask-1.0.whl".to_owned()));
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn test_upload_store_error_response_reports_a_store_fault_as_internal_error() {
    let headers = HeaderMap::new();
    let response = upload_store_error_response(&audit(&headers), CacheError::Meta(meta_error()));
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
