use std::sync::Arc;

use axum::http::{Method, StatusCode};
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use peryx_storage::meta::{AccountingClass, NewQuotaReservation};

use super::{app_with, auth, bind_ownership, body_has_code, oci_digest, send, send_body, send_with};
use crate::quota_reservation;

#[test]
fn test_quota_reservation_preserves_oci_identity() {
    for (case, tag) in [("tagged manifest", Some("stable")), ("blob", None)] {
        assert_eq!(
            (
                case,
                quota_reservation(
                    "images",
                    "team/api",
                    tag,
                    "sha256:abc",
                    42,
                    AccountingClass::Generated,
                    100,
                ),
            ),
            (
                case,
                NewQuotaReservation {
                    repository: "images",
                    resource: Some("team/api"),
                    group: tag,
                    digest: "sha256:abc",
                    bytes: 42,
                    class: AccountingClass::Generated,
                    created_at_unix: 100,
                },
            )
        );
    }
}

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

fn quota_store(dir: &tempfile::TempDir, limits: &PolicyConfig) -> (Arc<peryx_driver::AppState>, axum::Router) {
    app_with(dir, quota_index(limits))
}

fn quota_store_distributed(
    dir: &tempfile::TempDir,
    limits: &PolicyConfig,
) -> (Arc<peryx_driver::AppState>, axum::Router) {
    super::app_with_distributed(dir, quota_index(limits))
}

fn quota_index(limits: &PolicyConfig) -> Index {
    Index {
        acl: crate::tests::writer_acl(TOKEN),
        policy: Policy::compile(limits, str::to_owned),
        ..super::oci_index("store", "store", IndexKind::Hosted { volatile: true })
    }
}

fn tag_quota_store(
    dir: &tempfile::TempDir,
    max_tags_per_repository: u64,
) -> (Arc<peryx_driver::AppState>, axum::Router) {
    let policy = Policy::compile(&PolicyConfig::default(), str::to_owned).with_capabilities(
        crate::policy::compile_capabilities(&crate::policy::OciPolicyConfig {
            max_tags_per_repository: Some(max_tags_per_repository),
        }),
    );
    app_with(
        dir,
        Index {
            acl: crate::tests::writer_acl(TOKEN),
            policy,
            ..super::oci_index("store", "store", IndexKind::Hosted { volatile: true })
        },
    )
}

async fn push_blob(app: &axum::Router, repo: &str, blob: &[u8]) -> StatusCode {
    let digest = oci_digest(blob);
    send_body(
        app,
        Method::POST,
        &format!("/v2/{repo}/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await
    .0
}

async fn delete_blob(app: &axum::Router, repo: &str, digest: &str) -> StatusCode {
    send_body(
        app,
        Method::DELETE,
        &format!("/v2/{repo}/blobs/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await
    .0
}

async fn push_manifest(app: &axum::Router, repo: &str, tag: &str, body: &[u8]) -> StatusCode {
    send_body(
        app,
        Method::PUT,
        &format!("/v2/{repo}/manifests/{tag}"),
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        body.to_vec(),
    )
    .await
    .0
}

#[tokio::test]
async fn test_blob_push_over_the_repository_byte_quota_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(4),
            ..PolicyConfig::default()
        },
    );
    let blob = b"five!";
    let digest = oci_digest(blob);
    let (status, _, body) = send_body(
        &app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body_has_code(&body, "DENIED"), "{body:?}");
    assert_eq!(
        send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        state
            .serving
            .meta
            .quota_usage("store")
            .unwrap()
            .accounted_bytes
            .committed,
        0
    );
}

#[tokio::test]
async fn test_blob_push_within_the_repository_byte_quota_is_accounted() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(64),
            ..PolicyConfig::default()
        },
    );
    let blob = b"a-real-layer-of-bytes";

    assert_eq!(push_blob(&app, "store/app", blob).await, StatusCode::CREATED);
    let usage = state.serving.meta.quota_usage("store").unwrap();
    assert_eq!(
        (usage.accounted_bytes.committed, usage.resources.committed),
        (blob.len() as u64, 1)
    );
}

#[tokio::test]
async fn test_chunked_blob_push_is_accounted_at_commit() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(64),
            ..PolicyConfig::default()
        },
    );
    let blob = b"a-chunked-layer";
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers["location"].to_str().unwrap();
    assert_eq!(
        send_body(
            &app,
            Method::PATCH,
            location,
            &[("authorization", &auth(TOKEN))],
            blob[..4].to_vec(),
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        send_body(
            &app,
            Method::PUT,
            &format!("{location}?digest={}", oci_digest(blob)),
            &[("authorization", &auth(TOKEN))],
            blob[4..].to_vec(),
        )
        .await
        .0,
        StatusCode::CREATED
    );

    let usage = state.serving.meta.quota_usage("store").unwrap();
    assert_eq!(
        (usage.accounted_bytes.committed, usage.accounted_bytes.reserved),
        (blob.len() as u64, 0)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_concurrent_push_of_one_digest_charges_bytes_once() {
    let dir = tempfile::tempdir().unwrap();
    let blob = b"a-shared-layer";
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(blob.len() as u64),
            ..PolicyConfig::default()
        },
    );
    let one = tokio::spawn({
        let app = app.clone();
        async move { push_blob(&app, "store/app", blob).await }
    });
    let two = tokio::spawn(async move { push_blob(&app, "store/app", blob).await });
    let (one, two) = (one.await.unwrap(), two.await.unwrap());

    assert_eq!((one, two), (StatusCode::CREATED, StatusCode::CREATED));
    assert_eq!(
        state
            .serving
            .meta
            .quota_usage("store")
            .unwrap()
            .accounted_bytes
            .committed,
        blob.len() as u64
    );
}

#[tokio::test]
async fn test_repeated_push_of_one_digest_charges_bytes_once() {
    let dir = tempfile::tempdir().unwrap();
    let blob = b"a-shared-layer";
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(blob.len() as u64),
            ..PolicyConfig::default()
        },
    );

    assert_eq!(push_blob(&app, "store/app", blob).await, StatusCode::CREATED);
    assert_eq!(push_blob(&app, "store/app", blob).await, StatusCode::CREATED);
    let usage = state.serving.meta.quota_usage("store").unwrap();
    assert_eq!(
        (usage.accounted_bytes.committed, usage.artifact_bytes.committed),
        (blob.len() as u64, blob.len() as u64)
    );
}

#[tokio::test]
async fn test_deleting_a_blob_releases_its_quota() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(64),
            ..PolicyConfig::default()
        },
    );
    let (first, second) = (b"first-layer".as_slice(), b"second-distinct-layer".as_slice());
    let first_digest = oci_digest(first);
    assert_eq!(push_blob(&app, "store/app", first).await, StatusCode::CREATED);
    assert_eq!(push_blob(&app, "store/app", second).await, StatusCode::CREATED);

    assert_eq!(
        delete_blob(&app, "store/app", &first_digest).await,
        StatusCode::ACCEPTED
    );

    let usage = state.serving.meta.quota_usage("store").unwrap();
    assert_eq!(
        (
            usage.accounted_bytes.committed,
            usage.artifact_bytes.committed,
            usage.resources.committed,
        ),
        (second.len() as u64, second.len() as u64, 1)
    );

    assert_eq!(
        send(&app, Method::GET, &format!("/v2/store/app/blobs/{first_digest}"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete_blob(&app, "store/app", &first_digest).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_deleting_a_shared_digest_frees_bytes_after_the_last_reference() {
    let dir = tempfile::tempdir().unwrap();
    let blob = b"a-shared-layer".as_slice();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(64),
            ..PolicyConfig::default()
        },
    );
    let digest = oci_digest(blob);
    assert_eq!(push_blob(&app, "store/app", blob).await, StatusCode::CREATED);
    assert_eq!(push_blob(&app, "store/api", blob).await, StatusCode::CREATED);

    assert_eq!(delete_blob(&app, "store/app", &digest).await, StatusCode::ACCEPTED);
    let usage = state.serving.meta.quota_usage("store").unwrap();
    assert_eq!(
        (
            usage.accounted_bytes.committed,
            usage.artifact_bytes.committed,
            usage.resources.committed,
        ),
        (blob.len() as u64, blob.len() as u64, 1)
    );

    assert_eq!(delete_blob(&app, "store/api", &digest).await, StatusCode::ACCEPTED);
    assert_eq!(
        state.serving.meta.quota_usage("store").unwrap(),
        peryx_storage::meta::QuotaUsage::default()
    );
}

#[tokio::test]
async fn test_session_upload_over_quota_is_rejected_and_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(4),
            ..PolicyConfig::default()
        },
    );
    let blob = b"over-the-byte-limit";

    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers["location"].to_str().unwrap().to_owned();
    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={}", oci_digest(blob)),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_ne!(status, StatusCode::CREATED);

    let (status, _, _) = send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_session_finish_with_a_wrong_digest_releases_its_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(64),
            ..PolicyConfig::default()
        },
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
    let location = headers["location"].to_str().unwrap().to_owned();
    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        b"actual".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={}", oci_digest(b"different")),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_ne!(status, StatusCode::CREATED);
    assert_eq!(
        state
            .serving
            .meta
            .quota_usage("store")
            .unwrap()
            .accounted_bytes
            .reserved,
        0
    );
}

#[tokio::test]
async fn test_failed_blob_upload_releases_its_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(64),
            ..PolicyConfig::default()
        },
    );
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
    assert!(body_has_code(&body, "DIGEST_INVALID"), "{body:?}");
    let usage = state.serving.meta.quota_usage("store").unwrap();
    assert_eq!(
        (
            usage.accounted_bytes.committed,
            usage.accounted_bytes.reserved,
            usage.artifact_bytes.committed,
            usage.artifact_bytes.reserved,
        ),
        (0, 0, 0, 0)
    );
}

#[tokio::test]
async fn test_audit_mode_records_the_violation_and_accepts_the_push() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(4),
            quota_audit: true,
            ..PolicyConfig::default()
        },
    );
    let blob = b"a-real-layer-of-bytes";
    let digest = oci_digest(blob);

    assert_eq!(push_blob(&app, "store/app", blob).await, StatusCode::CREATED);
    assert_eq!(
        send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        state
            .serving
            .meta
            .quota_usage("store")
            .unwrap()
            .accounted_bytes
            .committed,
        blob.len() as u64
    );
}

#[tokio::test]
async fn test_manifest_over_the_version_quota_stays_absent_from_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = tag_quota_store(&dir, 1);
    let first = br#"{"schemaVersion":2}"#;
    let second = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    assert_eq!(push_manifest(&app, "store/app", "v1", first).await, StatusCode::CREATED);
    assert_eq!(
        push_manifest(&app, "store/app", "v2", second).await,
        StatusCode::FORBIDDEN
    );

    let (_, _, tags) = send(&app, Method::GET, "/v2/store/app/tags/list").await;
    let tags = std::str::from_utf8(&tags).unwrap();
    assert!(tags.contains("\"v1\"") && !tags.contains("\"v2\""), "{tags:?}");
    assert_eq!(
        send(&app, Method::GET, "/v2/store/app/manifests/v2").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            Method::GET,
            &format!("/v2/store/app/manifests/{}", oci_digest(second))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        state
            .serving
            .meta
            .quota_resource_usage("store", "app")
            .unwrap()
            .groups
            .committed,
        1
    );
}

#[tokio::test]
async fn test_manifest_re_push_under_the_same_tag_is_not_double_counted() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = tag_quota_store(&dir, 1);
    let manifest = br#"{"schemaVersion":2}"#;

    assert_eq!(
        push_manifest(&app, "store/app", "v1", manifest).await,
        StatusCode::CREATED
    );
    assert_eq!(
        push_manifest(&app, "store/app", "v1", manifest).await,
        StatusCode::CREATED
    );
    assert_eq!(
        state
            .serving
            .meta
            .quota_resource_usage("store", "app")
            .unwrap()
            .groups
            .committed,
        1
    );
}

#[tokio::test]
async fn test_manifest_re_push_from_trash_is_not_double_counted() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = tag_quota_store(&dir, 1);
    let manifest = br#"{"schemaVersion":2}"#;
    assert_eq!(
        push_manifest(&app, "store/app", "v1", manifest).await,
        StatusCode::CREATED
    );
    assert_eq!(
        send_body(
            &app,
            Method::DELETE,
            "/v2/store/app/manifests/v1",
            &[("authorization", &auth(TOKEN))],
            Vec::new(),
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );

    assert_eq!(
        push_manifest(&app, "store/app", "v1", manifest).await,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn test_a_new_tag_on_an_existing_manifest_counts_a_version() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = tag_quota_store(&dir, 2);
    let manifest = br#"{"schemaVersion":2}"#;

    assert_eq!(
        push_manifest(&app, "store/app", "v1", manifest).await,
        StatusCode::CREATED
    );
    assert_eq!(
        push_manifest(&app, "store/app", "v2", manifest).await,
        StatusCode::CREATED
    );
    assert_eq!(
        state
            .serving
            .meta
            .quota_resource_usage("store", "app")
            .unwrap()
            .groups
            .committed,
        2
    );
}

#[tokio::test]
async fn test_manifest_re_push_by_digest_is_not_re_accounted() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(64),
            ..PolicyConfig::default()
        },
    );
    let manifest = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(manifest);

    assert_eq!(
        push_manifest(&app, "store/app", &digest, manifest).await,
        StatusCode::CREATED
    );
    assert_eq!(
        push_manifest(&app, "store/app", &digest, manifest).await,
        StatusCode::CREATED
    );
    assert_eq!(
        state
            .serving
            .meta
            .quota_usage("store")
            .unwrap()
            .accounted_bytes
            .committed,
        manifest.len() as u64
    );
}

async fn mount(app: &axum::Router, digest: &str, source: &str) -> StatusCode {
    send_body(
        app,
        Method::POST,
        &format!("/v2/store/target/blobs/uploads/?mount={digest}&from={source}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await
    .0
}

#[tokio::test]
async fn test_cross_repo_mount_is_accounted_as_a_new_resource() {
    let dir = tempfile::tempdir().unwrap();
    let blob = b"a-real-layer-of-bytes";
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_resources: Some(2),
            ..PolicyConfig::default()
        },
    );
    let digest = oci_digest(blob);
    assert_eq!(push_blob(&app, "store/source", blob).await, StatusCode::CREATED);

    assert_eq!(mount(&app, &digest, "store/source").await, StatusCode::CREATED);
    assert_eq!(
        send(&app, Method::GET, &format!("/v2/store/target/blobs/{digest}"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(state.serving.meta.quota_usage("store").unwrap().resources.committed, 2);
}

#[tokio::test]
async fn test_re_mount_of_a_present_blob_is_not_re_accounted() {
    let dir = tempfile::tempdir().unwrap();
    let blob = b"a-real-layer-of-bytes";
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_resources: Some(2),
            ..PolicyConfig::default()
        },
    );
    let digest = oci_digest(blob);
    assert_eq!(push_blob(&app, "store/source", blob).await, StatusCode::CREATED);
    assert_eq!(mount(&app, &digest, "store/source").await, StatusCode::CREATED);

    assert_eq!(mount(&app, &digest, "store/source").await, StatusCode::CREATED);
    assert_eq!(state.serving.meta.quota_usage("store").unwrap().resources.committed, 2);
}

#[tokio::test]
async fn test_cross_repo_mount_over_the_resource_quota_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let blob = b"a-real-layer-of-bytes";
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_resources: Some(1),
            ..PolicyConfig::default()
        },
    );
    let digest = oci_digest(blob);
    assert_eq!(push_blob(&app, "store/source", blob).await, StatusCode::CREATED);

    assert_eq!(mount(&app, &digest, "store/source").await, StatusCode::FORBIDDEN);
    assert_eq!(
        send(&app, Method::GET, &format!("/v2/store/target/blobs/{digest}"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(state.serving.meta.quota_usage("store").unwrap().resources.committed, 1);
}

#[tokio::test]
async fn test_quota_decisions_increment_the_admitted_and_rejected_counters() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(4),
            ..PolicyConfig::default()
        },
    );
    assert_eq!(push_blob(&app, "store/app", b"ok").await, StatusCode::CREATED);
    assert_eq!(push_blob(&app, "store/app", b"too-large").await, StatusCode::FORBIDDEN);

    let want = std::collections::BTreeMap::from([("quota_admitted", 1), ("quota_rejected", 1)]);
    state.serving.metrics.flush().unwrap();
    let counters = state.serving.metrics.index_totals();
    assert!(
        counters
            .get("store")
            .is_some_and(|store| want.iter().all(|(key, value)| store.ecosystem.get(key) == Some(value))),
        "quota metrics settled on an unexpected state: {:?}",
        counters.get("store")
    );
}

#[tokio::test]
async fn test_a_push_to_an_unmetered_index_records_no_quota_usage() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_artifact_size_bytes: Some(1024),
            ..PolicyConfig::default()
        },
    );

    assert_eq!(
        push_blob(&app, "store/app", b"a-real-layer-of-bytes").await,
        StatusCode::CREATED
    );
    let usage = state.serving.meta.quota_usage("store").unwrap();
    assert_eq!((usage.accounted_bytes.committed, usage.resources.committed), (0, 0));
}

#[tokio::test]
async fn test_audit_without_limits_records_no_quota_usage() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            quota_audit: true,
            ..PolicyConfig::default()
        },
    );

    assert_eq!(push_blob(&app, "store/app", b"a-real-layer").await, StatusCode::CREATED);
    let usage = state.serving.meta.quota_usage("store").unwrap();
    assert_eq!((usage.accounted_bytes.committed, usage.resources.committed), (0, 0));
}

#[tokio::test]
async fn test_metered_upload_under_a_superseded_epoch_releases_its_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store_distributed(
        &dir,
        &PolicyConfig {
            max_accounted_bytes: Some(64),
            ..PolicyConfig::default()
        },
    );
    bind_ownership(&state, super::EpochAuthority::superseded(5, 6));
    let blob = b"a-metered-layer-that-loses-the-race";
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
    let usage = state.serving.meta.quota_usage("store").unwrap();
    assert_eq!(
        (usage.accounted_bytes.reserved, usage.accounted_bytes.committed),
        (0, 0)
    );
    assert_eq!(
        send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}
