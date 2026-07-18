//! Hosted-push quota enforcement: byte, project, and version reservations bracket the blob-upload,
//! cross-repo-mount, and manifest-publication boundaries, with audit mode recording rather than
//! denying.

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use peryx_storage::meta::{AccountingClass, NewQuotaReservation};

use super::{app_with, auth, body_has_code, oci_digest, send, send_body};
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
                    project: Some("team/api"),
                    version: tag,
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

/// A hosted store at route `store` whose one upload token is `s3cret`, under the quota `limits`.
fn quota_store(dir: &tempfile::TempDir, limits: &PolicyConfig) -> (Arc<peryx_driver::AppState>, axum::Router) {
    let index = Index {
        acl: IndexAcl::upload_token(TOKEN),
        policy: Policy::compile(limits, str::to_owned),
        ..super::oci_index("store", "store", IndexKind::Hosted { volatile: true })
    };
    app_with(dir, index)
}

/// Push a blob monolithically under the upload token, returning the response status.
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

/// Push a manifest by tag under the upload token, returning the response status.
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

    // The push is refused through the distribution-spec error contract, the bytes never become a
    // member of the repository, and no capacity is charged.
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body_has_code(&body, "DENIED"), "{body:?}");
    assert_eq!(
        send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(state.meta.quota_usage("store").unwrap().accounted_bytes.committed, 0);
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
    let usage = state.meta.quota_usage("store").unwrap();
    assert_eq!(
        (usage.accounted_bytes.committed, usage.projects.committed),
        (blob.len() as u64, 1)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_concurrent_push_of_one_digest_charges_bytes_once() {
    let dir = tempfile::tempdir().unwrap();
    let blob = b"a-shared-layer";
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            // Exactly one copy of the blob fits; deduplication must let both racing pushes in.
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
        state.meta.quota_usage("store").unwrap().accounted_bytes.committed,
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
    // A digest the repository already serves is not reserved a second time, so a re-push neither
    // fails against the exact-fit limit nor inflates the logical byte counter.
    assert_eq!(push_blob(&app, "store/app", blob).await, StatusCode::CREATED);
    let usage = state.meta.quota_usage("store").unwrap();
    assert_eq!(
        (usage.accounted_bytes.committed, usage.file_bytes.committed),
        (blob.len() as u64, blob.len() as u64)
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

    // The commit fails on the digest mismatch, so the reservation it took is released and the
    // repository is charged nothing.
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "DIGEST_INVALID"), "{body:?}");
    let usage = state.meta.quota_usage("store").unwrap();
    assert_eq!(
        (
            usage.accounted_bytes.committed,
            usage.accounted_bytes.reserved,
            usage.file_bytes.committed,
            usage.file_bytes.reserved,
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

    // The same content that enforce mode denies is admitted, served, and counted, so an operator can
    // observe projected enforcement against live traffic.
    assert_eq!(push_blob(&app, "store/app", blob).await, StatusCode::CREATED);
    assert_eq!(
        send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        state.meta.quota_usage("store").unwrap().accounted_bytes.committed,
        blob.len() as u64
    );
}

#[tokio::test]
async fn test_manifest_over_the_version_quota_stays_absent_from_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_versions_per_project: Some(1),
            ..PolicyConfig::default()
        },
    );
    let first = br#"{"schemaVersion":2}"#;
    let second = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    assert_eq!(push_manifest(&app, "store/app", "v1", first).await, StatusCode::CREATED);
    assert_eq!(
        push_manifest(&app, "store/app", "v2", second).await,
        StatusCode::FORBIDDEN
    );

    // The rejected tag reaches neither the tag listing nor by-digest discovery.
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
            .meta
            .quota_project_usage("store", "app")
            .unwrap()
            .versions
            .committed,
        1
    );
}

#[tokio::test]
async fn test_manifest_re_push_under_the_same_tag_is_not_double_counted() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_versions_per_project: Some(1),
            ..PolicyConfig::default()
        },
    );
    let manifest = br#"{"schemaVersion":2}"#;

    // Pushing the identical image under the same tag twice is idempotent: the second push accounts no
    // fresh version and so does not exhaust the single-version quota.
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
            .meta
            .quota_project_usage("store", "app")
            .unwrap()
            .versions
            .committed,
        1
    );
}

#[tokio::test]
async fn test_a_new_tag_on_an_existing_manifest_counts_a_version() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_versions_per_project: Some(2),
            ..PolicyConfig::default()
        },
    );
    let manifest = br#"{"schemaVersion":2}"#;

    // The same digest under a second tag is a new version even though its bytes are already stored.
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
            .meta
            .quota_project_usage("store", "app")
            .unwrap()
            .versions
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
        state.meta.quota_usage("store").unwrap().accounted_bytes.committed,
        manifest.len() as u64
    );
}

/// `POST ?mount=<digest>&from=<source>` — publish a stored blob into `store/target` without a
/// transfer, returning the response status.
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
async fn test_cross_repo_mount_is_accounted_as_a_new_project() {
    let dir = tempfile::tempdir().unwrap();
    let blob = b"a-real-layer-of-bytes";
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_projects: Some(2),
            ..PolicyConfig::default()
        },
    );
    let digest = oci_digest(blob);
    assert_eq!(push_blob(&app, "store/source", blob).await, StatusCode::CREATED);

    // The mount publishes the blob into a second repository and counts that repository as a project,
    // even though the deduplicated bytes were already accounted.
    assert_eq!(mount(&app, &digest, "store/source").await, StatusCode::CREATED);
    assert_eq!(
        send(&app, Method::GET, &format!("/v2/store/target/blobs/{digest}"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(state.meta.quota_usage("store").unwrap().projects.committed, 2);
}

#[tokio::test]
async fn test_re_mount_of_a_present_blob_is_not_re_accounted() {
    let dir = tempfile::tempdir().unwrap();
    let blob = b"a-real-layer-of-bytes";
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_projects: Some(2),
            ..PolicyConfig::default()
        },
    );
    let digest = oci_digest(blob);
    assert_eq!(push_blob(&app, "store/source", blob).await, StatusCode::CREATED);
    assert_eq!(mount(&app, &digest, "store/source").await, StatusCode::CREATED);

    // A digest the target repository already serves is not reserved again, so re-mounting it neither
    // fails against the exhausted project quota nor counts a further project.
    assert_eq!(mount(&app, &digest, "store/source").await, StatusCode::CREATED);
    assert_eq!(state.meta.quota_usage("store").unwrap().projects.committed, 2);
}

#[tokio::test]
async fn test_cross_repo_mount_over_the_project_quota_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let blob = b"a-real-layer-of-bytes";
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_projects: Some(1),
            ..PolicyConfig::default()
        },
    );
    let digest = oci_digest(blob);
    assert_eq!(push_blob(&app, "store/source", blob).await, StatusCode::CREATED);

    // The one project slot is spent on `source`, so mounting into a second repository is refused and
    // that repository never comes to serve the blob.
    assert_eq!(mount(&app, &digest, "store/source").await, StatusCode::FORBIDDEN);
    assert_eq!(
        send(&app, Method::GET, &format!("/v2/store/target/blobs/{digest}"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(state.meta.quota_usage("store").unwrap().projects.committed, 1);
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
    for _ in 0..500 {
        let counters = state.metrics.index_totals();
        if counters
            .get("store")
            .is_some_and(|store| want.iter().all(|(key, value)| store.ecosystem.get(key) == Some(value)))
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!(
        "quota metrics never settled: {:?}",
        state.metrics.index_totals().get("store")
    );
}

#[tokio::test]
async fn test_a_push_to_an_unmetered_index_records_no_quota_usage() {
    let dir = tempfile::tempdir().unwrap();
    // Only a per-file size limit is set, which the byte stream enforces on its own, so no repository
    // accounting runs.
    let (state, app) = quota_store(
        &dir,
        &PolicyConfig {
            max_file_size_bytes: Some(1024),
            ..PolicyConfig::default()
        },
    );

    assert_eq!(
        push_blob(&app, "store/app", b"a-real-layer-of-bytes").await,
        StatusCode::CREATED
    );
    let usage = state.meta.quota_usage("store").unwrap();
    assert_eq!((usage.accounted_bytes.committed, usage.projects.committed), (0, 0));
}
