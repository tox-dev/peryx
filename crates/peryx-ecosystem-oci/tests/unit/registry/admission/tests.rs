use super::*;
use axum::http::StatusCode;
use peryx_core::{NodeRole, TopologyConfig, TopologyMember, TopologyMode};
use peryx_storage::meta::{IntentPhase, IntentUsage};

const DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";
const OTHER: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000002";
const OPERATION: &str = "oci:store:app:sha256:0000000000000000000000000000000000000000000000000000000000000001";
const LIMITS: IntentLimits = IntentLimits {
    max_records: 8,
    max_bytes: 1024,
    backpressure_percent: 80,
};
/// The shed decision every full authority answers with: a `503` and the backoff the client waits.
const SHED: (StatusCode, Option<&str>) = (StatusCode::SERVICE_UNAVAILABLE, Some("30"));

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn request(digest: &str) -> AdmissionRequest<'_> {
    AdmissionRequest {
        index: "store",
        repo: "app",
        digest,
        size: 21,
        operation: OPERATION,
        session: None,
        reservation: None,
        ingress_dc: "east",
    }
}

/// The decision as a comparable value: the key it retained the push under, or the status and backoff
/// it shed the push with.
fn outcome(admission: Admission) -> Result<String, (StatusCode, Option<String>)> {
    match admission {
        Admission::Staged(key) => Ok(key),
        Admission::Shed(response) => Err((
            response.status(),
            response
                .headers()
                .get(header::RETRY_AFTER)
                .map(|value| value.to_str().unwrap().to_owned()),
        )),
    }
}

fn shed() -> Result<String, (StatusCode, Option<String>)> {
    Err((SHED.0, SHED.1.map(str::to_owned)))
}

#[test]
fn test_the_intent_key_binds_the_index_repository_and_digest() {
    assert_eq!(
        intent_key("store", "app", DIGEST),
        format!("oci:blob:store:app:{DIGEST}")
    );
}

#[test]
fn test_a_staged_push_retains_its_upload_identity() {
    let (_dir, meta) = store();

    let key = outcome(admit(&meta, LIMITS, &request(DIGEST), 100).unwrap()).unwrap();

    let retained = meta.staged_intent(&key).unwrap().unwrap();
    assert_eq!(
        serde_json::from_slice::<BlobIntent>(&retained.payload).unwrap(),
        BlobIntent {
            version: PAYLOAD_VERSION,
            index: "store".to_owned(),
            repo: "app".to_owned(),
            authority: crate::name::authority_key("app"),
            digest: DIGEST.to_owned(),
            size: 21,
            ingress_dc: "east".to_owned(),
            operation: OPERATION.to_owned(),
            session: None,
            reservation: None,
        }
    );
}

#[test]
fn test_a_resumable_push_retains_the_session_its_publication_closes() {
    let (_dir, meta) = store();
    let mut request = request(DIGEST);
    request.session = Some("upload-7");

    let key = outcome(admit(&meta, LIMITS, &request, 100).unwrap()).unwrap();

    let retained = meta.staged_intent(&key).unwrap().unwrap();
    assert_eq!(
        serde_json::from_slice::<BlobIntent>(&retained.payload).unwrap().session,
        Some("upload-7".to_owned())
    );
}

#[test]
fn test_a_staged_push_is_accounted_under_its_repository_authority() {
    let (_dir, meta) = store();

    admit(&meta, LIMITS, &request(DIGEST), 100).unwrap();

    assert_eq!(
        meta.staged_intent_usage(&crate::name::authority_key("app")).unwrap(),
        IntentUsage { records: 1, bytes: 21 }
    );
}

#[test]
fn test_a_resent_push_of_the_same_layer_deduplicates_onto_one_intent() {
    let (_dir, meta) = store();

    let first = outcome(admit(&meta, LIMITS, &request(DIGEST), 100).unwrap());
    let second = outcome(admit(&meta, LIMITS, &request(DIGEST), 200).unwrap());

    assert_eq!((first, meta.count_staged_intents().unwrap()), (second, 1));
}

#[test]
fn test_an_authority_at_its_record_ceiling_sheds_the_push() {
    let (_dir, meta) = store();
    let limits = IntentLimits {
        max_records: 0,
        ..LIMITS
    };

    assert_eq!(outcome(admit(&meta, limits, &request(DIGEST), 100).unwrap()), shed());
}

#[test]
fn test_an_authority_at_its_byte_ceiling_sheds_the_push() {
    let (_dir, meta) = store();
    let limits = IntentLimits { max_bytes: 0, ..LIMITS };

    assert_eq!(outcome(admit(&meta, limits, &request(DIGEST), 100).unwrap()), shed());
}

#[test]
fn test_a_key_already_bound_to_other_content_sheds_the_push() {
    let (_dir, meta) = store();
    let key = intent_key("store", "app", DIGEST);
    meta.stage_intent(
        IntentAdmission {
            authority: &crate::name::authority_key("app"),
            key: &key,
            digest: OTHER,
            size: 9,
            payload: b"{}",
        },
        LIMITS,
        50,
    )
    .unwrap();

    assert_eq!(outcome(admit(&meta, LIMITS, &request(DIGEST), 100).unwrap()), shed());
}

#[test]
fn test_a_push_that_crosses_the_soft_threshold_is_still_staged() {
    let (_dir, meta) = store();
    let limits = IntentLimits {
        max_records: 2,
        backpressure_percent: 50,
        ..LIMITS
    };

    let key = outcome(admit(&meta, limits, &request(DIGEST), 100).unwrap()).unwrap();

    assert_eq!(meta.staged_intent(&key).unwrap().unwrap().phase, IntentPhase::Pending);
}

#[test]
fn test_a_deployment_with_no_roster_records_the_standalone_datacenter() {
    assert_eq!(ingress_dc(&TopologyConfig::default()), "local");
}

#[test]
fn test_a_rostered_deployment_records_the_local_datacenter() {
    let topology = TopologyConfig {
        mode: TopologyMode::Ha,
        group: Some("test".to_owned()),
        members: vec![TopologyMember {
            node: "writer".to_owned(),
            dc: "east".to_owned(),
            address: "http://127.0.0.1".to_owned(),
            role: NodeRole::Writer,
        }],
        local_node: Some("writer".to_owned()),
    };

    assert_eq!(ingress_dc(&topology), "east");
}
