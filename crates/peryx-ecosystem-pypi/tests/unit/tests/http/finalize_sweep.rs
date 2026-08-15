use peryx_driver::serving::IntentFinalizer as _;
use peryx_ha::{ArtifactPlacement, ArtifactSource};
use peryx_storage::meta::{IntentAdmission, IntentLimits, IntentPhase, OperationState};

use super::support::*;
use crate::PypiServing;
use crate::serving::finalize_sweep::finalize_admitted;
use crate::store::put_upload;

const ARTIFACT: &[u8] = b"finalized-artifact-bytes";
const INDEX: &str = "hosted";
const AUTHORITY: &str = "flask";
const FILENAME: &str = "flask-1.0-py3-none-any.whl";
const INTENT_KEY: &str = "pypi:hosted:flask:flask-1.0-py3-none-any.whl";
const RECORD: &[u8] = br#"{"filename":"flask-1.0-py3-none-any.whl"}"#;

const LIMITS: IntentLimits = IntentLimits {
    max_records: 1_000,
    max_bytes: 1 << 30,
    backpressure_percent: 80,
};

fn digest() -> String {
    Digest::of(ARTIFACT).as_str().to_owned()
}

fn operation() -> String {
    format!("{INTENT_KEY}:{}", digest())
}

fn stage(meta: &MetaStore, key: &str, authority: &str) {
    meta.stage_intent(
        IntentAdmission {
            authority,
            key,
            digest: &digest(),
            size: ARTIFACT.len() as u64,
            payload: b"payload",
        },
        LIMITS,
        1000,
    )
    .unwrap();
}

fn place(meta: &MetaStore) {
    meta.put_artifact_placement(&digest(), &ArtifactPlacement::record(ArtifactSource::Hosted, true))
        .unwrap();
}

#[tokio::test]
async fn test_a_sweep_finalizes_a_pending_admitted_upload() {
    let harness = harness_with(true, true).await;
    initialize_distributed_schema(&harness.state);
    stage(&harness.state.serving.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.serving.meta);
    put_upload(&harness.state.serving.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();

    let finalized = PypiServing.finalize_admitted(harness.state.serving.clone()).await;

    assert_eq!(finalized, 1);
    assert_eq!(
        harness
            .state
            .serving
            .meta
            .staged_intent(INTENT_KEY)
            .unwrap()
            .unwrap()
            .phase,
        IntentPhase::Admitted,
        "the sweep advances the intent out of pending",
    );
    let record = harness
        .state
        .serving
        .meta
        .operation_outcome(&operation())
        .unwrap()
        .unwrap();
    assert_eq!(record.state, OperationState::Published);
    assert_eq!(record.response, b"upload accepted");
}

#[tokio::test]
async fn test_a_sweep_replaying_a_finalized_intent_leaves_it_settled_and_counts_nothing() {
    let harness = harness_with(true, true).await;
    initialize_distributed_schema(&harness.state);
    stage(&harness.state.serving.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.serving.meta);
    put_upload(&harness.state.serving.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();
    assert_eq!(finalize_admitted(&harness.state.serving).await, 1);

    let again = finalize_admitted(&harness.state.serving).await;

    assert_eq!(again, 0, "a settled intent is no longer pending");
}

#[tokio::test]
async fn test_a_sweep_skips_a_non_pypi_intent() {
    let harness = harness_with(true, true).await;
    stage(&harness.state.serving.meta, "oci:library:nginx", "nginx");

    let finalized = finalize_admitted(&harness.state.serving).await;

    assert_eq!(finalized, 0);
    assert_eq!(
        harness
            .state
            .serving
            .meta
            .staged_intent("oci:library:nginx")
            .unwrap()
            .unwrap()
            .phase,
        IntentPhase::Pending,
        "an intent of another ecosystem is left for its own driver",
    );
}

#[tokio::test]
async fn test_a_sweep_skips_an_intent_whose_rows_are_not_stored_here() {
    let harness = harness_with(true, true).await;
    stage(&harness.state.serving.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.serving.meta);

    let finalized = finalize_admitted(&harness.state.serving).await;

    assert_eq!(finalized, 0);
    assert_eq!(
        harness
            .state
            .serving
            .meta
            .staged_intent(INTENT_KEY)
            .unwrap()
            .unwrap()
            .phase,
        IntentPhase::Pending,
    );
}

#[tokio::test]
async fn test_a_sweep_skips_an_intent_no_index_token_may_write() {
    let harness = harness_with(false, true).await;
    stage(&harness.state.serving.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.serving.meta);
    put_upload(&harness.state.serving.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();

    let finalized = finalize_admitted(&harness.state.serving).await;

    assert_eq!(finalized, 0);
    assert_eq!(
        harness
            .state
            .serving
            .meta
            .staged_intent(INTENT_KEY)
            .unwrap()
            .unwrap()
            .phase,
        IntentPhase::Pending,
    );
}

#[tokio::test]
async fn test_a_sweep_returns_nothing_when_the_intent_ledger_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    stage(&meta, INTENT_KEY, AUTHORITY);
    drop(meta);
    let database = redb::Database::open(&path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut table = write
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("ingress_intent"))
            .unwrap();
        table.insert(INTENT_KEY, b"not json".as_slice()).unwrap();
    }
    write.commit().unwrap();
    drop(database);
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let app = AppState::with_clock(
        MetaStore::open_existing(&path).unwrap(),
        blobs,
        60,
        Vec::new(),
        Arc::new(|| 1000),
    );

    let finalized = finalize_admitted(&app.serving).await;

    assert_eq!(
        finalized, 0,
        "a read failure finalizes nothing rather than failing the pass"
    );
}

#[tokio::test]
async fn test_a_sweep_leaves_an_intent_pending_when_a_validation_refuses() {
    let harness = harness_with(true, true).await;
    stage(&harness.state.serving.meta, INTENT_KEY, AUTHORITY);
    put_upload(&harness.state.serving.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();

    let finalized = finalize_admitted(&harness.state.serving).await;

    assert_eq!(finalized, 0);
    assert_eq!(
        harness
            .state
            .serving
            .meta
            .staged_intent(INTENT_KEY)
            .unwrap()
            .unwrap()
            .phase,
        IntentPhase::Pending,
        "a refused finalize leaves the intent for a later pass once the condition clears",
    );
    assert!(
        harness
            .state
            .serving
            .meta
            .operation_outcome(&operation())
            .unwrap()
            .is_none(),
        "a refusal records nothing durable",
    );
}
