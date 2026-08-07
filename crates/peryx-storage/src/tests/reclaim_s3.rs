//! Provider-contract tests for reclaiming a blob over the object-store backend.
//!
//! The reclamation executor is generic over [`BlobBackend`](crate::blob::BlobBackend), so it drives
//! the S3 backend through the same path as the filesystem one, mapping a `DeleteObject` to a removal
//! and a `404` to a proved absence. These fixtures point a real [`S3Backend`](crate::blob::S3Backend)
//! at a wiremock endpoint, run the executor in a child process the way the S3 unit suite does so the
//! AWS SDK's process-global credential resolution stays isolated, and assert the outcome for a
//! successful delete, a proved absence, and a provider error that must leave the candidate retryable.

use std::time::Duration;

use peryx_identity::ArtifactDigest;
use rstest::rstest;
use tokio::process::Command;
use wiremock::matchers::{method, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::blob::{Digest, S3Backend, S3Config, S3Settings};
use crate::meta::{AccountingClass, MetaStore, NewQuotaReservation, ObservedFrontier, QuotaLimits, ReclamationState};
use crate::reclaim::{ReclaimError, ReclaimOutcome, ReclaimRequest, reclaim_ready_blob};

const CHILD: &str = "PERYX_RECLAIM_S3_CHILD";
const ENDPOINT: &str = "PERYX_RECLAIM_S3_ENDPOINT";
const BUCKET: &str = "peryx-tests";
const PAYLOAD: &[u8] = b"payload";
const JOB: &str = "reclaim-sweep";
const HOLDER: &str = "node-a";
const EPOCH: u64 = 5;
const FRONTIER: u64 = 5;

fn settings(endpoint: String) -> S3Settings {
    S3Settings {
        endpoint,
        bucket: BUCKET.to_owned(),
        prefix: "cache".to_owned(),
        region: "us-east-1".to_owned(),
        path_style: true,
        request_timeout: Duration::from_secs(5),
        max_retries: 0,
        multipart_threshold: 5 << 20,
        part_size: 5 << 20,
        upload_concurrency: 2,
        conditional_writes: true,
        checksum_writes: true,
    }
}

async fn mount(server: &MockServer, scenario: &str) {
    Mock::given(method("GET"))
        .and(query_param("location", ""))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<LocationConstraint xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"></LocationConstraint>",
            "application/xml",
        ))
        .with_priority(2)
        .mount(server)
        .await;
    let head = match scenario {
        "absence" => ResponseTemplate::new(404),
        "head_error" => ResponseTemplate::new(500),
        _ => ResponseTemplate::new(200).insert_header("Content-Length", "7"),
    };
    Mock::given(method("HEAD")).respond_with(head).mount(server).await;
    let delete = if scenario == "delete_error" {
        ResponseTemplate::new(500)
    } else {
        ResponseTemplate::new(204)
    };
    Mock::given(method("DELETE")).respond_with(delete).mount(server).await;
}

#[rstest]
#[case::success("success")]
#[case::absence("absence")]
#[case::delete_error("delete_error")]
#[case::head_error("head_error")]
#[tokio::test]
async fn test_reclaim_over_s3(#[case] scenario: &str) {
    let server = MockServer::start().await;
    mount(&server, scenario).await;
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("tests::reclaim_s3::reclaim_s3_child")
            .arg("--nocapture")
            .env(CHILD, scenario)
            .env(ENDPOINT, server.uri())
            .env("AWS_ACCESS_KEY_ID", "id")
            .env("AWS_SECRET_ACCESS_KEY", "secret")
            .env("AWS_REGION", "us-east-1")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .env_remove("AWS_PROFILE")
            .env_remove("AWS_SHARED_CREDENTIALS_FILE")
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn reclaim_s3_child() {
    let Ok(scenario) = std::env::var(CHILD) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let config = S3Config::new(settings(std::env::var(ENDPOINT).unwrap())).unwrap();
    let backend = S3Backend::new(config, dir.path().join("staging"));
    let artifact = ArtifactDigest::from_sha256(Digest::of(PAYLOAD).as_str()).unwrap();

    meta.claim_job_lease(JOB, HOLDER, EPOCH, 0, 30).unwrap();
    meta.select_reclamation_candidate(&artifact, false, FRONTIER, EPOCH, 0)
        .unwrap();
    meta.mark_reclamation_ready(
        &artifact,
        false,
        ObservedFrontier {
            replica: Some(FRONTIER),
            backup: Some(FRONTIER),
        },
        EPOCH,
        1,
    )
    .unwrap();
    let reservation = meta
        .reserve_quota(
            NewQuotaReservation {
                repository: "private",
                project: None,
                version: None,
                digest: &artifact.canonical(),
                bytes: 7,
                class: AccountingClass::Generated,
                created_at_unix: 0,
            },
            QuotaLimits::default(),
        )
        .unwrap();
    meta.commit_quota_reservation(reservation.id).unwrap();

    let request = ReclaimRequest {
        digest: &artifact,
        job: JOB,
        holder: HOLDER,
        epoch: EPOCH,
        reservations: &[reservation.id],
    };
    let result = reclaim_ready_blob(&meta, &backend, request).await;

    match scenario.as_str() {
        "success" => {
            assert_eq!(
                result.unwrap(),
                ReclaimOutcome::Reclaimed {
                    deleted: true,
                    credited: 1
                }
            );
            assert!(meta.reclamation_tombstone(&artifact).unwrap().is_none());
            assert!(meta.quota_reservation(reservation.id).unwrap().is_none());
        }
        "absence" => {
            assert_eq!(
                result.unwrap(),
                ReclaimOutcome::Reclaimed {
                    deleted: false,
                    credited: 1
                }
            );
            assert!(meta.reclamation_tombstone(&artifact).unwrap().is_none());
        }
        "delete_error" | "head_error" => {
            assert!(matches!(result, Err(ReclaimError::Blob(_))));
            assert_eq!(
                meta.reclamation_tombstone(&artifact).unwrap().unwrap().state,
                ReclamationState::Ready
            );
            assert!(meta.quota_reservation(reservation.id).unwrap().is_some());
        }
        other => panic!("unknown scenario {other}"),
    }
}
