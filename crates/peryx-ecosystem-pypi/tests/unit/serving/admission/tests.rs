use peryx_core::{NodeRole, TopologyConfig, TopologyMember, TopologyMode};
use peryx_storage::blob::DurabilityCapabilities;
use peryx_storage::meta::{IntentPhase, MetaStore};
use redb::TableDefinition;

use super::*;

fn meta(dir: &tempfile::TempDir) -> MetaStore {
    MetaStore::open(dir.path().join("meta.redb")).unwrap()
}

fn request<'a>(filename: &'a str, digest: &'a str) -> AdmissionRequest<'a> {
    AdmissionRequest {
        tenant: "root/hosted",
        authority: "flask",
        filename,
        digest,
        size: 11,
        ingress_dc: "dc-a",
        provenance: None,
    }
}

fn limits(records: u64, bytes: u64) -> IntentLimits {
    IntentLimits {
        max_records: records,
        max_bytes: bytes,
        backpressure_percent: 80,
    }
}

fn rejected(admission: Admission) -> Option<Response> {
    match admission {
        Admission::Reject(response) => Some(*response),
        Admission::Admitted(_) => None,
    }
}

#[test]
fn test_admit_stages_a_fresh_intent_bound_to_its_identity() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);
    let request = request("flask-1.0.whl", "aa");

    assert!(matches!(
        admit(&meta, DurabilityCapabilities::FILESYSTEM, STAGING_LIMITS, &request, 10),
        Admission::Admitted(_)
    ));

    let staged = meta
        .staged_intent("pypi:root/hosted:flask:flask-1.0.whl")
        .unwrap()
        .unwrap();
    assert_eq!((staged.digest.as_str(), staged.size), ("aa", 11));
    let intent: IngressIntent = serde_json::from_slice(&staged.payload).unwrap();
    assert_eq!(intent.ingress_dc, "dc-a");
    assert_eq!(intent.operation, "pypi:root/hosted:flask:flask-1.0.whl:aa");
}

#[test]
fn test_admit_binds_an_attested_upload_to_its_own_operation() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);
    let request = AdmissionRequest {
        provenance: Some("bb"),
        ..request("flask-1.0.whl", "aa")
    };

    assert!(matches!(
        admit(&meta, DurabilityCapabilities::FILESYSTEM, STAGING_LIMITS, &request, 10),
        Admission::Admitted(_)
    ));

    let staged = meta
        .staged_intent("pypi:root/hosted:flask:flask-1.0.whl")
        .unwrap()
        .unwrap();
    let intent: IngressIntent = serde_json::from_slice(&staged.payload).unwrap();
    assert_eq!(
        intent.operation, "pypi:root/hosted:flask:flask-1.0.whl:aa:bb",
        "a re-upload that changes the bundle is a new operation, not a retry of the first"
    );
}

#[test]
fn test_admit_accepts_an_object_store_that_proves_durability() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);

    let admission = admit(
        &meta,
        DurabilityCapabilities::object_store(true, true),
        STAGING_LIMITS,
        &request("flask-1.0.whl", "aa"),
        10,
    );

    assert!(rejected(admission).is_none());
    assert_eq!(meta.count_staged_intents().unwrap(), 1);
}

#[test]
fn test_admit_deduplicates_an_identical_resend() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);
    let request = request("flask-1.0.whl", "aa");

    assert!(matches!(
        admit(&meta, DurabilityCapabilities::FILESYSTEM, STAGING_LIMITS, &request, 10),
        Admission::Admitted(_)
    ));
    assert!(matches!(
        admit(&meta, DurabilityCapabilities::FILESYSTEM, STAGING_LIMITS, &request, 20),
        Admission::Admitted(_)
    ));

    assert_eq!(meta.count_staged_intents().unwrap(), 1);
}

#[test]
fn test_admit_refuses_different_content_for_a_taken_filename() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);
    admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        STAGING_LIMITS,
        &request("flask-1.0.whl", "aa"),
        10,
    );

    let admission = admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        STAGING_LIMITS,
        &request("flask-1.0.whl", "bb"),
        20,
    );

    assert_eq!(rejected(admission).unwrap().status(), StatusCode::BAD_REQUEST);
}

#[test]
fn test_admit_sheds_load_when_the_record_ceiling_is_reached() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);
    admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        limits(1, 1 << 20),
        &request("flask-1.0.whl", "aa"),
        10,
    );

    let admission = admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        limits(1, 1 << 20),
        &request("click-8.0.whl", "bb"),
        20,
    );

    let response = rejected(admission).unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers()[header::RETRY_AFTER],
        SHED_RETRY_AFTER_SECS.to_string().as_str()
    );
}

#[test]
fn test_admit_sheds_load_when_the_next_intent_would_cross_the_byte_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);
    admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        limits(8, 15),
        &request("flask-1.0.whl", "aa"),
        10,
    );

    let admission = admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        limits(8, 15),
        &request("click-8.0.whl", "bb"),
        20,
    );

    let response = rejected(admission).unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response.headers()[header::RETRY_AFTER],
        SHED_RETRY_AFTER_SECS.to_string().as_str()
    );
}

#[test]
fn test_admit_still_stages_while_backpressured_below_the_hard_bound() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);

    // Two records, backpressure at 80%: the first admitted intent already sits at the soft threshold,
    // so admission stays open under backpressure rather than shedding before the hard bound.
    let admission = admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        limits(2, 1 << 20),
        &request("flask-1.0.whl", "aa"),
        10,
    );

    assert!(matches!(admission, Admission::Admitted(_)));
    assert_eq!(meta.count_staged_intents().unwrap(), 1);
}

#[test]
fn test_admit_bounds_each_authority_independently() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);
    let full = admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        limits(1, 1 << 20),
        &request("flask-1.0.whl", "aa"),
        10,
    );
    assert!(matches!(full, Admission::Admitted(_)));

    // A second authority at its own ceiling admits though the first authority is full: the buffer is
    // bounded per authority, so one busy project cannot starve another.
    let other = AdmissionRequest {
        authority: "click",
        ..request("click-8.0.whl", "bb")
    };
    let admission = admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        limits(1, 1 << 20),
        &other,
        20,
    );

    assert!(matches!(admission, Admission::Admitted(_)));
    assert_eq!(meta.count_staged_intents().unwrap(), 2);
}

#[test]
fn test_admit_refuses_a_backend_that_cannot_prove_durability() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);

    let admission = admit(
        &meta,
        DurabilityCapabilities::object_store(false, false),
        STAGING_LIMITS,
        &request("flask-1.0.whl", "aa"),
        10,
    );

    assert_eq!(rejected(admission).unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(meta.count_staged_intents().unwrap(), 0);
}

#[test]
fn test_admit_surfaces_a_store_fault_as_internal_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.redb");
    {
        let db = redb::Database::create(&path).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut table = txn
                .open_table(TableDefinition::<&str, &[u8]>::new("ingress_intent"))
                .unwrap();
            table
                .insert("pypi:root/hosted:flask:flask-1.0.whl", b"not json".as_slice())
                .unwrap();
        }
        txn.commit().unwrap();
    }
    let meta = MetaStore::open(&path).unwrap();

    let admission = admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        STAGING_LIMITS,
        &request("flask-1.0.whl", "aa"),
        10,
    );

    assert_eq!(rejected(admission).unwrap().status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn test_ingress_dc_reads_the_local_roster_member() {
    let topology = TopologyConfig {
        mode: TopologyMode::Dc,
        group: Some("group".to_owned()),
        members: vec![TopologyMember {
            node: "node-1".to_owned(),
            dc: "dc-west".to_owned(),
            address: "node-1.internal".to_owned(),
            role: NodeRole::Writer,
        }],
        local_node: Some("node-1".to_owned()),
    };

    assert_eq!(ingress_dc(&topology), "dc-west");
}

#[test]
fn test_ingress_dc_falls_back_for_a_rosterless_node() {
    assert_eq!(ingress_dc(&TopologyConfig::default()), STANDALONE_DC);
}

const STAGED_KEY: &str = "pypi:root/hosted:flask:flask-1.0.whl";

#[test]
fn test_fault_home_loss_before_admission_retains_the_write_unpublished() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);

    let admission = admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        STAGING_LIMITS,
        &request("flask-1.0.whl", "aa"),
        10,
    );

    assert!(matches!(admission, Admission::Admitted(_)));
    assert_eq!(
        meta.staged_intent(STAGED_KEY).unwrap().unwrap().phase,
        IntentPhase::Pending
    );
}

#[test]
fn test_fault_home_loss_after_local_durability_never_drops_the_retained_write() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);
    admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        STAGING_LIMITS,
        &request("flask-1.0.whl", "aa"),
        10,
    );

    assert_eq!(meta.prune_ingress_intents(1_000_000, 60, 100).unwrap(), 0);
    assert_eq!(
        meta.staged_intent(STAGED_KEY).unwrap().unwrap().phase,
        IntentPhase::Pending
    );
    assert_eq!(meta.count_staged_intents().unwrap(), 1);
}

#[test]
fn test_fault_home_loss_during_retry_deduplicates_and_stays_pending() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);
    let request = request("flask-1.0.whl", "aa");
    admit(&meta, DurabilityCapabilities::FILESYSTEM, STAGING_LIMITS, &request, 10);

    let resend = admit(&meta, DurabilityCapabilities::FILESYSTEM, STAGING_LIMITS, &request, 20);

    assert!(matches!(resend, Admission::Admitted(_)));
    assert_eq!(meta.count_staged_intents().unwrap(), 1);
    assert_eq!(
        meta.staged_intent(STAGED_KEY).unwrap().unwrap().phase,
        IntentPhase::Pending
    );
}

#[test]
fn test_fault_home_loss_after_expiry_reclaims_the_slot() {
    let dir = tempfile::tempdir().unwrap();
    let meta = meta(&dir);
    admit(
        &meta,
        DurabilityCapabilities::FILESYSTEM,
        STAGING_LIMITS,
        &request("flask-1.0.whl", "aa"),
        10,
    );
    meta.advance_intent(STAGED_KEY, IntentPhase::Expired, 20).unwrap();

    assert_eq!(meta.prune_ingress_intents(100, 60, 100).unwrap(), 1);
    assert_eq!(meta.staged_intent(STAGED_KEY).unwrap(), None);
    assert_eq!(meta.staged_intent_usage("flask").unwrap().records, 0);
}
