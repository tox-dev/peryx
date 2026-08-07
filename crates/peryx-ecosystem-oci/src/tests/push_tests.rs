//! Hosted-push paths: blob upload (session and monolithic), mount, manifest PUT, and DELETE.

use std::collections::BTreeSet;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use peryx_storage::blob::Digest;
use peryx_storage::meta::{DriverBatch, OperationOutcomeQuery, OperationState};
use rstest::rstest;
use tower::ServiceExt as _;

use super::{
    EpochAuthority, app_with_indexes, auth, body_has_code, hosted, hosted_writable, oci_digest, proxy, scoped_index,
    send, send_body, send_with, writable_index,
};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

#[tokio::test]
async fn test_session_upload_then_pull() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"a-real-layer-of-bytes";
    let digest = oci_digest(blob);

    // Start the upload session.
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    assert!(!headers["docker-upload-uuid"].is_empty());

    // Send the bytes as a chunk.
    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Finish, committing under the digest.
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["docker-content-digest"], digest);
    assert_eq!(headers[header::LOCATION], format!("/v2/store/app/blobs/{digest}"));

    // The blob now serves from the store.
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
}

#[tokio::test]
async fn test_chunked_upload_resumes_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let blob = b"a-real-layer-of-bytes-that-arrives-across-a-restart";
    let digest = oci_digest(blob);
    let split = 20;

    // First process: open a session and stage the first chunk, then the process ends. Dropping the app
    // and its state leaves only the durable store and the staged file on disk.
    let location = {
        let (_state, app) = hosted_writable(&dir, TOKEN);
        let (status, headers, _) = send_body(
            &app,
            Method::POST,
            "/v2/store/app/blobs/uploads/",
            &[("authorization", &auth(TOKEN))],
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let location = headers[header::LOCATION].to_str().unwrap().to_owned();
        let (status, _, _) = send_body(
            &app,
            Method::PATCH,
            &location,
            &[("authorization", &auth(TOKEN))],
            blob[..split].to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        location
    };

    // Second process over the same directory, sharing no memory with the first.
    let (_state, app) = hosted_writable(&dir, TOKEN);

    // The session survived: status reports the bytes staged before the restart.
    let (status, headers, _) = send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers[header::RANGE], format!("0-{}", split - 1));

    // Resume with the remaining bytes at the recorded offset, then finalize under the digest.
    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[
            ("authorization", &auth(TOKEN)),
            ("content-range", &format!("{split}-{}", blob.len() - 1)),
        ],
        blob[split..].to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["docker-content-digest"], digest);

    // The blob assembled across the restart serves whole from the store.
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
}

#[tokio::test]
async fn test_concurrent_chunks_on_one_session_do_not_interleave() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();

    // Two chunks race to append at offset 0 on the same session. The per-session lock serializes them:
    // one lands, and the other, finding the offset already advanced, is refused as out of range instead
    // of overwriting or interleaving the first.
    let first = b"first-chunk-bytes";
    let second = b"other-chunk-value";
    let range = format!("0-{}", first.len() - 1);
    let token = auth(TOKEN);
    let request_headers = [("authorization", token.as_str()), ("content-range", range.as_str())];
    let (a, b) = tokio::join!(
        send_body(&app, Method::PATCH, &location, &request_headers, first.to_vec()),
        send_body(&app, Method::PATCH, &location, &request_headers, second.to_vec()),
    );

    let (winner, loser_status) = if a.0 == StatusCode::ACCEPTED {
        (&first[..], b.0)
    } else {
        (&second[..], a.0)
    };
    assert_eq!(loser_status, StatusCode::RANGE_NOT_SATISFIABLE);

    // The stage holds exactly the winning chunk: finalizing under its digest assembles that blob, proving
    // no bytes from the losing chunk reached the stage.
    let digest = oci_digest(winner);
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, winner);
}

#[tokio::test]
async fn test_session_upload_of_an_empty_blob() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = oci_digest(b"");

    // Open a session and finalize it with no chunk and no body: a zero-byte blob.
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["docker-content-digest"], digest);

    // The empty blob serves.
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(got.is_empty());
}

async fn stage_one_chunk(app: &axum::Router, blob: &[u8]) -> String {
    let (status, headers, _) = send_body(
        app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status, _, _) = send_body(
        app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    location
}

#[tokio::test]
async fn test_session_finish_with_an_invalid_digest_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let location = stage_one_chunk(&app, b"payload").await;

    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest=not-a-sha256-digest"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "DIGEST_INVALID"), "{body:?}");
}

/// A closing `PUT` whose `digest` query is absent, malformed, or names an unsupported algorithm is
/// rejected before its body is read, so the trailing chunk never reaches the stage and the session's
/// offset stays at the committed prefix. A client that fixes the query and resends the same chunk then
/// finalizes cleanly instead of appending those bytes a second time.
#[rstest]
#[case::missing("")]
#[case::malformed("?digest=not-a-sha256-digest")]
#[case::unsupported_algorithm("?digest=sha512:abcdef")]
#[tokio::test]
async fn test_session_finish_with_a_bad_digest_leaves_the_trailing_chunk_unappended(#[case] finish_query: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"a-committed-prefix-then-a-trailing-final-chunk";
    let split = 22;
    let range = format!("{split}-{}", blob.len() - 1);

    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        blob[..split].to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // The closing PUT carries the tail bytes but names a bad digest: it is rejected...
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        &format!("{location}{finish_query}"),
        &[("authorization", &auth(TOKEN)), ("content-range", &range)],
        blob[split..].to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "DIGEST_INVALID"), "{body:?}");

    // ...and status still reports only the committed prefix, so the tail did not reach the stage.
    let (status, headers, _) = send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers[header::RANGE], format!("0-{}", split - 1));

    // Resending the same tail under a valid digest finalizes without appending those bytes twice.
    let digest = oci_digest(blob);
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN)), ("content-range", &range)],
        blob[split..].to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
}

#[tokio::test]
async fn test_session_finish_with_a_wrong_digest_keeps_the_stage_for_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"the-actual-bytes";
    let location = stage_one_chunk(&app, blob).await;

    // Finishing under a valid but wrong digest fails without committing the blob.
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={}", oci_digest(b"different-bytes")),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_ne!(status, StatusCode::CREATED);

    // The staged bytes survive, so a retry with the right digest completes without re-uploading.
    let digest = oci_digest(blob);
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
}

#[tokio::test]
async fn test_session_finish_of_an_already_stored_blob_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"shared-layer-bytes";
    let digest = oci_digest(blob);
    // Store the blob once through a monolithic upload.
    let (status, _, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A session that re-pushes the same digest finalizes without reserving quota again.
    let location = stage_one_chunk(&app, blob).await;
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["docker-content-digest"], digest);
}

#[tokio::test]
async fn test_monolithic_upload() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"single-post-blob";
    let digest = oci_digest(blob);
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["docker-content-digest"], digest);
    assert!(
        state
            .blobs
            .head(&Digest::from_hex(digest.strip_prefix("sha256:").unwrap()).unwrap())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn test_monolithic_upload_digest_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let wrong = format!("sha256:{}", "0".repeat(64));
    let (status, _, body) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={wrong}"),
        &[("authorization", &auth(TOKEN))],
        b"mismatched".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(super::body_has_code(&body, "DIGEST_INVALID"), "{body:?}");
}

#[tokio::test]
async fn test_monolithic_upload_non_sha256_digest() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (status, _, body) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/?digest=sha512:abcdef",
        &[("authorization", &auth(TOKEN))],
        b"bytes".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(super::body_has_code(&body, "DIGEST_INVALID"), "{body:?}");
}

/// The operation id a hosted `store/app` push claims, keyed by index, repository, and digest.
fn blob_operation(digest: &str) -> String {
    format!("oci:store:app:{digest}")
}

#[tokio::test]
async fn test_monolithic_push_records_a_published_operation() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"ledger-recorded-layer";
    let digest = oci_digest(blob);
    let (status, _, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let record = state.meta.operation_outcome(&blob_operation(&digest)).unwrap().unwrap();
    assert_eq!(record.state, OperationState::Published);
}

#[tokio::test]
async fn test_monolithic_repush_keeps_the_blob_retrievable() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"twice-pushed-layer";
    let digest = oci_digest(blob);
    let uri = format!("/v2/store/app/blobs/uploads/?digest={digest}");
    for _ in 0..2 {
        let (status, headers, _) = send_body(
            &app,
            Method::POST,
            &uri,
            &[("authorization", &auth(TOKEN))],
            blob.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(headers["docker-content-digest"], digest);
    }
    // The re-push re-commits the content rather than short-circuiting on the recorded claim, so the blob
    // still serves; the ledger dedups the outcome to one published record.
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
    let record = state.meta.operation_outcome(&blob_operation(&digest)).unwrap().unwrap();
    assert_eq!(record.state, OperationState::Published);
}

#[tokio::test]
async fn test_monolithic_digest_mismatch_records_a_failed_operation() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let wrong = format!("sha256:{}", "0".repeat(64));
    let (status, _, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={wrong}"),
        &[("authorization", &auth(TOKEN))],
        b"mismatched".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let record = state.meta.operation_outcome(&blob_operation(&wrong)).unwrap().unwrap();
    assert_eq!(record.state, OperationState::Failed);
}

#[tokio::test]
async fn test_session_push_records_an_operation_the_view_reflects() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"session-recorded-layer";
    let digest = oci_digest(blob);
    let location = stage_one_chunk(&app, blob).await;
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let operation = blob_operation(&digest);
    assert_eq!(
        state.meta.operation_outcome(&operation).unwrap().unwrap().state,
        OperationState::Published
    );
    // The operations data source the availability view reads reflects the recorded write.
    let page = state
        .meta
        .list_operation_outcomes(&OperationOutcomeQuery::default())
        .unwrap();
    assert!(page.rows.iter().any(|row| row.operation == operation));
}

#[tokio::test]
async fn test_session_repush_keeps_the_blob_retrievable() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"session-repush-layer";
    let digest = oci_digest(blob);
    // Publish once through a monolithic upload, then re-push the same digest through a session.
    assert_eq!(
        send_body(
            &app,
            Method::POST,
            &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
            &[("authorization", &auth(TOKEN))],
            blob.to_vec(),
        )
        .await
        .0,
        StatusCode::CREATED
    );
    let location = stage_one_chunk(&app, blob).await;
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["docker-content-digest"], digest);
    // The session re-push commits over the existing content rather than short-circuiting, so the blob
    // still serves and the ledger holds one published record.
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
    let record = state.meta.operation_outcome(&blob_operation(&digest)).unwrap().unwrap();
    assert_eq!(record.state, OperationState::Published);
}

#[tokio::test]
async fn test_session_wrong_digest_records_a_failed_operation() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let location = stage_one_chunk(&app, b"the-actual-bytes").await;
    let wrong = oci_digest(b"different-bytes");
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={wrong}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_ne!(status, StatusCode::CREATED);
    let record = state.meta.operation_outcome(&blob_operation(&wrong)).unwrap().unwrap();
    assert_eq!(record.state, OperationState::Failed);
}

#[tokio::test]
async fn test_cross_repo_mount_of_an_existing_blob() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"already-here";
    let digest = upload_blob(&app, "store/other/repo", blob).await;
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?mount={digest}&from=store/other/repo"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let (get_status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(
        (
            status,
            headers["docker-content-digest"].to_str().unwrap(),
            get_status,
            got.as_ref(),
        ),
        (StatusCode::CREATED, digest.as_str(), StatusCode::OK, blob.as_slice())
    );
}

#[rstest]
#[case::get(Method::GET, "")]
#[case::head(Method::HEAD, "")]
#[case::contents(Method::GET, "/contents")]
#[tokio::test]
async fn test_blob_bytes_do_not_grant_another_repository_access(#[case] method: Method, #[case] suffix: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload_blob(&app, "store/private/app", b"private-layer").await;

    let (status, _, _) = send(&app, method, &format!("/v2/store/public/app/blobs/{digest}{suffix}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[rstest]
#[case::missing_from(true, None)]
#[case::absent_source(false, Some("other/repo"))]
#[tokio::test]
async fn test_mount_falls_back_to_a_session(#[case] present: bool, #[case] source: Option<&str>) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let existing = upload_blob(&app, "store/source/app", b"source-layer").await;
    let digest = if present {
        existing
    } else {
        format!("sha256:{}", "1".repeat(64))
    };
    let from = source.map_or_else(String::new, |source| format!("&from={source}"));
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/target/app/blobs/uploads/?mount={digest}{from}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    assert_eq!(
        (status, headers.contains_key(header::LOCATION)),
        (StatusCode::ACCEPTED, true)
    );
}

#[tokio::test]
async fn test_manifest_put_by_tag_then_pull_and_list() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let digest = oci_digest(manifest);

    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["docker-content-digest"], digest);

    let (status, headers, got) = send(&app, Method::GET, "/v2/store/app/manifests/v1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], MANIFEST_TYPE);
    assert_eq!(got, &manifest[..]);

    let (status, _, tags) = send(&app, Method::GET, "/v2/store/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert!(std::str::from_utf8(&tags).unwrap().contains("\"v1\""));
}

#[tokio::test]
async fn test_manifest_put_by_digest_and_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let manifest = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(manifest);
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let wrong = format!("sha256:{}", "2".repeat(64));
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{wrong}"),
        &[("authorization", &auth(TOKEN))],
        manifest.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(super::body_has_code(&body, "DIGEST_INVALID"), "{body:?}");
}

#[tokio::test]
async fn test_manifest_delete_by_tag() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let manifest = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(manifest);
    send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;
    let (status, _, _) = send_body(
        &app,
        Method::DELETE,
        "/v2/store/app/manifests/v1?reason=bad%20build",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _, _) = send(&app, Method::GET, "/v2/store/app/manifests/v1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, tags) = send(&app, Method::GET, "/v2/store/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&tags).unwrap()["tags"],
        serde_json::json!([])
    );
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &manifest[..]);
    let (status, _, body) = send_body(
        &app,
        Method::DELETE,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1/restore",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(headers["docker-content-digest"], digest);
    let (status, _, got) = send(&app, Method::GET, "/v2/store/app/manifests/v1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &manifest[..]);
    let (status, _, tags) = send(&app, Method::GET, "/v2/store/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&tags).unwrap()["tags"],
        serde_json::json!(["v1"])
    );
}

#[tokio::test]
async fn test_manifest_delete_by_digest_and_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let manifest = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(manifest);
    send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;
    let (status, _, _) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _, body) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[rstest]
#[case("missing")]
#[case("sha256:1111111111111111111111111111111111111111111111111111111111111111")]
#[tokio::test]
async fn test_manifest_restore_rejects_an_unknown_reference(#[case] reference: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{reference}/restore"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_manifest_delete_by_digest_retained_while_another_index_tags_it() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = app_with_indexes(
        &dir,
        vec![
            writable_index("store", "store", true, TOKEN),
            writable_index("keep", "keep", true, TOKEN),
        ],
    );
    let manifest = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(manifest);
    for route in ["store", "keep"] {
        let (status, _, _) = send_body(
            &app,
            Method::PUT,
            &format!("/v2/{route}/app/manifests/v1"),
            &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
            manifest.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, _, _) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // `keep` still tags the shared manifest, so it survives and serves.
    let (status, _, got) = send(&app, Method::GET, "/v2/keep/app/manifests/v1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &manifest[..]);
    // `store`'s own tag to it is cleaned, gone from its listing.
    let (status, _, tags) = send(&app, Method::GET, "/v2/store/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!std::str::from_utf8(&tags).unwrap().contains("\"v1\""), "{tags:?}");
}

#[tokio::test]
async fn test_manifest_delete_by_digest_retains_an_image_index_child() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let child = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let child_digest = oci_digest(child);
    send_body(
        &app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{child_digest}"),
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        child.to_vec(),
    )
    .await;
    let index = format!(
        r#"{{"schemaVersion":2,"manifests":[{{"digest":"{child_digest}","platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#
    );
    send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/latest",
        &[
            ("authorization", &auth(TOKEN)),
            ("content-type", "application/vnd.oci.image.index.v1+json"),
        ],
        index.into_bytes(),
    )
    .await;

    let (status, _, _) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{child_digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // The index still retains the child bytes, but the repository tombstone hides a direct pull.
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{child_digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/latest",
        &[("accept", MANIFEST_TYPE)],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_manifest_delete_by_digest_restores_the_same_bytes_and_tags() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let manifest = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(manifest);
    send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;
    let (status, _, _) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{digest}/restore"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(headers["oci-restored-tags"], "1");
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &manifest[..]);
    let (status, _, got) = send(&app, Method::GET, "/v2/store/app/manifests/v1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &manifest[..]);
}

#[tokio::test]
async fn test_republish_by_digest_leaves_deleted_tags_in_trash() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let manifest = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(manifest);
    send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;
    send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        send(&app, Method::GET, &format!("/v2/store/app/manifests/{digest}"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, Method::GET, "/v2/store/app/manifests/v1").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_digest_restore_keeps_a_concurrently_reused_tag() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let old = br#"{"schemaVersion":2,"annotations":{"build":"old"}}"#;
    let new = br#"{"schemaVersion":2,"annotations":{"build":"new"}}"#;
    let old_digest = oci_digest(old);
    let new_digest = oci_digest(new);
    for manifest in [old.as_slice(), new.as_slice()] {
        let reference = if manifest == old { "v1" } else { "next" };
        send_body(
            &app,
            Method::PUT,
            &format!("/v2/store/app/manifests/{reference}"),
            &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
            manifest.to_vec(),
        )
        .await;
    }
    send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{old_digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        new.to_vec(),
    )
    .await;

    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{old_digest}/restore"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(headers["oci-tag-conflicts"], "v1");
    let (status, headers, got) = send(&app, Method::GET, "/v2/store/app/manifests/v1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], new_digest);
    assert_eq!(got, &new[..]);
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{old_digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &old[..]);
}

#[tokio::test]
async fn test_manifest_trash_keeps_shared_layer_bytes_available() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"shared-layer";
    let blob_digest = upload_blob(&app, "store/app", blob).await;
    let manifest = format!(r#"{{"schemaVersion":2,"layers":[{{"digest":"{blob_digest}"}}]}}"#);
    let manifest_digest = oci_digest(manifest.as_bytes());
    send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.into_bytes(),
    )
    .await;
    send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{manifest_digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{blob_digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
}

#[tokio::test]
async fn test_blob_delete_and_missing_and_bad_digest() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload_blob(&app, "store/app", b"gc-me").await;

    let (status, _, _) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/blobs/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _, body) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/blobs/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "BLOB_UNKNOWN"), "{body:?}");

    let (status, _, body) = send_body(
        &app,
        Method::DELETE,
        "/v2/store/app/blobs/sha512:abc",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(super::body_has_code(&body, "DIGEST_INVALID"), "{body:?}");
}

#[tokio::test]
async fn test_blob_delete_clears_a_link_whose_bytes_are_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload_blob(&app, "store/app", b"lost-bytes").await;
    state
        .blobs
        .delete(&Digest::from_hex(digest.strip_prefix("sha256:").unwrap()).unwrap())
        .await
        .unwrap();

    let (first_status, _, _) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/blobs/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let (second_status, _, body) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/blobs/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(
        (first_status, second_status, body_has_code(&body, "BLOB_UNKNOWN")),
        (StatusCode::ACCEPTED, StatusCode::NOT_FOUND, true),
        "{body:?}"
    );
}

/// A hosted store whose one credential is a `ci` token that may push under `team/`.
fn scoped(dir: &tempfile::TempDir) -> axum::Router {
    let index = Index {
        acl: IndexAcl {
            anonymous_read: true,
            tokens: vec![NamedToken {
                name: "ci".to_owned(),
                secret: TOKEN.to_owned(),
                grants: vec![Grant {
                    projects: vec![Glob::new("team/*")],
                    actions: BTreeSet::from([Action::Write]),
                }],
                expires_at: None,
            }],
        },
        ..super::oci_index("store", "store", IndexKind::Hosted { volatile: true })
    };
    app_with_indexes(dir, vec![index]).1
}

#[tokio::test]
async fn test_a_scoped_token_pushes_a_repository_its_glob_covers() {
    let dir = tempfile::tempdir().unwrap();
    let app = scoped(&dir);

    let (status, _, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/team/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_a_scoped_token_may_not_push_outside_its_glob() {
    let dir = tempfile::tempdir().unwrap();
    let app = scoped(&dir);

    let (status, _, body) = send_body(
        &app,
        Method::POST,
        "/v2/store/other/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body_has_code(&body, "DENIED"), "{body:?}");
}

#[tokio::test]
async fn test_a_write_only_token_may_not_delete() {
    let dir = tempfile::tempdir().unwrap();
    let app = scoped(&dir);

    let (status, _, _) = send_body(
        &app,
        Method::DELETE,
        "/v2/store/team/app/manifests/v1",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/team/app/manifests/v1/restore",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_push_requires_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (status, headers, _) = send_body(&app, Method::POST, "/v2/store/app/blobs/uploads/", &[], Vec::new()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers[header::WWW_AUTHENTICATE], "Basic realm=\"peryx\"");
}

#[tokio::test]
async fn test_push_rejects_a_wrong_token() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (status, _, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth("wrong"))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_push_to_a_store_with_no_token_is_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted(&dir);
    let (status, _, body) = send_body(&app, Method::POST, "/v2/store/app/blobs/uploads/", &[], Vec::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(super::body_has_code(&body, "DENIED"), "{body:?}");
}

#[tokio::test]
async fn test_patch_to_an_unknown_session_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (status, _, body) = send_body(
        &app,
        Method::PATCH,
        "/v2/store/app/blobs/uploads/deadbeef",
        &[("authorization", &auth(TOKEN))],
        b"chunk".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "BLOB_UPLOAD_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_finish_an_unknown_session_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/blobs/uploads/deadbeef?digest=sha256:x",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_finish_without_a_digest_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status_put, _, body) = send_body(
        &app,
        Method::PUT,
        &location,
        &[("authorization", &auth(TOKEN))],
        b"bytes".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(status_put, StatusCode::BAD_REQUEST);
    assert!(super::body_has_code(&body, "DIGEST_INVALID"), "{body:?}");
}

#[tokio::test]
async fn test_every_write_verb_is_denied_on_a_read_only_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let cases = [
        (Method::POST, "/v2/hub/app/blobs/uploads/".to_owned()),
        (Method::PATCH, "/v2/hub/app/blobs/uploads/xyz".to_owned()),
        (Method::PUT, "/v2/hub/app/blobs/uploads/xyz?digest=sha256:x".to_owned()),
        (Method::DELETE, "/v2/hub/app/manifests/v1".to_owned()),
        (Method::DELETE, format!("/v2/hub/app/blobs/sha256:{}", "a".repeat(64))),
    ];
    for (method, uri) in cases {
        let (status, _, _) = send_body(&app, method.clone(), &uri, &[], b"x".to_vec()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
}

#[tokio::test]
async fn test_write_to_an_unresolvable_name_is_name_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, _, body) = send_body(&app, Method::POST, "/v2/other/app/blobs/uploads/", &[], Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "NAME_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_manifest_put_body_error_is_a_gateway_error() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let erroring = futures_util::stream::iter(vec![
        Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{")),
        Err(std::io::Error::other("boom")),
    ]);
    let request = Request::builder()
        .method(Method::PUT)
        .uri("/v2/store/app/manifests/v1")
        .header("authorization", auth(TOKEN))
        .body(Body::from_stream(erroring))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let _ = response.into_body().collect().await;
}

#[tokio::test]
async fn test_monolithic_upload_body_read_error_is_a_gateway_error() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    // A monolithic `POST` streams straight into the blob, so a mid-transfer failure is a gateway error.
    let erroring = futures_util::stream::iter(vec![
        Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"partial")),
        Err(std::io::Error::other("boom")),
    ]);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v2/store/app/blobs/uploads/?digest=sha256:0000000000000000000000000000000000000000000000000000000000000000")
        .header("authorization", auth(TOKEN))
        .body(Body::from_stream(erroring))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let _ = response.into_body().collect().await;
}

#[tokio::test]
async fn test_manifest_push_rejects_unsupported_media_type() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", "text/plain")],
        br#"{"schemaVersion":2}"#.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "MANIFEST_INVALID"), "{body:?}");
}

#[tokio::test]
async fn test_manifest_push_over_the_size_limit_is_413() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    // One byte past the 4 MiB manifest cap: the body read hits axum's length limit before validation.
    let oversize = vec![b'{'; 4 * 1024 * 1024 + 1];
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        oversize,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body_has_code(&body, "SIZE_INVALID"), "{body:?}");
}

#[tokio::test]
async fn test_manifest_push_ignores_content_type_parameters() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[
            ("authorization", &auth(TOKEN)),
            ("content-type", &format!("{MANIFEST_TYPE}; charset=utf-8")),
        ],
        manifest.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, headers, _) = send(&app, Method::GET, "/v2/store/app/manifests/v1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], MANIFEST_TYPE);
}

#[tokio::test]
async fn test_manifest_push_rejects_missing_referenced_blob() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let manifest = format!(
        r#"{{"schemaVersion":2,"config":{{"digest":"sha256:{}"}},"layers":[]}}"#,
        "a".repeat(64)
    );
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "MANIFEST_BLOB_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_manifest_push_rejects_an_unsupported_blob_digest() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let manifest = br#"{"schemaVersion":2,"config":{"digest":"md5:00112233445566778899aabbccddeeff"},"layers":[]}"#;
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "MANIFEST_BLOB_UNKNOWN"), "{body:?}");
}

#[rstest]
#[case::same_repository("store/app", StatusCode::CREATED, None)]
#[case::other_repository("store/other", StatusCode::BAD_REQUEST, Some("MANIFEST_BLOB_UNKNOWN"))]
#[tokio::test]
async fn test_manifest_push_checks_referenced_blob_membership(
    #[case] blob_repository: &str,
    #[case] expected_status: StatusCode,
    #[case] expected_error: Option<&str>,
) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let config = upload_blob(&app, blob_repository, b"config-bytes").await;
    let manifest = format!(r#"{{"schemaVersion":2,"config":{{"digest":"{config}"}},"layers":[]}}"#);
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.into_bytes(),
    )
    .await;

    assert_eq!(
        (status, expected_error.map(|code| body_has_code(&body, code))),
        (expected_status, expected_error.map(|_| true)),
        "{body:?}"
    );
}

#[tokio::test]
async fn test_manifest_push_rejects_index_with_missing_child() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let index = format!(
        r#"{{"schemaVersion":2,"manifests":[{{"digest":"sha256:{}"}}]}}"#,
        "b".repeat(64)
    );
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[
            ("authorization", &auth(TOKEN)),
            ("content-type", "application/vnd.oci.image.index.v1+json"),
        ],
        index.into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "MANIFEST_BLOB_UNKNOWN"), "{body:?}");
}

#[rstest]
#[case::metered_same_repository("store/app", true, StatusCode::CREATED, None, StatusCode::OK)]
#[case::unmetered_other_repository("store/other", false, StatusCode::CREATED, None, StatusCode::OK)]
#[case::metered_other_repository(
    "store/other",
    true,
    StatusCode::BAD_REQUEST,
    Some("MANIFEST_BLOB_UNKNOWN"),
    StatusCode::NOT_FOUND
)]
#[tokio::test]
async fn test_manifest_push_checks_referenced_child_membership(
    #[case] child_repository: &str,
    #[case] metered: bool,
    #[case] expected_status: StatusCode,
    #[case] expected_error: Option<&str>,
    #[case] expected_visibility: StatusCode,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut index = writable_index("store", "store", true, TOKEN);
    if metered {
        index.policy = Policy::compile(
            &PolicyConfig {
                max_accounted_bytes: Some(u64::MAX),
                ..PolicyConfig::default()
            },
            str::to_owned,
        );
    }
    let (_state, app) = app_with_indexes(&dir, vec![index]);
    let child = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let child_digest = oci_digest(child);
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("/v2/{child_repository}/manifests/{child_digest}"),
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        child.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let index = format!(r#"{{"schemaVersion":2,"manifests":[{{"digest":"{child_digest}"}}]}}"#);
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/latest",
        &[
            ("authorization", &auth(TOKEN)),
            ("content-type", "application/vnd.oci.image.index.v1+json"),
        ],
        index.into_bytes(),
    )
    .await;
    assert_eq!(
        (
            status,
            expected_error.map(|code| body_has_code(&body, code)),
            send(&app, Method::GET, &format!("/v2/store/app/manifests/{child_digest}"))
                .await
                .0,
        ),
        (expected_status, expected_error.map(|_| true), expected_visibility),
        "{body:?}"
    );
}

#[tokio::test]
async fn test_upload_session_is_scoped_to_its_index() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = app_with_indexes(
        &dir,
        vec![
            writable_index("store", "store", true, TOKEN),
            writable_index("other", "other", true, "other-token"),
        ],
    );
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let session = location.rsplit('/').next().unwrap();

    // A client authorized for `other` cannot reach `store`'s session by its id, even to read it.
    let attack = format!("/v2/other/app/blobs/uploads/{session}");
    for method in [Method::GET, Method::PATCH] {
        let (status, _, body) = send_body(
            &app,
            method,
            &attack,
            &[("authorization", &auth("other-token"))],
            b"x".to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body_has_code(&body, "BLOB_UPLOAD_UNKNOWN"), "{body:?}");
    }

    // The owner still uses its own session normally.
    let (status, _, _) = send_body(
        &app,
        Method::GET,
        &location,
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        b"x".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[rstest]
#[case::status(Method::GET, &[])]
#[case::append(Method::PATCH, b"attacker")]
#[case::finish(Method::PUT, b"attacker")]
#[case::cancel(Method::DELETE, &[])]
#[tokio::test]
async fn test_upload_session_is_scoped_to_its_repository(#[case] method: Method, #[case] body: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let location = start_session(&app, "store/app", TOKEN).await;
    let session = location.rsplit('/').next().unwrap();

    let attack = format!("/v2/store/other/blobs/uploads/{session}");
    let (status, _, response) =
        send_body(&app, method, &attack, &[("authorization", &auth(TOKEN))], body.to_vec()).await;
    assert_eq!(
        (status, body_has_code(&response, "BLOB_UPLOAD_UNKNOWN")),
        (StatusCode::NOT_FOUND, true),
        "{response:?}"
    );

    let (status, _, _) = send_body(
        &app,
        Method::GET,
        &location,
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn test_upload_session_can_resume_with_another_authorized_credential() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = scoped_index("store", "store", "writer-a", "secret-a", "app", &[Action::Write]);
    index.acl.tokens.push(NamedToken {
        name: "writer-b".to_owned(),
        secret: "secret-b".to_owned(),
        grants: vec![Grant {
            projects: vec![Glob::new("app")],
            actions: BTreeSet::from([Action::Write]),
        }],
        expires_at: None,
    });
    let (_state, app) = app_with_indexes(&dir, vec![index]);
    let location = start_session(&app, "store/app", "secret-a").await;

    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth("secret-b"))],
        b"chunk".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_upload_session_id_is_128_bit_lowercase_hex() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);

    let location = start_session(&app, "store/app", TOKEN).await;
    let session = location.rsplit('/').next().unwrap();

    assert_eq!(
        (
            session.len(),
            session
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        ),
        (32, true)
    );
}

async fn start_session(app: &axum::Router, name: &str, token: &str) -> String {
    let (status, headers, _) = send_body(
        app,
        Method::POST,
        &format!("/v2/{name}/blobs/uploads/"),
        &[("authorization", &auth(token))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    headers[header::LOCATION].to_str().unwrap().to_owned()
}

#[tokio::test]
async fn test_blob_delete_retains_a_referenced_blob() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload_blob(&app, "store/app", b"referenced-layer").await;
    let layer = Digest::from_hex(digest.strip_prefix("sha256:").unwrap()).unwrap();
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"digest":"{digest}"}},"layers":[]}}"#
    );
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/blobs/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // Retained: a manifest still references it, so the shared blob is not unlinked.
    assert!(state.blobs.head(&layer).await.unwrap().is_some());
}

#[tokio::test]
async fn test_blob_delete_removes_only_the_target_repository_link() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"shared-layer";
    let digest = upload_blob(&app, "store/source", blob).await;
    let target_digest = upload_blob(&app, "store/target", blob).await;
    let (warm_status, _, _) = send(&app, Method::GET, &format!("/v2/store/target/blobs/{digest}")).await;

    let (status, _, _) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/target/blobs/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let (target_status, _, _) = send(&app, Method::GET, &format!("/v2/store/target/blobs/{digest}")).await;
    let (source_status, _, got) = send(&app, Method::GET, &format!("/v2/store/source/blobs/{digest}")).await;
    assert_eq!(
        (
            warm_status,
            status,
            target_digest,
            target_status,
            source_status,
            got.as_ref(),
            state
                .blobs
                .head(&Digest::from_hex(digest.strip_prefix("sha256:").unwrap()).unwrap())
                .await
                .unwrap()
                .is_some(),
        ),
        (
            StatusCode::OK,
            StatusCode::ACCEPTED,
            digest,
            StatusCode::NOT_FOUND,
            StatusCode::OK,
            blob.as_slice(),
            true,
        )
    );
}

#[tokio::test]
async fn test_blob_membership_cache_evicts_oldest_link_over_limit() {
    const CACHE_BYTES: usize = 8 << 20;
    const INDEX_BYTES: usize = 1 << 20;
    let index_name = "x".repeat(INDEX_BYTES);
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = app_with_indexes(&dir, vec![writable_index(&index_name, "store", true, TOKEN)]);
    let digest = upload_blob(&app, "store/source", b"shared-layer").await;
    let repos = (0..=CACHE_BYTES / INDEX_BYTES)
        .map(|index| format!("repo{index}"))
        .collect::<Vec<_>>();
    let keys = repos
        .iter()
        .map(|repo| format!("oci\0bm\0{index_name}\0{repo}\0{digest}"))
        .collect::<Vec<_>>();
    let mut puts = DriverBatch::new();
    for key in &keys {
        puts.put(key.clone(), Vec::new());
    }
    state.meta.commit_driver_batch(&puts, false).unwrap();
    for repo in &repos {
        send(&app, Method::GET, &format!("/v2/store/{repo}/blobs/{digest}")).await;
    }
    let newest_index = repos.len() - 1;
    let mut deletes = DriverBatch::new();
    deletes.delete(keys[0].clone());
    deletes.delete(keys[newest_index].clone());
    state.meta.commit_driver_batch(&deletes, false).unwrap();

    let oldest = send(&app, Method::GET, &format!("/v2/store/{}/blobs/{digest}", repos[0]))
        .await
        .0;
    let newest = send(
        &app,
        Method::GET,
        &format!("/v2/store/{}/blobs/{digest}", repos[newest_index]),
    )
    .await
    .0;
    assert_eq!((oldest, newest), (StatusCode::NOT_FOUND, StatusCode::OK));
}

async fn upload_blob(app: &axum::Router, name: &str, blob: &[u8]) -> String {
    let digest = oci_digest(blob);
    let (status, _, _) = send_body(
        app,
        Method::POST,
        &format!("/v2/{name}/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    digest
}

#[tokio::test]
async fn test_abandoned_upload_sessions_expire() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (state, app) = super::hosted_with_clock(&dir, TOKEN, Arc::new(move || ticking.load(Ordering::Relaxed)));

    // Open a session, then abandon it.
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();

    // Age past the TTL and run the background sweep, which reclaims the abandoned session.
    now.store(1000 + 3601, Ordering::Relaxed);
    state
        .driver_for(crate::ECOSYSTEM)
        .unwrap()
        .reclaim_idle(state.serving.clone())
        .await;

    // The abandoned session is gone.
    let (status, _, body) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        b"x".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "BLOB_UPLOAD_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_background_sweep_removes_an_abandoned_upload_file() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (state, app) = super::hosted_with_clock(&dir, TOKEN, Arc::new(move || ticking.load(Ordering::Relaxed)));

    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    // A chunk stages bytes to a durable per-session file.
    send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        b"abc".to_vec(),
    )
    .await;
    let staged = std::fs::read_dir(dir.path().join("blobs").join("uploads"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(staged.is_file());

    now.store(1000 + 3600, Ordering::Relaxed);
    let reclaimed = state
        .driver_for(crate::ECOSYSTEM)
        .unwrap()
        .reclaim_idle(state.serving.clone())
        .await;

    assert_eq!(reclaimed, 1);
    assert!(!staged.exists());
}

#[tokio::test]
async fn test_cancel_removes_the_upload_file_before_responding() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let location = start_session(&app, "store/app", TOKEN).await;
    // A chunk stages bytes to a durable per-session file that the cancel must remove.
    send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        b"abc".to_vec(),
    )
    .await;
    let staged = std::fs::read_dir(dir.path().join("blobs").join("uploads"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();

    let (status, _, _) = send_body(
        &app,
        Method::DELETE,
        &location,
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!staged.exists());
}

#[tokio::test]
async fn test_active_upload_session_survives_eviction() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (state, app) = super::hosted_with_clock(&dir, TOKEN, Arc::new(move || ticking.load(Ordering::Relaxed)));

    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();

    // A chunk sent well into the upload refreshes the session's last-activity clock.
    now.store(3000, Ordering::Relaxed);
    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        b"abc".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    now.store(3000 + 3599, Ordering::Relaxed);
    let reclaimed = state
        .driver_for(crate::ECOSYSTEM)
        .unwrap()
        .reclaim_idle(state.serving.clone())
        .await;
    assert_eq!(reclaimed, 0);

    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        b"def".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_upload_status_read_refreshes_the_session_ttl() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (state, app) = super::hosted_with_clock(&dir, TOKEN, Arc::new(move || ticking.load(Ordering::Relaxed)));

    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();

    // Polling status is activity too: it refreshes the session's last-activity clock.
    now.store(3000, Ordering::Relaxed);
    let (status, _, _) = send_body(
        &app,
        Method::GET,
        &location,
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    now.store(3000 + 3599, Ordering::Relaxed);
    let reclaimed = state
        .driver_for(crate::ECOSYSTEM)
        .unwrap()
        .reclaim_idle(state.serving.clone())
        .await;
    assert_eq!(reclaimed, 0);

    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        b"abc".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_patch_body_read_error_keeps_session_resumable() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"a-real-layer-of-bytes";
    let (landed, rest) = blob.split_at(8);
    let digest = oci_digest(blob);

    let (_, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();

    // A chunk that streams a few bytes and then hits a read error: the bytes that landed must stay.
    let chunks = futures_util::stream::iter([
        Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(landed)),
        Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset")),
    ]);
    let request = Request::builder()
        .method(Method::PATCH)
        .uri(&location)
        .header("authorization", auth(TOKEN))
        .body(Body::from_stream(chunks))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    // The session survives at the offset the accepted bytes reached, so the client can resume.
    let (status, headers, _) = send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers[header::RANGE], format!("0-{}", landed.len() - 1));

    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        rest.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
}

#[tokio::test]
async fn test_put_without_digest_keeps_session_resumable() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"a-real-layer-of-bytes";
    let digest = oci_digest(blob);

    let (_, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // Closing the session without a digest cannot commit, but the staged bytes must not be lost.
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        &location,
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "DIGEST_INVALID"), "{body:?}");

    let (status, headers, _) = send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers[header::RANGE], format!("0-{}", blob.len() - 1));

    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
}

#[rstest]
#[case::home_ingress(true)]
#[case::non_home_ingress(false)]
#[tokio::test]
async fn test_monolithic_upload_at_the_current_epoch(#[case] homed: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    // A repository at a settled epoch admits the push, and a non-home node at that epoch admits it too.
    state.set_ownership_authority(EpochAuthority::settled(5, homed));
    let blob = b"single-post-blob-under-authority";
    let digest = oci_digest(blob);

    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["docker-content-digest"], digest);

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
}

#[tokio::test]
async fn test_monolithic_upload_under_a_superseded_epoch_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    // The push leases epoch 5, but the repository home advanced to 6 while the bytes arrived.
    state.set_ownership_authority(EpochAuthority::superseded(5, 6));
    let blob = b"a-layer-that-loses-the-race";
    let digest = oci_digest(blob);

    let (status, _, body) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_has_code(&body, "UNAVAILABLE"), "{body:?}");
    assert_no_topology(&body);

    // The fenced push changed no placement: the repository does not serve the blob.
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_session_finish_under_a_superseded_epoch_keeps_the_stage_for_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let group = EpochAuthority::superseded(5, 6);
    state.set_ownership_authority(group.clone());
    let blob = b"a-resumable-layer-across-a-transfer";
    let digest = oci_digest(blob);
    let location = stage_one_chunk(&app, blob).await;

    // The finalize leases epoch 5, which the advanced authority no longer admits.
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_has_code(&body, "UNAVAILABLE"), "{body:?}");

    // The durable stage survived, so upload status stays usable.
    let (status, headers, _) = send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(headers.contains_key(header::RANGE));

    // Once the node catches up to the new epoch, the same retry finalizes without re-uploading a byte.
    group.settle();
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["docker-content-digest"], digest);
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
}

#[tokio::test]
async fn test_mount_under_a_superseded_epoch_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload_blob(&app, "store/source/app", b"source-layer-to-mount").await;
    // The mount into the target leases epoch 5, but the target's home advanced to 6.
    state.set_ownership_authority(EpochAuthority::superseded(5, 6));

    let (status, _, body) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/target/app/blobs/uploads/?mount={digest}&from=store/source/app"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_has_code(&body, "UNAVAILABLE"), "{body:?}");

    // The fenced mount recorded no membership: the target does not serve the blob.
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/store/target/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_mount_at_the_current_epoch_publishes_the_blob() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let blob = b"source-layer-to-mount";
    let digest = upload_blob(&app, "store/source/app", blob).await;
    state.set_ownership_authority(EpochAuthority::settled(7, true));

    let (status, _, _) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/target/app/blobs/uploads/?mount={digest}&from=store/source/app"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/store/target/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
}

/// A stale-epoch upload error is protocol-safe: it names no leader, datacenter, or membership, so it
/// leaks no internal control topology.
fn assert_no_topology(body: &bytes::Bytes) {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    for leaked in ["leader", "voter", "datacenter", "://", ".internal"] {
        assert!(!text.contains(leaked), "stale-epoch response leaked {leaked:?}: {text}");
    }
}
