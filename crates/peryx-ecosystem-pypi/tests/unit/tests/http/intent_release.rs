use peryx_storage::meta::{IntentAdmission, IntentLimits, IntentPhase, IntentUsage};

use super::support::*;

const AUTHORITY: &str = "peryxpkg";
const FILENAME: &str = "peryxpkg-1.0-py3-none-any.whl";
const INTENT_KEY: &str = "pypi:hosted:peryxpkg:peryxpkg-1.0-py3-none-any.whl";

const LIMITS: IntentLimits = IntentLimits {
    max_records: 1_000,
    max_bytes: 1 << 30,
    backpressure_percent: 80,
};

/// A record the simple-API parser rejects, so the upload fails inside the store rather than before the
/// intent is staged.
fn poison_the_store(state: &Arc<AppState>) {
    state
        .serving
        .meta
        .put_upload("hosted", AUTHORITY, FILENAME, b"not-json")
        .unwrap();
}

async fn upload_the_wheel(state: &Arc<AppState>, wheel: &[u8]) -> StatusCode {
    let (content_type, body) = multipart_body(&upload_fields(), Some((FILENAME, wheel)));
    let (status, _) = post_upload_response(state, "/hosted/", Some(&upload_auth()), &content_type, body).await;
    status
}

#[tokio::test]
async fn test_a_store_fault_releases_the_intent_the_upload_staged() {
    let h = harness().await;
    poison_the_store(&h.state);

    let status = upload_the_wheel(&h.state, &fixture_wheel()).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        h.state.serving.meta.staged_intent(INTENT_KEY).unwrap(),
        None,
        "a write that stored nothing leaves no intent for the reaper to expire"
    );
    assert_eq!(
        h.state.serving.meta.staged_intent_usage(AUTHORITY).unwrap(),
        IntentUsage::default(),
        "the authority is back to the capacity it had before the upload"
    );
}

#[tokio::test]
async fn test_a_store_fault_leaves_the_intent_a_concurrent_resend_deduplicated_onto() {
    let h = harness().await;
    let wheel = fixture_wheel();
    h.state
        .serving
        .meta
        .stage_intent(
            IntentAdmission {
                authority: AUTHORITY,
                key: INTENT_KEY,
                digest: Digest::of(&wheel).as_str(),
                size: wheel.len() as u64,
                payload: b"payload",
            },
            LIMITS,
            1_000,
        )
        .unwrap();
    poison_the_store(&h.state);

    let status = upload_the_wheel(&h.state, &wheel).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        h.state
            .serving
            .meta
            .staged_intent(INTENT_KEY)
            .unwrap()
            .map(|intent| intent.phase),
        Some(IntentPhase::Pending),
        "the record belongs to the upload still storing its bytes, not to this failed resend"
    );
}
