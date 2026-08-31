//! A registry push answers a success code only once the configured write-acknowledgement policy has
//! proven the write durable, and answers a retryable `503` while it has not.

use std::sync::Arc;

use axum::http::{Method, StatusCode, header};
use peryx_driver::AppState;
use peryx_ha::WriteDurability;
use peryx_storage::meta::OperationState;
use rstest::rstest;

use super::{
    ObservedWrite, ScriptedDurability, auth, body_has_code, hosted_writable_distributed_with_durability, oci_digest,
    send, send_body,
};

const TOKEN: &str = "s3cret";
const LAYER: &[u8] = b"a-layer-whose-durability-must-be-proven";

const CONFIRMED: WriteDurability = WriteDurability::Confirmed {
    scope: peryx_core::BlobDurability::Filesystem,
};

/// The terminal blob writes a push can end in, each of which publishes a repository membership and
/// therefore has to acknowledge before it answers `201`.
#[derive(Debug, Clone, Copy)]
enum Push {
    Monolithic,
    Resumable,
    Mount,
}

impl Push {
    /// Run the push and return its response status and `Retry-After` header.
    async fn run(self, app: &axum::Router, blob: &[u8]) -> (StatusCode, Option<String>, bytes::Bytes) {
        let digest = oci_digest(blob);
        let (status, headers, body) = match self {
            Self::Monolithic => {
                send_body(
                    app,
                    Method::POST,
                    &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
                    &[("authorization", &auth(TOKEN))],
                    blob.to_vec(),
                )
                .await
            }
            Self::Resumable => {
                let (_, headers, _) = send_body(
                    app,
                    Method::POST,
                    "/v2/store/app/blobs/uploads/",
                    &[("authorization", &auth(TOKEN))],
                    Vec::new(),
                )
                .await;
                let location = headers[header::LOCATION].to_str().unwrap().to_owned();
                send_body(
                    app,
                    Method::PATCH,
                    &location,
                    &[("authorization", &auth(TOKEN))],
                    blob.to_vec(),
                )
                .await;
                send_body(
                    app,
                    Method::PUT,
                    &format!("{location}?digest={digest}"),
                    &[("authorization", &auth(TOKEN))],
                    Vec::new(),
                )
                .await
            }
            Self::Mount => {
                send_body(
                    app,
                    Method::POST,
                    &format!("/v2/store/other/blobs/uploads/?mount={digest}&from=store/app"),
                    &[("authorization", &auth(TOKEN))],
                    Vec::new(),
                )
                .await
            }
        };
        let retry_after = headers
            .get(header::RETRY_AFTER)
            .map(|value| value.to_str().unwrap().to_owned());
        (status, retry_after, body)
    }

    /// The repository the push publishes membership into, and so the authority its write acknowledges
    /// under.
    const fn repo(self) -> &'static str {
        match self {
            Self::Monolithic | Self::Resumable => "app",
            Self::Mount => "other",
        }
    }

    /// A mount publishes a blob that is already stored, so its fixture needs that prior push. It runs
    /// against a separate app so the acknowledgement under test sees only the mount's own write.
    async fn prepare(
        self,
        dir: &tempfile::TempDir,
        blob: &[u8],
    ) -> (Arc<AppState>, Arc<ScriptedDurability>, axum::Router) {
        let (state, durability, app) = hosted_writable_distributed_with_durability(dir, TOKEN, CONFIRMED);
        if matches!(self, Self::Mount) {
            assert_eq!(Self::Monolithic.run(&app, blob).await.0, StatusCode::CREATED);
        }
        (state, durability, app)
    }
}

/// A push whose configured durability the resolver cannot prove must not tell the client the blob is
/// safe. It answers a retryable `503` and leaves the operation pending for a retry to finish. The
/// content and its membership are committed either way - that is what gives the metadata frontier
/// something to acknowledge - so the withheld promise is the success code, not the write.
#[rstest]
#[case::monolithic_pending(Push::Monolithic, WriteDurability::Pending)]
#[case::monolithic_unavailable(Push::Monolithic, WriteDurability::Unavailable)]
#[case::resumable_pending(Push::Resumable, WriteDurability::Pending)]
#[case::resumable_unavailable(Push::Resumable, WriteDurability::Unavailable)]
#[case::mount_pending(Push::Mount, WriteDurability::Pending)]
#[case::mount_unavailable(Push::Mount, WriteDurability::Unavailable)]
#[tokio::test]
async fn test_a_push_withholds_success_until_the_configured_durability_is_proven(
    #[case] push: Push,
    #[case] verdict: WriteDurability,
) {
    let dir = tempfile::tempdir().unwrap();
    let (state, durability, app) = push.prepare(&dir, LAYER).await;
    durability.answer(verdict);

    let (status, retry_after, body) = push.run(&app, LAYER).await;

    assert_eq!(
        (status, retry_after.as_deref(), body_has_code(&body, "UNAVAILABLE")),
        (StatusCode::SERVICE_UNAVAILABLE, Some("1"), true)
    );
    assert_eq!(
        state
            .serving
            .meta
            .operation_outcome(&operation(push, LAYER))
            .unwrap()
            .unwrap()
            .state,
        OperationState::Pending
    );
}

/// The same push against a resolver that proves the policy answers the spec's `201` and publishes the
/// operation, so the gate withholds success rather than breaking it.
#[rstest]
#[case::monolithic(Push::Monolithic)]
#[case::resumable(Push::Resumable)]
#[case::mount(Push::Mount)]
#[tokio::test]
async fn test_a_push_answers_created_once_the_configured_durability_is_proven(#[case] push: Push) {
    let dir = tempfile::tempdir().unwrap();
    let (state, _durability, app) = push.prepare(&dir, LAYER).await;

    let (status, retry_after, _) = push.run(&app, LAYER).await;

    assert_eq!((status, retry_after), (StatusCode::CREATED, None));
    assert_eq!(
        state
            .serving
            .meta
            .operation_outcome(&operation(push, LAYER))
            .unwrap()
            .unwrap()
            .state,
        OperationState::Published
    );
    assert_eq!(send(&app, Method::GET, &blob_url(push, LAYER)).await.0, StatusCode::OK);
}

/// A client retrying the identical request after an unproven acknowledgement finishes the same
/// operation rather than starting a second one, because the content-addressed commit and the
/// membership upsert both replay without a further effect.
#[rstest]
#[case::monolithic(Push::Monolithic)]
#[case::resumable(Push::Resumable)]
#[case::mount(Push::Mount)]
#[tokio::test]
async fn test_a_retry_finishes_the_operation_the_unproven_push_left_pending(#[case] push: Push) {
    let dir = tempfile::tempdir().unwrap();
    let (state, durability, app) = push.prepare(&dir, LAYER).await;
    durability.answer(WriteDurability::Pending);
    assert_eq!(push.run(&app, LAYER).await.0, StatusCode::SERVICE_UNAVAILABLE);

    durability.answer(CONFIRMED);
    let (status, _, _) = push.run(&app, LAYER).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        state
            .serving
            .meta
            .operation_outcome(&operation(push, LAYER))
            .unwrap()
            .unwrap()
            .state,
        OperationState::Published
    );
}

/// Each terminal path presents the digest, the byte length and the journal serial its own commit
/// earned, so the resolver weighs this write's evidence rather than the backend's advertised
/// guarantees.
#[rstest]
#[case::monolithic(Push::Monolithic)]
#[case::resumable(Push::Resumable)]
#[case::mount(Push::Mount)]
#[tokio::test]
async fn test_a_push_presents_the_evidence_its_own_commit_earned(#[case] push: Push) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, durability, app) = push.prepare(&dir, LAYER).await;
    durability.forget();

    assert_eq!(push.run(&app, LAYER).await.0, StatusCode::CREATED);

    assert_eq!(
        durability.observed(),
        vec![ObservedWrite {
            digest: oci_digest(LAYER).strip_prefix("sha256:").unwrap().to_owned(),
            size: LAYER.len() as u64,
            authority: format!("oci:{}", push.repo()),
            journaled: true,
            evidence: peryx_core::WriteEvidence::NodeLocal,
        }]
    );
}

fn operation(push: Push, blob: &[u8]) -> String {
    format!("oci:store:{}:{}", push.repo(), oci_digest(blob))
}

fn blob_url(push: Push, blob: &[u8]) -> String {
    format!("/v2/store/{}/blobs/{}", push.repo(), oci_digest(blob))
}
