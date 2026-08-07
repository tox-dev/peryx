//! The home datacenter finalizes admitted `PyPI` uploads through the scheduled maintenance pass.
//!
//! The finalize mechanism lives in the `PyPI` driver; this drives it through the neutral
//! [`EcosystemDriver::finalize_admitted`] entry the maintenance job calls, so the home-side finalize is
//! exercised the way the running server reaches it.

use std::collections::BTreeSet;
use std::sync::Arc;

use peryx_driver::serving::EcosystemDriver;
use peryx_driver::state::AppState;
use peryx_ecosystem_registry::pypi::PypiServing;
use peryx_ecosystem_registry::pypi::store::put_upload;
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};
use peryx_index::{Index, IndexKind};
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::{
    ArtifactPlacement, ArtifactSource, IntentAdmission, IntentLimits, IntentPhase, MetaStore, OperationState,
};

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

/// A hosted index whose ACL grants `uploader` a write, so a finalize re-authorizes against it.
fn hosted_index() -> Index {
    Index {
        name: INDEX.to_owned(),
        route: INDEX.to_owned(),
        ecosystem: peryx_ecosystem_registry::PYPI,
        kind: IndexKind::Hosted { volatile: false },
        policy: peryx_policy::Policy::default(),
        acl: IndexAcl {
            anonymous_read: true,
            tokens: vec![NamedToken {
                name: "uploader".to_owned(),
                secret: "s3cret".to_owned(),
                grants: vec![Grant {
                    projects: vec![Glob::new("*")],
                    actions: BTreeSet::from([Action::Write]),
                }],
                expires_at: None,
            }],
        },
    }
}

fn state_with_hosted_index() -> (tempfile::TempDir, Arc<peryx_driver::state::ServingState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let app = AppState::with_clock(meta, blobs, 60, vec![hosted_index()], Arc::new(|| 1_000));
    (dir, app.serving)
}

#[tokio::test]
async fn test_the_home_dc_finalizes_a_pending_admitted_upload() {
    let (_dir, state) = state_with_hosted_index();
    let digest = Digest::of(ARTIFACT).as_str().to_owned();
    state
        .meta
        .stage_intent(
            IntentAdmission {
                authority: AUTHORITY,
                key: INTENT_KEY,
                digest: &digest,
                size: ARTIFACT.len() as u64,
                payload: b"payload",
            },
            LIMITS,
            1000,
        )
        .unwrap();
    state
        .meta
        .put_artifact_placement(&digest, &ArtifactPlacement::record(ArtifactSource::Hosted, true))
        .unwrap();
    put_upload(&state.meta, INDEX, AUTHORITY, FILENAME, RECORD).unwrap();

    let finalized = PypiServing.finalize_admitted(state.clone()).await;

    assert_eq!(finalized, 1, "the home DC finalizes the one pending upload");
    assert_eq!(
        state.meta.staged_intent(INTENT_KEY).unwrap().unwrap().phase,
        IntentPhase::Admitted,
        "finalization advances the intent out of pending",
    );
    let outcome = state
        .meta
        .operation_outcome(&format!("{INTENT_KEY}:{digest}"))
        .unwrap()
        .unwrap();
    assert_eq!(outcome.state, OperationState::Published);
    assert_eq!(outcome.response, b"upload accepted");
}
