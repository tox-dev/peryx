//! The home-side finalize sweep over this node's pending `PyPI` ingress intents.

use peryx_driver::serving::IntentFinalizer as _;
use peryx_storage::meta::{
    ArtifactPlacement, ArtifactSource, IntentAdmission, IntentLimits, IntentPhase, OperationState,
};

use super::support::*;
use crate::PypiServing;
use crate::serving::finalize_sweep::finalize_admitted;
use crate::store::put_upload;

/// The artifact whose admitted bytes the sweep finalizes into a release.
const ARTIFACT: &[u8] = b"finalized-artifact-bytes";
/// The hosted store the harness admits uploads into, whose ACL grants `uploader` a write.
const INDEX: &str = "hosted";
/// The project the upload publishes under, its PEP 503 normalized name and the fenced authority.
const AUTHORITY: &str = "flask";
/// The distribution filename, the tail of the intent key.
const FILENAME: &str = "flask-1.0-py3-none-any.whl";
/// The staging key admission minted, `pypi:{route}:{authority}:{filename}`.
const INTENT_KEY: &str = "pypi:hosted:flask:flask-1.0-py3-none-any.whl";
/// The serialized file record the ingress node stored; the sweep re-publishes it idempotently.
const RECORD: &[u8] = br#"{"filename":"flask-1.0-py3-none-any.whl"}"#;

/// Generous per-authority ceilings; the sweep tests exercise a single admission, not the bounds.
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

/// Stage the pending intent under `key` for `authority`, the durable admission a home finalize reads.
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

/// Place the artifact's bytes, the state a finalize requires before it publishes.
fn place(meta: &MetaStore) {
    meta.put_artifact_placement(&digest(), &ArtifactPlacement::record(ArtifactSource::Hosted, true))
        .unwrap();
}

#[tokio::test]
async fn test_a_sweep_finalizes_a_pending_admitted_upload() {
    let harness = harness_with(true, true).await;
    stage(&harness.state.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.meta);
    put_upload(&harness.state.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();

    // Through the driver's maintenance entry point, the way the scheduled pass reaches it.
    let finalized = PypiServing.finalize_admitted(harness.state.serving.clone()).await;

    assert_eq!(finalized, 1);
    assert_eq!(
        harness.state.meta.staged_intent(INTENT_KEY).unwrap().unwrap().phase,
        IntentPhase::Admitted,
        "the sweep advances the intent out of pending",
    );
    let record = harness.state.meta.operation_outcome(&operation()).unwrap().unwrap();
    assert_eq!(record.state, OperationState::Published);
    assert_eq!(record.response, b"upload accepted");
}

#[tokio::test]
async fn test_a_sweep_replaying_a_finalized_intent_leaves_it_settled_and_counts_nothing() {
    let harness = harness_with(true, true).await;
    stage(&harness.state.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.meta);
    put_upload(&harness.state.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();
    assert_eq!(finalize_admitted(&harness.state.serving).await, 1);

    // The intent already advanced, so a second pass finds nothing pending to finalize.
    let again = finalize_admitted(&harness.state.serving).await;

    assert_eq!(again, 0, "a settled intent is no longer pending");
}

#[tokio::test]
async fn test_a_sweep_skips_a_non_pypi_intent() {
    let harness = harness_with(true, true).await;
    stage(&harness.state.meta, "oci:library:nginx", "nginx");

    let finalized = finalize_admitted(&harness.state.serving).await;

    assert_eq!(finalized, 0);
    assert_eq!(
        harness
            .state
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
    stage(&harness.state.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.meta);
    // No upload row is stored, so no configured index holds this file to finalize.

    let finalized = finalize_admitted(&harness.state.serving).await;

    assert_eq!(finalized, 0);
    assert_eq!(
        harness.state.meta.staged_intent(INTENT_KEY).unwrap().unwrap().phase,
        IntentPhase::Pending,
    );
}

#[tokio::test]
async fn test_a_sweep_skips_an_intent_no_index_token_may_write() {
    // The hosted index carries no write token, so no principal re-authorizes the finalize.
    let harness = harness_with(false, true).await;
    stage(&harness.state.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.meta);
    put_upload(&harness.state.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();

    let finalized = finalize_admitted(&harness.state.serving).await;

    assert_eq!(finalized, 0);
    assert_eq!(
        harness.state.meta.staged_intent(INTENT_KEY).unwrap().unwrap().phase,
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
    stage(&harness.state.meta, INTENT_KEY, AUTHORITY);
    put_upload(&harness.state.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();
    // The bytes are not placed, so the finalize refuses without recording a terminal outcome.

    let finalized = finalize_admitted(&harness.state.serving).await;

    assert_eq!(finalized, 0);
    assert_eq!(
        harness.state.meta.staged_intent(INTENT_KEY).unwrap().unwrap().phase,
        IntentPhase::Pending,
        "a refused finalize leaves the intent for a later pass once the condition clears",
    );
    assert!(
        harness.state.meta.operation_outcome(&operation()).unwrap().is_none(),
        "a refusal records nothing durable",
    );
}
