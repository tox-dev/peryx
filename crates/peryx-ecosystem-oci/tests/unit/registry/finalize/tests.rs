use super::*;
use peryx_driver::AppState;
use peryx_identity::IndexAcl;
use peryx_index::IndexKind;
use peryx_policy::{Policy, PolicyConfig};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{
    AccountingClass, IntentAdmission, IntentLimits, MetaStore, NewQuotaReservation, OperationState, QuotaLimits,
    QuotaReservationRecord,
};

use crate::upload_session::UploadStore as _;

const LAYER: &[u8] = b"a-retained-layer";
const LIMITS: IntentLimits = IntentLimits {
    max_records: 64,
    max_bytes: 1 << 20,
    backpressure_percent: 80,
};

struct Node {
    dir: tempfile::TempDir,
    state: Arc<ServingState>,
}

impl Node {
    fn with_policy(policy: Policy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let index = Index {
            name: "store".to_owned(),
            route: "store".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy,
            acl: IndexAcl::default(),
        };
        let app = AppState::with_clock(
            MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
            BlobStore::new(dir.path().join("blobs")),
            60,
            vec![index],
            Arc::new(|| 1000),
        );
        Self {
            dir,
            state: app.serving,
        }
    }

    fn open() -> Self {
        Self::with_policy(Policy::default())
    }

    /// Put a regular file where the content store shards its blobs, so every content read faults
    /// rather than reporting the blob absent.
    fn fault_content_reads(&self) {
        let root = self.dir.path().join("blobs");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("sha256"), b"not a shard directory").unwrap();
    }

    fn store_layer(&self) -> String {
        let digest = BlobStore::new(self.dir.path().join("blobs")).write(LAYER).unwrap();
        format!("sha256:{}", digest.as_str())
    }

    fn stage(&self, payload: &BlobIntent) -> String {
        let key = crate::registry::admission::intent_key(&payload.index, &payload.repo, &payload.digest);
        self.stage_raw(&key, &payload.digest, &serde_json::to_vec(payload).unwrap())
    }

    fn stage_raw(&self, key: &str, digest: &str, payload: &[u8]) -> String {
        self.state
            .meta
            .stage_intent(
                IntentAdmission {
                    authority: &crate::name::authority_key("app"),
                    key,
                    digest,
                    size: LAYER.len() as u64,
                    payload,
                },
                LIMITS,
                10,
            )
            .unwrap();
        key.to_owned()
    }

    async fn sweep(&self) -> u64 {
        finalize_admitted(&self.state, true).await
    }

    fn retained(&self, key: &str) -> (IntentPhase, u32) {
        let intent = self.state.meta.staged_intent(key).unwrap().unwrap();
        (intent.phase, intent.refusals)
    }

    fn serves(&self, digest: &str) -> bool {
        store::blob_is_member(&self.state.meta, "store", "app", digest).unwrap()
    }
}

fn envelope(digest: &str) -> BlobIntent {
    BlobIntent {
        version: PAYLOAD_VERSION,
        index: "store".to_owned(),
        repo: "app".to_owned(),
        authority: crate::name::authority_key("app"),
        digest: digest.to_owned(),
        size: LAYER.len() as u64,
        ingress_dc: "east".to_owned(),
        operation: format!("oci:store:app:{digest}"),
        session: None,
        reservation: None,
    }
}

#[tokio::test]
async fn test_a_retained_push_is_published_at_its_home() {
    let node = Node::open();
    let digest = node.store_layer();
    node.stage(&envelope(&digest));

    assert_eq!((node.sweep().await, node.serves(&digest)), (1, true));
}

#[tokio::test]
async fn test_publishing_a_retained_push_settles_its_intent() {
    let node = Node::open();
    let digest = node.store_layer();
    let key = node.stage(&envelope(&digest));

    node.sweep().await;

    assert_eq!(node.retained(&key), (IntentPhase::Admitted, 0));
}

#[tokio::test]
async fn test_publishing_a_retained_push_records_its_terminal_result() {
    let node = Node::open();
    let digest = node.store_layer();
    let envelope = envelope(&digest);
    node.stage(&envelope);

    node.sweep().await;

    assert_eq!(
        node.state
            .meta
            .operation_outcome(&envelope.operation)
            .unwrap()
            .map(|record| record.state),
        Some(OperationState::Published)
    );
}

#[tokio::test]
async fn test_a_settled_intent_is_not_published_twice() {
    let node = Node::open();
    let digest = node.store_layer();
    node.stage(&envelope(&digest));
    assert_eq!(node.sweep().await, 1);

    assert_eq!(node.sweep().await, 0);
}

#[tokio::test]
async fn test_a_retained_push_closes_the_session_it_finished_on() {
    let node = Node::open();
    let digest = node.store_layer();
    let mut payload = envelope(&digest);
    payload.session = Some("upload-3".to_owned());
    node.state.meta.begin_upload("upload-3", "store", "app", 5).unwrap();
    node.stage(&payload);

    node.sweep().await;

    assert_eq!(node.state.meta.upload_record("upload-3").unwrap(), None);
}

#[tokio::test]
async fn test_a_retained_push_commits_the_quota_its_admission_reserved() {
    let node = Node::open();
    let digest = node.store_layer();
    let mut payload = envelope(&digest);
    payload.reservation = Some(reserve(&node.state.meta, &digest));
    node.stage(&payload);

    node.sweep().await;

    let usage = node.state.meta.quota_usage("store").unwrap().accounted_bytes;
    assert_eq!((usage.reserved, usage.committed), (0, LAYER.len() as u64));
}

#[tokio::test]
async fn test_a_retained_push_whose_content_is_gone_is_refused() {
    let node = Node::open();
    let key = node.stage(&envelope(&format!("sha256:{}", Digest::of(LAYER).as_str())));

    assert_eq!(
        (node.sweep().await, node.retained(&key)),
        (0, (IntentPhase::Pending, 1))
    );
}

#[tokio::test]
async fn test_a_retained_push_for_an_unconfigured_index_is_refused() {
    let node = Node::open();
    let digest = node.store_layer();
    let mut payload = envelope(&digest);
    payload.index = "retired".to_owned();
    let key = node.stage(&payload);

    assert_eq!(
        (node.sweep().await, node.retained(&key)),
        (0, (IntentPhase::Pending, 1))
    );
}

#[tokio::test]
async fn test_a_retained_push_a_size_limit_now_refuses_is_refused() {
    let node = Node::with_policy(Policy::compile(
        &PolicyConfig {
            max_artifact_size_bytes: Some(1),
            ..PolicyConfig::default()
        },
        str::to_owned,
    ));
    let digest = node.store_layer();
    let key = node.stage(&envelope(&digest));

    assert_eq!(
        (node.sweep().await, node.retained(&key)),
        (0, (IntentPhase::Pending, 1))
    );
}

#[tokio::test]
async fn test_a_retained_push_naming_an_unsupported_digest_is_refused() {
    let node = Node::open();
    let key = node.stage(&envelope("sha512:beef"));

    assert_eq!(
        (node.sweep().await, node.retained(&key)),
        (0, (IntentPhase::Pending, 1))
    );
}

#[tokio::test]
async fn test_an_envelope_from_a_later_build_is_left_pending() {
    let node = Node::open();
    let digest = node.store_layer();
    let mut payload = envelope(&digest);
    payload.version = PAYLOAD_VERSION + 1;
    let key = node.stage(&payload);

    assert_eq!(
        (node.sweep().await, node.retained(&key)),
        (0, (IntentPhase::Pending, 0))
    );
}

#[tokio::test]
async fn test_an_undecodable_envelope_is_left_pending() {
    let node = Node::open();
    let digest = node.store_layer();
    let key = node.stage_raw(
        &crate::registry::admission::intent_key("store", "app", &digest),
        &digest,
        b"not an envelope",
    );

    assert_eq!(
        (node.sweep().await, node.retained(&key)),
        (0, (IntentPhase::Pending, 0))
    );
}

#[tokio::test]
async fn test_another_ecosystems_intent_is_left_alone() {
    let node = Node::open();
    let digest = node.store_layer();
    let key = node.stage_raw("pypi:store:app:wheel", &digest, b"not an oci envelope");

    assert_eq!(
        (node.sweep().await, node.retained(&key)),
        (0, (IntentPhase::Pending, 0))
    );
}

#[tokio::test]
async fn test_a_content_read_fault_leaves_the_pass_pending() {
    let node = Node::open();
    node.fault_content_reads();
    let key = node.stage(&envelope(&format!("sha256:{}", Digest::of(LAYER).as_str())));

    assert_eq!(
        (node.sweep().await, node.retained(&key)),
        (0, (IntentPhase::Pending, 0))
    );
}

fn reserve(meta: &MetaStore, digest: &str) -> QuotaReservationRecord {
    meta.reserve_quota(
        NewQuotaReservation {
            repository: "store",
            resource: Some("app"),
            group: None,
            digest,
            bytes: LAYER.len() as u64,
            class: AccountingClass::Hosted,
            created_at_unix: 10,
        },
        QuotaLimits::default(),
    )
    .unwrap()
}

async fn drain(node: &Node, authority: &str, key: &str) -> bool {
    finalize_retained(&node.state, true, authority, key).await
}

#[tokio::test]
async fn test_a_drain_publishes_the_write_retained_for_its_authority() {
    let node = Node::open();
    let digest = node.store_layer();
    let key = node.stage(&envelope(&digest));

    let settled = drain(&node, &crate::name::authority_key("app"), &key).await;

    assert_eq!((settled, node.serves(&digest)), (true, true));
}

#[tokio::test]
async fn test_a_drain_settles_the_intent_it_published() {
    let node = Node::open();
    let digest = node.store_layer();
    let key = node.stage(&envelope(&digest));
    drain(&node, &crate::name::authority_key("app"), &key).await;

    assert_eq!(node.retained(&key), (IntentPhase::Admitted, 0));
}

#[tokio::test]
async fn test_a_drain_leaves_another_authoritys_write_alone() {
    let node = Node::open();
    let digest = node.store_layer();
    let key = node.stage(&envelope(&digest));

    let settled = drain(&node, &crate::name::authority_key("other"), &key).await;

    assert_eq!(
        (settled, node.serves(&digest), node.retained(&key)),
        (false, false, (IntentPhase::Pending, 0))
    );
}

#[tokio::test]
async fn test_a_drain_declines_a_key_another_ecosystem_minted() {
    let node = Node::open();
    let digest = node.store_layer();
    let key = node.stage_raw("pypi:store:app:wheel", &digest, b"not an oci envelope");

    assert_eq!(
        (
            drain(&node, &crate::name::authority_key("app"), &key).await,
            node.retained(&key)
        ),
        (false, (IntentPhase::Pending, 0))
    );
}

#[tokio::test]
async fn test_a_drain_declines_a_key_nothing_is_staged_under() {
    let node = Node::open();

    let settled = drain(
        &node,
        &crate::name::authority_key("app"),
        &crate::registry::admission::intent_key("store", "app", "sha256:gone"),
    )
    .await;

    assert!(!settled);
}

#[tokio::test]
async fn test_a_drain_leaves_a_write_it_could_not_read_pending() {
    let node = Node::open();
    node.fault_content_reads();
    let key = node.stage(&envelope(&format!("sha256:{}", Digest::of(LAYER).as_str())));

    assert_eq!(
        (
            drain(&node, &crate::name::authority_key("app"), &key).await,
            node.retained(&key)
        ),
        (false, (IntentPhase::Pending, 0))
    );
}

/// A drain settles what a home can publish even after the tick sweep has stopped offering it.
#[tokio::test]
async fn test_a_drain_publishes_a_write_the_sweep_gave_up_on() {
    let node = Node::open();
    let digest = node.store_layer();
    let key = node.stage(&envelope(&digest));
    for _ in 0..MAX_INTENT_REFUSALS {
        node.state.meta.refuse_intent(&key).unwrap();
    }
    assert_eq!(node.sweep().await, 0);

    let settled = drain(&node, &crate::name::authority_key("app"), &key).await;

    assert_eq!((settled, node.serves(&digest)), (true, true));
}
