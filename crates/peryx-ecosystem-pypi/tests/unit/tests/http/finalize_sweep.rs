use peryx_driver::jobs::MAX_INTENT_REFUSALS;
use peryx_driver::serving::IntentFinalizer as _;
use peryx_ha::{ArtifactPlacement, ArtifactSource};
use peryx_storage::meta::{IntentAdmission, IntentLimits, IntentPhase, OperationState};

use super::support::*;
use crate::PypiServing;
use crate::serving::finalize_sweep::finalize_admitted;
use crate::store::{list_upload_entries, put_upload};

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

fn state_with_route_collision() -> (tempfile::TempDir, Arc<peryx_driver::state::ServingState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.initialize_distributed_state().unwrap();
    let state = AppState::with_clock(
        meta,
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        vec![
            Index {
                name: "other".to_owned(),
                route: "other".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Hosted { volatile: true },
                policy: Policy::default(),
                acl: peryx_identity::IndexAcl::default(),
            },
            Index {
                name: INDEX.to_owned(),
                route: INDEX.to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Hosted { volatile: true },
                policy: Policy::default(),
                acl: crate::tests::writer_acl("s3cret"),
            },
        ],
        Arc::new(|| 1000),
    );
    (dir, state.serving)
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
async fn test_a_sweep_finalizes_against_the_intent_route() {
    let (_dir, state) = state_with_route_collision();
    stage(&state.meta, INTENT_KEY, AUTHORITY);
    place(&state.meta);
    put_upload(&state.meta, "other", AUTHORITY, FILENAME, b"other").unwrap();
    put_upload(&state.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();

    let finalized = PypiServing.finalize_admitted(state.clone()).await;

    assert_eq!(
        (
            finalized,
            state.meta.staged_intent(INTENT_KEY).unwrap().unwrap().phase,
            state.meta.operation_outcome(&operation()).unwrap().unwrap().state,
        ),
        (1, IntentPhase::Admitted, OperationState::Published),
    );
}

#[rstest]
#[case::missing_target_row(INDEX, false)]
#[case::unknown_route("missing", true)]
#[tokio::test]
async fn test_a_sweep_does_not_fall_back_to_another_index(#[case] route: &str, #[case] store_target_row: bool) {
    let (_dir, state) = state_with_route_collision();
    let key = format!("pypi:{route}:{AUTHORITY}:{FILENAME}");
    stage(&state.meta, &key, AUTHORITY);
    place(&state.meta);
    put_upload(&state.meta, "other", AUTHORITY, FILENAME, b"other").unwrap();
    if store_target_row {
        put_upload(&state.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();
    }

    let finalized = PypiServing.finalize_admitted(state.clone()).await;

    assert_eq!(
        (finalized, state.meta.staged_intent(&key).unwrap().unwrap().phase),
        (0, IntentPhase::Pending),
    );
}

#[tokio::test]
async fn test_a_sweep_resolves_a_virtual_upload_route() {
    let harness = harness_with(true, true).await;
    initialize_distributed_schema(&harness.state);
    let key = "pypi:root/pypi:flask:flask-1.0-py3-none-any.whl";
    stage(&harness.state.serving.meta, key, AUTHORITY);
    place(&harness.state.serving.meta);
    put_upload(&harness.state.serving.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();

    let finalized = PypiServing.finalize_admitted(harness.state.serving.clone()).await;

    assert_eq!(
        (
            finalized,
            harness.state.serving.meta.staged_intent(key).unwrap().unwrap().phase,
        ),
        (1, IntentPhase::Admitted),
    );
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

fn refusals(state: &peryx_driver::state::ServingState, key: &str) -> u32 {
    state.meta.staged_intent(key).unwrap().unwrap().refusals
}

#[tokio::test]
async fn test_a_sweep_refuses_an_intent_whose_rows_are_not_stored_here() {
    let harness = harness_with(true, true).await;
    stage(&harness.state.serving.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.serving.meta);

    finalize_admitted(&harness.state.serving).await;

    assert_eq!(
        refusals(&harness.state.serving, INTENT_KEY),
        1,
        "no upload can ever finalize an intent whose rows were never stored",
    );
}

#[tokio::test]
async fn test_a_sweep_stops_offering_an_intent_it_has_refused_to_the_ceiling() {
    let harness = harness_with(true, true).await;
    stage(&harness.state.serving.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.serving.meta);

    for _ in 0..=MAX_INTENT_REFUSALS {
        finalize_admitted(&harness.state.serving).await;
    }

    assert_eq!(
        refusals(&harness.state.serving, INTENT_KEY),
        MAX_INTENT_REFUSALS,
        "the pass after the ceiling no longer reads the intent, so an unfinalizable head cannot fill the batch",
    );
}

#[tokio::test]
async fn test_a_sweep_does_not_refuse_an_intent_an_acl_change_could_still_finalize() {
    let harness = harness_with(false, true).await;
    stage(&harness.state.serving.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.serving.meta);
    put_upload(&harness.state.serving.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();

    finalize_admitted(&harness.state.serving).await;

    assert_eq!(
        refusals(&harness.state.serving, INTENT_KEY),
        0,
        "the rows are stored, so this skip is transient and the intent stays offered",
    );
}

#[tokio::test]
async fn test_an_upload_retry_publishes_after_the_sweep_finds_no_target_row() {
    let harness = harness_with(true, true).await;
    let wheel = fixture_wheel();
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    let key = format!("pypi:{INDEX}:peryxpkg:{filename}");
    harness
        .state
        .serving
        .meta
        .stage_intent(
            IntentAdmission {
                authority: "peryxpkg",
                key: &key,
                digest: Digest::of(&wheel).as_str(),
                size: wheel.len() as u64,
                payload: b"payload",
            },
            LIMITS,
            1000,
        )
        .unwrap();
    assert_eq!(PypiServing.finalize_admitted(harness.state.serving.clone()).await, 0);
    let (content_type, body) = multipart_body(&upload_fields(), Some((filename, &wheel)));

    let status = post_upload(&harness.state, "/hosted/", Some(&upload_auth()), &content_type, body).await;

    assert_eq!(
        (
            status,
            harness.state.serving.meta.staged_intent(&key).unwrap().unwrap().phase,
        ),
        (StatusCode::OK, IntentPhase::Admitted),
    );
    assert_eq!(
        list_upload_entries(&harness.state.serving.meta, INDEX, "peryxpkg")
            .unwrap()
            .into_iter()
            .map(|(filename, _)| filename)
            .collect::<Vec<_>>(),
        [filename],
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

#[tokio::test]
async fn test_a_drain_publishes_the_retained_write_of_the_authority_it_names() {
    let harness = harness_with(true, true).await;
    initialize_distributed_schema(&harness.state);
    stage(&harness.state.serving.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.serving.meta);
    put_upload(&harness.state.serving.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();

    let settled = PypiServing
        .finalize_retained(harness.state.serving.clone(), AUTHORITY, INTENT_KEY)
        .await;

    assert_eq!(
        (
            settled,
            harness
                .state
                .serving
                .meta
                .staged_intent(INTENT_KEY)
                .unwrap()
                .unwrap()
                .phase,
            harness
                .state
                .serving
                .meta
                .operation_outcome(&operation())
                .unwrap()
                .unwrap()
                .state,
        ),
        (true, IntentPhase::Admitted, OperationState::Published),
    );
}

#[tokio::test]
async fn test_a_drain_leaves_a_retained_write_staged_for_another_authority_untouched() {
    let harness = harness_with(true, true).await;
    initialize_distributed_schema(&harness.state);
    stage(&harness.state.serving.meta, INTENT_KEY, AUTHORITY);
    place(&harness.state.serving.meta);
    put_upload(&harness.state.serving.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();

    let settled = PypiServing
        .finalize_retained(harness.state.serving.clone(), "django", INTENT_KEY)
        .await;

    assert_eq!(
        (
            settled,
            harness
                .state
                .serving
                .meta
                .staged_intent(INTENT_KEY)
                .unwrap()
                .unwrap()
                .phase,
            harness.state.serving.meta.operation_outcome(&operation()).unwrap(),
        ),
        (false, IntentPhase::Pending, None),
    );
}

#[tokio::test]
async fn test_a_drain_declines_a_key_no_intent_is_staged_under() {
    let harness = harness_with(true, true).await;

    let settled = PypiServing
        .finalize_retained(harness.state.serving.clone(), AUTHORITY, INTENT_KEY)
        .await;

    assert_eq!(
        (settled, harness.state.serving.meta.staged_intent(INTENT_KEY).unwrap()),
        (false, None),
    );
}

#[tokio::test]
async fn test_a_drain_declines_a_staging_record_it_cannot_read() {
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
    let app = AppState::with_clock(
        MetaStore::open_existing(&path).unwrap(),
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        Vec::new(),
        Arc::new(|| 1000),
    );

    let settled = PypiServing
        .finalize_retained(app.serving.clone(), AUTHORITY, INTENT_KEY)
        .await;

    assert!(!settled);
}
