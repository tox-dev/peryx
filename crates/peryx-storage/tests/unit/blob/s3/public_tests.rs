use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;

use crate::blob::{BlobErrorKind, BlobScanError, BlobStaged, BlobStorage, Digest, S3Config, S3Settings};
use rstest::rstest;
use tokio::process::Command;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BUCKET: &str = "peryx-tests";
const CHILD: &str = "PERYX_S3_UNIT_CHILD";
const ENDPOINT: &str = "PERYX_S3_UNIT_ENDPOINT";
const STAGING: &str = "PERYX_S3_UNIT_STAGING";

#[derive(Clone, Copy)]
enum Scenario {
    Health,
    Put,
    Head,
    WholeRead,
    Range,
    EmptyRange,
    InvalidRange,
    RangeMissing,
    RangeError,
    Verify,
    VerifyMissing,
    VerifyError,
    Materialize,
    MaterializeMissing,
    MaterializeError,
    Present,
    Delete,
    DeleteHeadError,
    DeleteError,
    Multipart,
    AbortMissing,
    PutMissingStage,
    MultipartMissingStage,
    BeginError,
    WriteFlush,
    WriteTail,
    WriteCommit,
    WriteAbort,
    StagedLen,
    StagedEmpty,
    StagedMaterialized,
    StagedAbort,
    BlockingStage,
    BlockingHead,
    BlockingRead,
    BlockingMaterialize,
    BlockingVerify,
    BlockingDelete,
    BlockingVisit,
}

impl Scenario {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Health => "health",
            Self::Put => "put",
            Self::Head => "head",
            Self::WholeRead => "whole_read",
            Self::Range => "range",
            Self::EmptyRange => "empty_range",
            Self::InvalidRange => "invalid_range",
            Self::RangeMissing => "range_missing",
            Self::RangeError => "range_error",
            Self::Verify => "verify",
            Self::VerifyMissing => "verify_missing",
            Self::VerifyError => "verify_error",
            Self::Materialize => "materialize",
            Self::MaterializeMissing => "materialize_missing",
            Self::MaterializeError => "materialize_error",
            Self::Present => "present",
            Self::Delete => "delete",
            Self::DeleteHeadError => "delete_head_error",
            Self::DeleteError => "delete_error",
            Self::Multipart => "multipart",
            Self::AbortMissing => "abort_missing",
            Self::PutMissingStage => "put_missing_stage",
            Self::MultipartMissingStage => "multipart_missing_stage",
            Self::BeginError => "begin_error",
            Self::WriteFlush => "write_flush",
            Self::WriteTail => "write_tail",
            Self::WriteCommit => "write_commit",
            Self::WriteAbort => "write_abort",
            Self::StagedLen => "staged_len",
            Self::StagedEmpty => "staged_empty",
            Self::StagedMaterialized => "staged_materialized",
            Self::StagedAbort => "staged_abort",
            Self::BlockingStage => "blocking_stage",
            Self::BlockingHead => "blocking_head",
            Self::BlockingRead => "blocking_read",
            Self::BlockingMaterialize => "blocking_materialize",
            Self::BlockingVerify => "blocking_verify",
            Self::BlockingDelete => "blocking_delete",
            Self::BlockingVisit => "blocking_visit",
        }
    }
}

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

fn object_path(bytes: &[u8]) -> String {
    format!("/{BUCKET}/cache/sha256/{}", Digest::of(bytes).as_str())
}

fn service_error(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(format!("<Error><Code>{code}</Code></Error>"), "application/xml")
}

async fn mount(server: &MockServer, scenario: Scenario) {
    Mock::given(method("GET"))
        .and(query_param("location", ""))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<LocationConstraint xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"></LocationConstraint>",
            "application/xml",
        ))
        .with_priority(2)
        .mount(server)
        .await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(object_path(b"package")))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "7"))
        .with_priority(2)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(header("Range", "bytes=1-4"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 1-4/7")
                .set_body_bytes(b"acka"),
        )
        .with_priority(2)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"package"))
        .mount(server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).insert_header("ETag", "object"))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<InitiateMultipartUploadResult><UploadId>upload-1</UploadId></InitiateMultipartUploadResult>",
            "application/xml",
        ))
        .mount(server)
        .await;
    Mock::given(method("PUT"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "part")
                .insert_header("x-amz-checksum-sha256", "checksum"),
        )
        .with_priority(2)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<CompleteMultipartUploadResult><ETag>etag</ETag></CompleteMultipartUploadResult>",
            "application/xml",
        ))
        .mount(server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;
    mount_scenario(server, scenario).await;
}

async fn mount_scenario(server: &MockServer, scenario: Scenario) {
    match scenario {
        Scenario::RangeMissing => {
            Mock::given(method("HEAD"))
                .respond_with(service_error(404, "NoSuchKey"))
                .with_priority(1)
                .mount(server)
                .await;
        }
        Scenario::RangeError | Scenario::DeleteHeadError => {
            Mock::given(method("HEAD"))
                .respond_with(service_error(500, "InternalError"))
                .with_priority(1)
                .mount(server)
                .await;
        }
        Scenario::VerifyMissing | Scenario::MaterializeMissing => {
            Mock::given(method("GET"))
                .respond_with(service_error(404, "NoSuchKey"))
                .with_priority(1)
                .mount(server)
                .await;
        }
        Scenario::VerifyError | Scenario::MaterializeError => {
            Mock::given(method("GET"))
                .respond_with(service_error(500, "InternalError"))
                .with_priority(1)
                .mount(server)
                .await;
        }
        Scenario::DeleteError => {
            Mock::given(method("DELETE"))
                .respond_with(service_error(500, "InternalError"))
                .with_priority(1)
                .mount(server)
                .await;
        }
        Scenario::AbortMissing => {
            Mock::given(method("POST"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(service_error(500, "InternalError"))
                .with_priority(1)
                .mount(server)
                .await;
            Mock::given(method("DELETE"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(service_error(404, "NoSuchUpload"))
                .with_priority(1)
                .mount(server)
                .await;
        }
        _ => {}
    }
}

#[rstest]
#[case::health(Scenario::Health)]
#[case::put(Scenario::Put)]
#[case::head(Scenario::Head)]
#[case::whole_read(Scenario::WholeRead)]
#[case::range(Scenario::Range)]
#[case::empty_range(Scenario::EmptyRange)]
#[case::invalid_range(Scenario::InvalidRange)]
#[case::range_missing(Scenario::RangeMissing)]
#[case::range_error(Scenario::RangeError)]
#[case::verify(Scenario::Verify)]
#[case::verify_missing(Scenario::VerifyMissing)]
#[case::verify_error(Scenario::VerifyError)]
#[case::materialize(Scenario::Materialize)]
#[case::materialize_missing(Scenario::MaterializeMissing)]
#[case::materialize_error(Scenario::MaterializeError)]
#[case::present(Scenario::Present)]
#[case::delete(Scenario::Delete)]
#[case::delete_head_error(Scenario::DeleteHeadError)]
#[case::delete_error(Scenario::DeleteError)]
#[case::multipart(Scenario::Multipart)]
#[case::abort_missing(Scenario::AbortMissing)]
#[case::put_missing_stage(Scenario::PutMissingStage)]
#[case::multipart_missing_stage(Scenario::MultipartMissingStage)]
#[case::begin_error(Scenario::BeginError)]
#[case::write_flush(Scenario::WriteFlush)]
#[case::write_tail(Scenario::WriteTail)]
#[case::write_commit(Scenario::WriteCommit)]
#[case::write_abort(Scenario::WriteAbort)]
#[case::staged_len(Scenario::StagedLen)]
#[case::staged_empty(Scenario::StagedEmpty)]
#[case::staged_materialized(Scenario::StagedMaterialized)]
#[case::staged_abort(Scenario::StagedAbort)]
#[case::blocking_stage(Scenario::BlockingStage)]
#[case::blocking_head(Scenario::BlockingHead)]
#[case::blocking_read(Scenario::BlockingRead)]
#[case::blocking_materialize(Scenario::BlockingMaterialize)]
#[case::blocking_verify(Scenario::BlockingVerify)]
#[case::blocking_delete(Scenario::BlockingDelete)]
#[case::blocking_visit(Scenario::BlockingVisit)]
#[tokio::test]
async fn test_s3_public_surface_in_the_unit_binary(#[case] scenario: Scenario) {
    let server = MockServer::start().await;
    mount(&server, scenario).await;
    let staging = tempfile::tempdir().unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("blob::s3::public_tests::test_s3_public_child")
            .arg("--nocapture")
            .env(CHILD, scenario.as_str())
            .env(ENDPOINT, server.uri())
            .env(STAGING, staging.path())
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
async fn test_s3_public_child() {
    let Ok(scenario) = std::env::var(CHILD) else {
        return;
    };
    let staging = PathBuf::from(std::env::var_os(STAGING).unwrap());
    let staging = if scenario == "begin_error" {
        let blocked = staging.join("blocked");
        std::fs::write(&blocked, []).unwrap();
        blocked
    } else {
        staging
    };
    let storage = BlobStorage::s3(
        S3Config::new(settings(std::env::var(ENDPOINT).unwrap())).unwrap(),
        staging,
    );
    run_child_scenario(&storage, &scenario).await;
}

async fn run_child_scenario(storage: &BlobStorage, scenario: &str) {
    match scenario {
        "health" | "put" | "head" | "whole_read" | "range" | "empty_range" | "verify" | "materialize" | "present"
        | "delete" | "multipart" => run_success_scenario(storage, scenario).await,
        "invalid_range"
        | "range_missing"
        | "range_error"
        | "verify_missing"
        | "verify_error"
        | "materialize_missing"
        | "materialize_error"
        | "delete_head_error"
        | "delete_error"
        | "abort_missing"
        | "put_missing_stage"
        | "multipart_missing_stage"
        | "begin_error" => run_error_scenario(storage, scenario).await,
        "write_flush"
        | "write_tail"
        | "write_commit"
        | "write_abort"
        | "staged_len"
        | "staged_empty"
        | "staged_materialized"
        | "staged_abort" => run_write_scenario(storage, scenario).await,
        "blocking_stage"
        | "blocking_head"
        | "blocking_read"
        | "blocking_materialize"
        | "blocking_verify"
        | "blocking_delete"
        | "blocking_visit" => run_blocking_scenario(storage, scenario),
        _ => unreachable!(),
    }
}

async fn run_success_scenario(storage: &BlobStorage, scenario: &str) {
    let digest = Digest::of(b"package");
    match scenario {
        "health" => storage.health().await.unwrap(),
        "put" => assert_eq!(storage.put_bytes(b"package").await.unwrap(), digest),
        "head" => assert_eq!(storage.head(&digest).await.unwrap().unwrap().bytes, 7),
        "whole_read" => assert_eq!(storage.read_bytes(&digest, 7).await.unwrap(), b"package"),
        "range" => assert_eq!(
            storage
                .open(&digest, Some(1..5))
                .await
                .unwrap()
                .collect(4)
                .await
                .unwrap(),
            b"acka"
        ),
        "empty_range" => assert!(
            storage
                .open(&digest, Some(3..3))
                .await
                .unwrap()
                .collect(0)
                .await
                .unwrap()
                .is_empty()
        ),
        "verify" => assert!(storage.verify(&digest).await.unwrap()),
        "materialize" => assert_eq!(
            std::fs::read(storage.materialize(&digest).await.unwrap().path()).unwrap(),
            b"package"
        ),
        "present" => assert!(storage.present(vec![digest.clone()]).await.unwrap().contains(&digest)),
        "delete" => assert!(storage.delete(&digest).await.unwrap()),
        "multipart" => {
            storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap();
        }
        _ => unreachable!(),
    }
}

async fn run_error_scenario(storage: &BlobStorage, scenario: &str) {
    let digest = Digest::of(b"package");
    match scenario {
        "invalid_range" => assert_eq!(
            storage.open(&digest, Some(8..9)).await.err().unwrap().kind(),
            BlobErrorKind::InvalidRange
        ),
        "range_missing" => assert_eq!(
            storage.open(&digest, Some(1..5)).await.err().unwrap().kind(),
            BlobErrorKind::NotFound
        ),
        "range_error" => assert_eq!(
            storage.open(&digest, Some(1..5)).await.err().unwrap().kind(),
            BlobErrorKind::Io
        ),
        "verify_missing" => assert_eq!(
            storage.verify(&digest).await.unwrap_err().kind(),
            BlobErrorKind::NotFound
        ),
        "verify_error" => assert_eq!(storage.verify(&digest).await.unwrap_err().kind(), BlobErrorKind::Io),
        "materialize_missing" => assert_eq!(
            storage.materialize(&digest).await.unwrap_err().kind(),
            BlobErrorKind::NotFound
        ),
        "materialize_error" => assert_eq!(
            storage.materialize(&digest).await.unwrap_err().kind(),
            BlobErrorKind::Io
        ),
        "delete_head_error" | "delete_error" => {
            assert_eq!(storage.delete(&digest).await.unwrap_err().kind(), BlobErrorKind::Io);
        }
        "abort_missing" => assert_eq!(
            storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap_err().kind(),
            BlobErrorKind::Io
        ),
        "put_missing_stage" => {
            let staged = stage(storage, b"package").await;
            staged.with_materialized(|path| std::fs::remove_file(path).unwrap());
            assert_eq!(staged.commit().await.unwrap_err().kind(), BlobErrorKind::Io);
        }
        "multipart_missing_stage" => {
            let staged = stage(storage, &vec![7; (5 << 20) + 1]).await;
            staged.with_materialized(|path| std::fs::remove_file(path).unwrap());
            assert_eq!(staged.commit().await.unwrap_err().kind(), BlobErrorKind::Io);
        }
        "begin_error" => assert_eq!(storage.begin().await.err().unwrap().kind(), BlobErrorKind::Io),
        _ => unreachable!(),
    }
}

async fn stage(storage: &BlobStorage, bytes: &[u8]) -> BlobStaged {
    let mut write = storage.begin().await.unwrap();
    write.write_chunk(Bytes::copy_from_slice(bytes)).await.unwrap();
    write.finish().await.unwrap()
}

async fn run_write_scenario(storage: &BlobStorage, scenario: &str) {
    match scenario {
        "write_flush" => {
            let mut write = storage.begin().await.unwrap();
            write.write_chunk(Bytes::from_static(b"package")).await.unwrap();
            assert_eq!(write.flush().await.unwrap(), 7);
        }
        "write_tail" => {
            let write = storage.begin().await.unwrap();
            assert!(write.tail().unwrap().open().is_ok());
        }
        "write_commit" => {
            let mut write = storage.begin().await.unwrap();
            write.write_chunk(Bytes::from_static(b"package")).await.unwrap();
            write.commit(&Digest::of(b"package")).await.unwrap();
        }
        "write_abort" => storage.begin().await.unwrap().abort().await.unwrap(),
        "staged_len" => assert_eq!(stage(storage, b"package").await.len(), 7),
        "staged_empty" => assert!(stage(storage, b"").await.is_empty()),
        "staged_materialized" => assert_eq!(
            stage(storage, b"package")
                .await
                .with_materialized(|path| std::fs::read(path))
                .unwrap(),
            b"package"
        ),
        "staged_abort" => stage(storage, b"package").await.abort().await.unwrap(),
        _ => unreachable!(),
    }
}

fn run_blocking_scenario(storage: &BlobStorage, scenario: &str) {
    let digest = Digest::of(b"package");
    let storage = storage.blocking();
    let error = match scenario {
        "blocking_stage" => storage.stage_reader(&mut std::io::Cursor::new(b"package")).unwrap_err(),
        "blocking_head" => storage.head(&digest).unwrap_err(),
        "blocking_read" => storage.read_bytes(&digest, 7).unwrap_err(),
        "blocking_materialize" => storage.materialize(&digest).unwrap_err(),
        "blocking_verify" => storage.verify(&digest).unwrap_err(),
        "blocking_delete" => storage.delete(&digest).unwrap_err(),
        "blocking_visit" => match storage.visit::<std::convert::Infallible>(|_| Ok(())) {
            Err(BlobScanError::Store(error)) => error,
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    assert_eq!(error.kind(), BlobErrorKind::Unsupported);
}
