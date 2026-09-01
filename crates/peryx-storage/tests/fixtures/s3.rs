#[cfg(feature = "container-tests")]
use std::error::Error as _;
use std::ffi::OsString;
#[cfg(feature = "container-tests")]
use std::io::{Read as _, Write as _, stdin};
use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use peryx_storage::blob::{
    BlobErrorKind, BlobOperation, BlobScanError, BlobStaged, BlobStorage, Digest, PlacementReceipt, S3Config,
    S3Settings, WriteEvidence,
};
#[cfg(feature = "container-tests")]
use tracing::{Event, Subscriber};
#[cfg(feature = "container-tests")]
use tracing_subscriber::Layer;
#[cfg(feature = "container-tests")]
use tracing_subscriber::layer::Context;
#[cfg(feature = "container-tests")]
use tracing_subscriber::prelude::*;

const BUCKET: &str = "peryx-tests";
#[cfg(feature = "container-tests")]
const STREAM_BYTES: usize = 8 << 20;
#[cfg(feature = "container-tests")]
const JOURNAL_WRITTEN: &str = "PERYX_JOURNAL_WRITTEN";
#[cfg(feature = "container-tests")]
const STREAM_OPENED: &str = "PERYX_STREAM_OPENED";

#[tokio::main]
async fn main() -> Result<(), String> {
    let mut arguments = std::env::args_os().skip(1);
    match argument(&mut arguments, "command")?.as_str() {
        "unit" => run_unit(arguments.collect()).await,
        "filesystem" => run_filesystem().await,
        "integration" => {
            run_integration(
                argument(&mut arguments, "scenario")?,
                argument(&mut arguments, "endpoint")?,
                PathBuf::from(argument(&mut arguments, "staging directory")?),
            )
            .await
        }
        command => Err(format!("unknown S3 fixture command: {command}")),
    }
}

async fn run_filesystem() -> Result<(), String> {
    let dir = tempfile::tempdir().unwrap();
    let storage = BlobStorage::filesystem(dir.path());
    storage.health().await.unwrap();
    assert_eq!(storage.recover_incomplete_uploads().await.unwrap(), 0);
    let digest = storage.put_bytes(b"package").await.unwrap();
    storage.head(&digest).await.unwrap();
    storage.open(&digest, None).await.unwrap();
    storage.present(vec![digest.clone()]).await.unwrap();
    storage.verify(&digest).await.unwrap();
    storage.materialize(&digest).await.unwrap();
    storage.delete(&digest).await.unwrap();
    let mut write = storage.begin().await.unwrap();
    write.write_chunk(Bytes::from_static(b"package")).await.unwrap();
    write.commit(&digest).await.unwrap();
    Ok(())
}

fn argument(arguments: &mut impl Iterator<Item = OsString>, name: &str) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("missing {name}"))?
        .into_string()
        .map_err(|_| format!("{name} is not valid UTF-8"))
}

async fn run_unit(arguments: Vec<OsString>) -> Result<(), String> {
    if !arguments.len().is_multiple_of(3) {
        return Err("unit scenarios require name, endpoint, and staging directory".to_owned());
    }
    for arguments in arguments.chunks_exact(3) {
        let scenario = UnitScenario::parse(arguments[0].to_str().ok_or("scenario is not valid UTF-8")?)?;
        let endpoint = arguments[1].to_str().ok_or("endpoint is not valid UTF-8")?.to_owned();
        run_scenario(
            &BlobStorage::s3(
                S3Config::new(unit_settings(endpoint)).map_err(|error| error.to_string())?,
                PathBuf::from(&arguments[2]),
            ),
            scenario,
        )
        .await;
    }
    Ok(())
}

async fn run_integration(scenario_name: String, endpoint: String, staging_dir: PathBuf) -> Result<(), String> {
    let mut settings = integration_settings(endpoint);
    #[cfg(feature = "container-tests")]
    if scenario_name == "cancel" {
        settings.upload_concurrency = 1;
    }
    if matches!(scenario_name.as_str(), "wire_multipart" | "wire_parallel_multipart") {
        settings.upload_concurrency = 3;
    } else if matches!(
        scenario_name.as_str(),
        "wire_abort_failure"
            | "wire_conflict_exhausted"
            | "wire_create_failure"
            | "wire_interrupted_multipart"
            | "wire_present_bound"
            | "wire_present_failure"
            | "recover_none"
            | "recover_one"
            | "recover_error"
    ) {
        settings.max_retries = 0;
    } else if scenario_name == "wire_huge_timeout" {
        settings.request_timeout = Duration::MAX;
    } else if matches!(
        scenario_name.as_str(),
        "wire_truncated_body" | "wire_head_missing_length" | "wire_get_missing_length"
    ) {
        settings.max_retries = 0;
    } else if scenario_name == "wire_send_timeout" {
        settings.request_timeout = Duration::from_millis(500);
        settings.max_retries = 0;
    }
    #[cfg(feature = "container-tests")]
    if scenario_name.starts_with("stream_") {
        // The body deadline starts after toxiproxy applies throttling.
        settings.request_timeout = Duration::from_secs(2);
        settings.max_retries = 0;
    }
    #[cfg(feature = "container-tests")]
    if scenario_name == "cancel" {
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(JournalSignal)).unwrap();
    }
    run_child_scenario(
        &BlobStorage::s3(S3Config::new(settings).map_err(|error| error.to_string())?, staging_dir),
        ChildScenario::parse(&scenario_name)?,
    )
    .await;
    Ok(())
}

fn unit_settings(endpoint: String) -> S3Settings {
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

fn integration_settings(endpoint: String) -> S3Settings {
    S3Settings {
        endpoint,
        bucket: BUCKET.to_owned(),
        prefix: "cache".to_owned(),
        region: "us-east-1".to_owned(),
        path_style: true,
        request_timeout: Duration::from_secs(10),
        max_retries: 2,
        multipart_threshold: 5 << 20,
        part_size: 5 << 20,
        upload_concurrency: 2,
        conditional_writes: true,
        checksum_writes: true,
    }
}

async fn run_scenario(storage: &BlobStorage, scenario: UnitScenario) {
    let digest = Digest::of(b"package");
    match scenario {
        UnitScenario::Health => storage.health().await.unwrap(),
        UnitScenario::Put => assert_eq!(storage.put_bytes(b"package").await.unwrap(), digest),
        UnitScenario::Head => assert_eq!(storage.head(&digest).await.unwrap().unwrap().bytes, 7),
        UnitScenario::WholeRead => assert_eq!(storage.read_bytes(&digest, 7).await.unwrap(), b"package"),
        UnitScenario::Range => assert_eq!(
            storage
                .open(&digest, Some(1..5))
                .await
                .unwrap()
                .collect(4)
                .await
                .unwrap(),
            b"acka"
        ),
        UnitScenario::EmptyRange => assert!(
            storage
                .open(&digest, Some(3..3))
                .await
                .unwrap()
                .collect(0)
                .await
                .unwrap()
                .is_empty()
        ),
        UnitScenario::Verify => assert!(storage.verify(&digest).await.unwrap()),
        UnitScenario::Materialize => assert_eq!(
            std::fs::read(storage.materialize(&digest).await.unwrap().path()).unwrap(),
            b"package"
        ),
        UnitScenario::Present => assert!(storage.present(vec![digest.clone()]).await.unwrap().contains(&digest)),
        UnitScenario::Delete => assert!(storage.delete(&digest).await.unwrap()),
        UnitScenario::Multipart => {
            storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap();
        }
        UnitScenario::InvalidRange => assert_eq!(
            storage.open(&digest, Some(8..9)).await.err().unwrap().kind(),
            BlobErrorKind::InvalidRange
        ),
        UnitScenario::RangeMissing => assert_eq!(
            storage.open(&digest, Some(1..5)).await.err().unwrap().kind(),
            BlobErrorKind::NotFound
        ),
        UnitScenario::RangeError => assert_eq!(
            storage.open(&digest, Some(1..5)).await.err().unwrap().kind(),
            BlobErrorKind::Io
        ),
        UnitScenario::VerifyMissing => assert_eq!(
            storage.verify(&digest).await.unwrap_err().kind(),
            BlobErrorKind::NotFound
        ),
        UnitScenario::VerifyError => assert_eq!(storage.verify(&digest).await.unwrap_err().kind(), BlobErrorKind::Io),
        UnitScenario::MaterializeMissing => assert_eq!(
            storage.materialize(&digest).await.unwrap_err().kind(),
            BlobErrorKind::NotFound
        ),
        UnitScenario::MaterializeError => assert_eq!(
            storage.materialize(&digest).await.unwrap_err().kind(),
            BlobErrorKind::Io
        ),
        UnitScenario::DeleteHeadError | UnitScenario::DeleteError => {
            assert_eq!(storage.delete(&digest).await.unwrap_err().kind(), BlobErrorKind::Io);
        }
        UnitScenario::AbortMissing => assert_eq!(
            storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap_err().kind(),
            BlobErrorKind::Io
        ),
        UnitScenario::PutMissingStage => {
            let staged = stage(storage, b"package").await;
            staged.with_materialized(|path| std::fs::remove_file(path).unwrap());
            assert_eq!(staged.commit().await.unwrap_err().kind(), BlobErrorKind::Io);
        }
        UnitScenario::MultipartMissingStage => {
            let staged = stage(storage, &vec![7; (5 << 20) + 1]).await;
            staged.with_materialized(|path| std::fs::remove_file(path).unwrap());
            assert_eq!(staged.commit().await.unwrap_err().kind(), BlobErrorKind::Io);
        }
        UnitScenario::BeginError => assert_eq!(storage.begin().await.err().unwrap().kind(), BlobErrorKind::Io),
        UnitScenario::WriteFlush => run_write(storage, UnitWriteScenario::Flush).await,
        UnitScenario::WriteTail => run_write(storage, UnitWriteScenario::Tail).await,
        UnitScenario::WriteCommit => run_write(storage, UnitWriteScenario::Commit).await,
        UnitScenario::WriteAbort => run_write(storage, UnitWriteScenario::Abort).await,
        UnitScenario::StagedLen => run_write(storage, UnitWriteScenario::StagedLen).await,
        UnitScenario::StagedEmpty => run_write(storage, UnitWriteScenario::StagedEmpty).await,
        UnitScenario::StagedMaterialized => run_write(storage, UnitWriteScenario::StagedMaterialized).await,
        UnitScenario::StagedAbort => run_write(storage, UnitWriteScenario::StagedAbort).await,
        UnitScenario::BlockingStage => run_blocking(storage, &digest, UnitBlockingScenario::Stage),
        UnitScenario::BlockingHead => run_blocking(storage, &digest, UnitBlockingScenario::Head),
        UnitScenario::BlockingRead => run_blocking(storage, &digest, UnitBlockingScenario::Read),
        UnitScenario::BlockingMaterialize => run_blocking(storage, &digest, UnitBlockingScenario::Materialize),
        UnitScenario::BlockingVerify => run_blocking(storage, &digest, UnitBlockingScenario::Verify),
        UnitScenario::BlockingDelete => run_blocking(storage, &digest, UnitBlockingScenario::Delete),
        UnitScenario::BlockingVisit => run_blocking(storage, &digest, UnitBlockingScenario::Visit),
    }
}

#[derive(Clone, Copy)]
enum UnitWriteScenario {
    Flush,
    Tail,
    Commit,
    Abort,
    StagedLen,
    StagedEmpty,
    StagedMaterialized,
    StagedAbort,
}

async fn run_write(storage: &BlobStorage, scenario: UnitWriteScenario) {
    match scenario {
        UnitWriteScenario::Flush => {
            let mut write = storage.begin().await.unwrap();
            write.write_chunk(Bytes::from_static(b"package")).await.unwrap();
            assert_eq!(write.flush().await.unwrap(), 7);
        }
        UnitWriteScenario::Tail => assert!(storage.begin().await.unwrap().tail().unwrap().open().is_ok()),
        UnitWriteScenario::Commit => {
            let mut write = storage.begin().await.unwrap();
            write.write_chunk(Bytes::from_static(b"package")).await.unwrap();
            write.commit(&Digest::of(b"package")).await.unwrap();
        }
        UnitWriteScenario::Abort => storage.begin().await.unwrap().abort().await.unwrap(),
        UnitWriteScenario::StagedLen => assert_eq!(stage(storage, b"package").await.len(), 7),
        UnitWriteScenario::StagedEmpty => assert!(stage(storage, b"").await.is_empty()),
        UnitWriteScenario::StagedMaterialized => assert_eq!(
            stage(storage, b"package")
                .await
                .with_materialized(|path| std::fs::read(path))
                .unwrap(),
            b"package"
        ),
        UnitWriteScenario::StagedAbort => stage(storage, b"package").await.abort().await.unwrap(),
    }
}

#[derive(Clone, Copy)]
enum UnitBlockingScenario {
    Stage,
    Head,
    Read,
    Materialize,
    Verify,
    Delete,
    Visit,
}

fn run_blocking(storage: &BlobStorage, digest: &Digest, scenario: UnitBlockingScenario) {
    match scenario {
        UnitBlockingScenario::Stage => assert_eq!(
            storage
                .blocking()
                .stage_reader(&mut std::io::Cursor::new(b"package"))
                .unwrap_err()
                .kind(),
            BlobErrorKind::Unsupported
        ),
        UnitBlockingScenario::Head => assert_eq!(
            storage.blocking().head(digest).unwrap_err().kind(),
            BlobErrorKind::Unsupported
        ),
        UnitBlockingScenario::Read => assert_eq!(
            storage.blocking().read_bytes(digest, 7).unwrap_err().kind(),
            BlobErrorKind::Unsupported
        ),
        UnitBlockingScenario::Materialize => assert_eq!(
            storage.blocking().materialize(digest).unwrap_err().kind(),
            BlobErrorKind::Unsupported
        ),
        UnitBlockingScenario::Verify => assert_eq!(
            storage.blocking().verify(digest).unwrap_err().kind(),
            BlobErrorKind::Unsupported
        ),
        UnitBlockingScenario::Delete => assert_eq!(
            storage.blocking().delete(digest).unwrap_err().kind(),
            BlobErrorKind::Unsupported
        ),
        UnitBlockingScenario::Visit => {
            let dir = tempfile::tempdir().unwrap();
            let filesystem = BlobStorage::filesystem(dir.path());
            filesystem.blocking().put_bytes(b"package").unwrap();
            let mut visits = 0;
            {
                let mut visit = |_: peryx_storage::blob::BlobEntry| {
                    visits += 1;
                    (visits == 1).then_some(()).ok_or("stop")
                };
                filesystem.blocking().visit(&mut visit).unwrap();
                assert!(matches!(
                    filesystem.blocking().visit(&mut visit),
                    Err(BlobScanError::Visit("stop"))
                ));
                assert!(matches!(
                    storage.blocking().visit(&mut visit),
                    Err(BlobScanError::Store(error)) if error.kind() == BlobErrorKind::Unsupported
                ));
            }
            assert_eq!(visits, 2);
        }
    }
}

async fn stage(storage: &BlobStorage, bytes: &[u8]) -> BlobStaged {
    let mut write = storage.begin().await.unwrap();
    write.write_chunk(Bytes::copy_from_slice(bytes)).await.unwrap();
    write.finish().await.unwrap()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnitScenario {
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

const UNIT_SCENARIOS: [UnitScenario; 39] = [
    UnitScenario::Health,
    UnitScenario::Put,
    UnitScenario::Head,
    UnitScenario::WholeRead,
    UnitScenario::Range,
    UnitScenario::EmptyRange,
    UnitScenario::InvalidRange,
    UnitScenario::RangeMissing,
    UnitScenario::RangeError,
    UnitScenario::Verify,
    UnitScenario::VerifyMissing,
    UnitScenario::VerifyError,
    UnitScenario::Materialize,
    UnitScenario::MaterializeMissing,
    UnitScenario::MaterializeError,
    UnitScenario::Present,
    UnitScenario::Delete,
    UnitScenario::DeleteHeadError,
    UnitScenario::DeleteError,
    UnitScenario::Multipart,
    UnitScenario::AbortMissing,
    UnitScenario::PutMissingStage,
    UnitScenario::MultipartMissingStage,
    UnitScenario::BeginError,
    UnitScenario::WriteFlush,
    UnitScenario::WriteTail,
    UnitScenario::WriteCommit,
    UnitScenario::WriteAbort,
    UnitScenario::StagedLen,
    UnitScenario::StagedEmpty,
    UnitScenario::StagedMaterialized,
    UnitScenario::StagedAbort,
    UnitScenario::BlockingStage,
    UnitScenario::BlockingHead,
    UnitScenario::BlockingRead,
    UnitScenario::BlockingMaterialize,
    UnitScenario::BlockingVerify,
    UnitScenario::BlockingDelete,
    UnitScenario::BlockingVisit,
];

impl UnitScenario {
    fn parse(value: &str) -> Result<Self, String> {
        UNIT_SCENARIOS
            .iter()
            .copied()
            .find(|scenario| scenario.name() == value)
            .ok_or_else(|| format!("unknown unit S3 scenario: {value}"))
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Health => "health",
            Self::Put => "put",
            Self::Head => "head",
            Self::WholeRead => "whole-read",
            Self::Range => "range",
            Self::EmptyRange => "empty-range",
            Self::InvalidRange => "invalid-range",
            Self::RangeMissing => "range-missing",
            Self::RangeError => "range-error",
            Self::Verify => "verify",
            Self::VerifyMissing => "verify-missing",
            Self::VerifyError => "verify-error",
            Self::Materialize => "materialize",
            Self::MaterializeMissing => "materialize-missing",
            Self::MaterializeError => "materialize-error",
            Self::Present => "present",
            Self::Delete => "delete",
            Self::DeleteHeadError => "delete-head-error",
            Self::DeleteError => "delete-error",
            Self::Multipart => "multipart",
            Self::AbortMissing => "abort-missing",
            Self::PutMissingStage => "put-missing-stage",
            Self::MultipartMissingStage => "multipart-missing-stage",
            Self::BeginError => "begin-error",
            Self::WriteFlush => "write-flush",
            Self::WriteTail => "write-tail",
            Self::WriteCommit => "write-commit",
            Self::WriteAbort => "write-abort",
            Self::StagedLen => "staged-len",
            Self::StagedEmpty => "staged-empty",
            Self::StagedMaterialized => "staged-materialized",
            Self::StagedAbort => "staged-abort",
            Self::BlockingStage => "blocking-stage",
            Self::BlockingHead => "blocking-head",
            Self::BlockingRead => "blocking-read",
            Self::BlockingMaterialize => "blocking-materialize",
            Self::BlockingVerify => "blocking-verify",
            Self::BlockingDelete => "blocking-delete",
            Self::BlockingVisit => "blocking-visit",
        }
    }
}

async fn run_child_scenario(storage: &BlobStorage, scenario: ChildScenario) {
    match scenario {
        ChildScenario::Health => storage.health().await.unwrap(),
        #[cfg(feature = "container-tests")]
        ChildScenario::Container(scenario) => run_container_child(storage, scenario).await,
        ChildScenario::Read(scenario) => run_wire_read_child(storage, scenario).await,
        ChildScenario::Write(scenario) => run_wire_write_child(storage, scenario).await,
        ChildScenario::Failure(scenario) => run_wire_failure_child(storage, scenario).await,
        ChildScenario::Multipart(scenario) => run_wire_multipart_child(storage, scenario).await,
        ChildScenario::Recover(scenario) => run_recover_child(storage, scenario).await,
    }
}

async fn run_recover_child(storage: &BlobStorage, scenario: RecoverScenario) {
    match scenario {
        RecoverScenario::Nothing => assert_eq!(storage.recover_incomplete_uploads().await.unwrap(), 0),
        RecoverScenario::Aborted => assert_eq!(storage.recover_incomplete_uploads().await.unwrap(), 1),
        RecoverScenario::Error => assert_eq!(
            storage.recover_incomplete_uploads().await.unwrap_err().kind(),
            BlobErrorKind::Io
        ),
    }
}

#[cfg(feature = "container-tests")]
async fn run_container_child(storage: &BlobStorage, scenario: ContainerScenario) {
    match scenario {
        ContainerScenario::Invalid => assert_eq!(storage.health().await.unwrap_err().kind(), BlobErrorKind::Io),
        ContainerScenario::Readonly => {
            storage.health().await.unwrap();
            assert_eq!(
                storage.put_bytes(b"denied").await.unwrap_err().kind(),
                BlobErrorKind::Io
            );
        }
        ContainerScenario::Cancel => {
            let other = storage.clone();
            let upload = tokio::spawn(async move { other.put_bytes(&vec![7; (5 << 20) + 1]).await });
            tokio::task::spawn_blocking(|| stdin().read_exact(&mut [0]))
                .await
                .unwrap()
                .unwrap();
            upload.abort();
            assert!(upload.await.unwrap_err().is_cancelled());
        }
        ContainerScenario::Multipart => {
            storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap();
        }
        ContainerScenario::Concurrent => {
            let other = storage.clone();
            let multipart = vec![7; (5 << 20) + 1];
            let (first, second) = tokio::join!(storage.put_bytes(&multipart), other.put_bytes(&multipart));
            assert_eq!(first.unwrap(), second.unwrap());
            // A whole-object write races on the same precondition, and the loser has to prove the object
            // the winner left before it reports a placement of its own.
            let (first, second) = tokio::join!(storage.put_bytes(b"concurrent"), other.put_bytes(b"concurrent"));
            assert_eq!(first.unwrap(), second.unwrap());
        }
        ContainerScenario::Foreign => {
            let error = storage.put_bytes(b"expected").await.unwrap_err();
            assert_eq!(error.kind(), BlobErrorKind::DigestMismatch);
            assert_eq!(
                error.mismatch(),
                Some((Digest::of(b"expected").as_str(), Digest::of(b"squatted").as_str()))
            );
        }
        ContainerScenario::StreamReset => {
            run_container_stream(storage, Some("s3 request failed: streaming error")).await;
        }
        ContainerScenario::StreamTimeout => {
            run_container_stream(storage, Some("s3 request failed: deadline has elapsed")).await;
        }
        ContainerScenario::StreamTrickle => run_container_stream(storage, None).await,
    }
}

#[cfg(feature = "container-tests")]
async fn run_container_stream(storage: &BlobStorage, expected_error: Option<&str>) {
    let bytes = vec![0x5a; STREAM_BYTES];
    let read = storage.open(&Digest::of(&bytes), None).await.unwrap();
    {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{STREAM_OPENED}").unwrap();
        stdout.flush().unwrap();
    }
    tokio::task::spawn_blocking(|| stdin().read_exact(&mut [0]))
        .await
        .unwrap()
        .unwrap();
    let result = read.collect(STREAM_BYTES as u64).await;
    if let Some(expected_error) = expected_error {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), BlobErrorKind::Io);
        assert_eq!(error.source().unwrap().to_string(), expected_error);
    } else {
        assert_eq!(result.unwrap(), bytes);
    }
}

async fn run_wire_read_child(storage: &BlobStorage, scenario: WireReadScenario) {
    match scenario {
        WireReadScenario::Missing => {
            assert_eq!(
                storage.open(&Digest::of(b"missing"), None).await.err().unwrap().kind(),
                BlobErrorKind::NotFound
            );
        }
        WireReadScenario::Head => {
            assert_eq!(storage.head(&Digest::of(b"package")).await.unwrap().unwrap().bytes, 7);
        }
        WireReadScenario::WholeRead => {
            assert_eq!(
                storage.read_bytes(&Digest::of(b"package"), 7).await.unwrap(),
                b"package"
            );
        }
        WireReadScenario::Range => {
            assert_eq!(
                storage
                    .open(&Digest::of(b"package"), Some(1..5))
                    .await
                    .unwrap()
                    .collect(4)
                    .await
                    .unwrap(),
                b"acka"
            );
        }
        WireReadScenario::RangeGenerationChanged => {
            assert_range_open_error(storage, "object changed during read").await;
        }
        WireReadScenario::RangeTotalMismatch => {
            assert_range_open_error(storage, "s3 returned an invalid content range total").await;
        }
        WireReadScenario::RangeMissingEtag => {
            assert_range_open_error(storage, "s3 returned an invalid ETag").await;
        }
        WireReadScenario::EmptyRange => {
            assert!(
                storage
                    .open(&Digest::of(b"package"), Some(3..3))
                    .await
                    .unwrap()
                    .collect(0)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        WireReadScenario::Verify => assert!(storage.verify(&Digest::of(b"package")).await.unwrap()),
        WireReadScenario::VerifyMismatch => {
            let digest = Digest::of(b"expected");
            assert_eq!(storage.read_bytes(&digest, 7).await.unwrap(), b"corrupt");
            assert!(!storage.verify(&digest).await.unwrap());
        }
        WireReadScenario::TruncatedBody => {
            assert_eq!(
                storage
                    .open(&Digest::of(b"short"), None)
                    .await
                    .unwrap()
                    .collect(10)
                    .await
                    .unwrap_err()
                    .kind(),
                BlobErrorKind::Io
            );
        }
        WireReadScenario::Materialize => {
            assert_eq!(
                std::fs::read(storage.materialize(&Digest::of(b"package")).await.unwrap().path()).unwrap(),
                b"package"
            );
        }
    }
}

async fn assert_range_open_error(storage: &BlobStorage, expected: &str) {
    let error = storage.open(&Digest::of(b"package"), Some(1..5)).await.err().unwrap();
    assert_eq!(
        (error.kind(), std::error::Error::source(&error).unwrap().to_string()),
        (BlobErrorKind::Io, expected.to_owned())
    );
}

async fn run_wire_write_child(storage: &BlobStorage, scenario: WireWriteScenario) {
    match scenario {
        WireWriteScenario::Delete => {
            let digest = Digest::of(b"package");
            assert!(storage.delete(&digest).await.unwrap());
            assert!(!storage.delete(&digest).await.unwrap());
        }
        WireWriteScenario::Present => {
            let present = Digest::of(b"present");
            assert_eq!(
                storage
                    .present(vec![present.clone(), Digest::of(b"missing"), present.clone()])
                    .await
                    .unwrap(),
                std::collections::HashSet::from([present])
            );
        }
        WireWriteScenario::PresentBound => assert!(storage.present(presence_batch()).await.unwrap().is_empty()),
        WireWriteScenario::PresentFailure => {
            let error = storage.present(presence_batch()).await.unwrap_err();
            assert_eq!(
                (error.kind(), error.context().unwrap().operation),
                (BlobErrorKind::Io, BlobOperation::Head)
            );
        }
        WireWriteScenario::SmallPut => {
            assert_eq!(
                commit_bytes(storage, b"package").await,
                WriteEvidence::ObjectStoreVerified
            );
        }
        WireWriteScenario::Resumable => {
            assert_eq!(storage.stage_upload_chunk("empty", 0, b"").await.unwrap(), 0);
            let empty = Digest::of(b"");
            assert_eq!(
                storage.finish_upload("empty", &empty).await.unwrap(),
                PlacementReceipt {
                    digest: empty,
                    size: 0,
                    durability: storage.durability(),
                    evidence: WriteEvidence::ObjectStoreVerified,
                }
            );
            assert_eq!(storage.staged_upload_len("empty").await.unwrap(), None);

            assert_eq!(storage.stage_upload_chunk("chunked", 0, b"layer ").await.unwrap(), 6);
            assert_eq!(storage.stage_upload_chunk("chunked", 6, b"bytes").await.unwrap(), 11);
            let error = storage
                .finish_upload("chunked", &Digest::of(b"wrong"))
                .await
                .unwrap_err();
            assert_eq!(error.kind(), BlobErrorKind::DigestMismatch);
            assert_eq!(storage.staged_upload_len("chunked").await.unwrap(), Some(11));

            let digest = Digest::of(b"layer bytes");
            let receipt = PlacementReceipt {
                digest: digest.clone(),
                size: 11,
                durability: storage.durability(),
                evidence: WriteEvidence::ObjectStoreVerified,
            };
            assert_eq!(storage.finish_upload("chunked", &digest).await.unwrap(), receipt);
            assert_eq!(storage.staged_upload_len("chunked").await.unwrap(), None);
            // The client that lost this response retries into a session whose stage the commit removed,
            // and the resident object answers it.
            assert_eq!(storage.finish_upload("chunked", &digest).await.unwrap(), receipt);
        }
        WireWriteScenario::ResumableFailure => {
            storage.stage_upload_chunk("chunked", 0, b"layer bytes").await.unwrap();

            let error = storage
                .finish_upload("chunked", &Digest::of(b"layer bytes"))
                .await
                .unwrap_err();

            assert_eq!(error.kind(), BlobErrorKind::Io);
            assert_eq!(storage.staged_upload_len("chunked").await.unwrap(), Some(11));
        }
        WireWriteScenario::ResumableMissing => {
            let error = storage
                .finish_upload("missing", &Digest::of(b"layer bytes"))
                .await
                .unwrap_err();

            assert_eq!(error.kind(), BlobErrorKind::NotFound);
            assert_eq!(error.context().unwrap().backend, "s3");
        }
        WireWriteScenario::Immutable => {
            // The create lost the precondition, so reading the resident object back is what proves these
            // bytes are the ones at that address.
            assert_eq!(
                commit_bytes(storage, b"expected").await,
                WriteEvidence::ObjectStoreVerified
            );
        }
        WireWriteScenario::ImmutableMismatch => {
            let error = storage.put_bytes(b"expected").await.unwrap_err();
            assert_eq!(error.kind(), BlobErrorKind::DigestMismatch);
            assert_eq!(
                error.mismatch(),
                Some((Digest::of(b"expected").as_str(), Digest::of(b"existing").as_str()))
            );
        }
    }
}

async fn commit_bytes(storage: &BlobStorage, bytes: &'static [u8]) -> WriteEvidence {
    let mut write = storage.begin().await.unwrap();
    write.write_chunk(Bytes::from_static(bytes)).await.unwrap();
    write.commit(&Digest::of(bytes)).await.unwrap().evidence
}

async fn run_wire_failure_child(storage: &BlobStorage, scenario: WireFailureScenario) {
    match scenario {
        WireFailureScenario::Health => assert_eq!(storage.health().await.unwrap_err().kind(), BlobErrorKind::Io),
        WireFailureScenario::Head | WireFailureScenario::HeadMissingLength => assert_eq!(
            storage.head(&Digest::of(b"package")).await.unwrap_err().kind(),
            BlobErrorKind::Io
        ),
        WireFailureScenario::HugeTimeout
        | WireFailureScenario::Get
        | WireFailureScenario::GetMissingLength
        | WireFailureScenario::GetMissingBucket => {
            assert_eq!(
                storage.open(&Digest::of(b"package"), None).await.err().unwrap().kind(),
                BlobErrorKind::Io
            );
        }
        WireFailureScenario::SendTimeout => {
            let error = storage.open(&Digest::of(b"package"), None).await.err().unwrap();
            assert_eq!(error.kind(), BlobErrorKind::Io);
            assert!(format!("{error:?}").contains("deadline has elapsed"), "{error:?}");
        }
        WireFailureScenario::Put => assert_eq!(
            storage.put_bytes(b"package").await.unwrap_err().kind(),
            BlobErrorKind::Io
        ),
        WireFailureScenario::DeleteMissingBucket => assert_eq!(
            storage.delete(&Digest::of(b"package")).await.unwrap_err().kind(),
            BlobErrorKind::Io
        ),
        WireFailureScenario::DeleteNotFound => assert!(!storage.delete(&Digest::of(b"missing")).await.unwrap()),
    }
}

async fn run_wire_multipart_child(storage: &BlobStorage, scenario: WireMultipartScenario) {
    match scenario {
        WireMultipartScenario::ParallelMultipart => {
            storage.put_bytes(&vec![7; (20 << 20) + 1]).await.unwrap();
        }
        WireMultipartScenario::Multipart
        | WireMultipartScenario::Conflict
        | WireMultipartScenario::CompleteExists
        | WireMultipartScenario::StaleUpload => {
            storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap();
        }
        WireMultipartScenario::CreateFailure => {
            let error = storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap_err();
            assert_eq!(error.kind(), BlobErrorKind::Io);
            assert!(format!("{error:?}").contains("s3 request failed"), "{error:?}");
        }
        WireMultipartScenario::CreateMissingId
        | WireMultipartScenario::PartMissingEtag
        | WireMultipartScenario::PartMissingChecksum
        | WireMultipartScenario::CompleteFailure
        | WireMultipartScenario::ConflictExhausted
        | WireMultipartScenario::JournalFailure
        | WireMultipartScenario::Interrupted => {
            assert_eq!(
                storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap_err().kind(),
                BlobErrorKind::Io
            );
        }
        WireMultipartScenario::AbortFailure => {
            let bytes = vec![11; (5 << 20) + 1];
            let error = storage.put_bytes(&bytes).await.unwrap_err();
            assert!(
                std::error::Error::source(&error)
                    .unwrap()
                    .to_string()
                    .contains("upload-1")
            );
            storage.put_bytes(&bytes).await.unwrap();
        }
    }
}

#[derive(Clone, Copy)]
enum ChildScenario {
    Health,
    #[cfg(feature = "container-tests")]
    Container(ContainerScenario),
    Read(WireReadScenario),
    Write(WireWriteScenario),
    Failure(WireFailureScenario),
    Multipart(WireMultipartScenario),
    Recover(RecoverScenario),
}

#[derive(Clone, Copy)]
enum RecoverScenario {
    Nothing,
    Aborted,
    Error,
}

#[cfg(feature = "container-tests")]
#[derive(Clone, Copy)]
enum ContainerScenario {
    Invalid,
    Readonly,
    Cancel,
    Multipart,
    Concurrent,
    Foreign,
    StreamReset,
    StreamTimeout,
    StreamTrickle,
}

#[derive(Clone, Copy)]
enum WireReadScenario {
    Missing,
    Head,
    WholeRead,
    Range,
    RangeGenerationChanged,
    RangeTotalMismatch,
    RangeMissingEtag,
    EmptyRange,
    Verify,
    VerifyMismatch,
    TruncatedBody,
    Materialize,
}

#[derive(Clone, Copy)]
enum WireWriteScenario {
    Delete,
    Present,
    PresentBound,
    PresentFailure,
    SmallPut,
    Resumable,
    ResumableFailure,
    ResumableMissing,
    Immutable,
    ImmutableMismatch,
}

/// Twice the bulk presence concurrency bound, so the batch cannot be served in a single round.
const PRESENCE_BATCH: usize = 64;

fn presence_batch() -> Vec<Digest> {
    (0..PRESENCE_BATCH)
        .map(|index| Digest::of(&index.to_le_bytes()))
        .collect()
}

#[derive(Clone, Copy)]
enum WireFailureScenario {
    Health,
    Head,
    HeadMissingLength,
    HugeTimeout,
    SendTimeout,
    GetMissingLength,
    Get,
    GetMissingBucket,
    Put,
    DeleteMissingBucket,
    DeleteNotFound,
}

#[derive(Clone, Copy)]
enum WireMultipartScenario {
    Multipart,
    ParallelMultipart,
    Conflict,
    CreateFailure,
    CreateMissingId,
    PartMissingEtag,
    PartMissingChecksum,
    CompleteFailure,
    ConflictExhausted,
    JournalFailure,
    CompleteExists,
    StaleUpload,
    AbortFailure,
    Interrupted,
}

impl ChildScenario {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "health" => Ok(Self::Health),
            #[cfg(feature = "container-tests")]
            "invalid" => Ok(Self::Container(ContainerScenario::Invalid)),
            #[cfg(feature = "container-tests")]
            "readonly" => Ok(Self::Container(ContainerScenario::Readonly)),
            #[cfg(feature = "container-tests")]
            "cancel" => Ok(Self::Container(ContainerScenario::Cancel)),
            #[cfg(feature = "container-tests")]
            "multipart" => Ok(Self::Container(ContainerScenario::Multipart)),
            #[cfg(feature = "container-tests")]
            "concurrent" => Ok(Self::Container(ContainerScenario::Concurrent)),
            #[cfg(feature = "container-tests")]
            "foreign" => Ok(Self::Container(ContainerScenario::Foreign)),
            #[cfg(feature = "container-tests")]
            "stream_reset" => Ok(Self::Container(ContainerScenario::StreamReset)),
            #[cfg(feature = "container-tests")]
            "stream_timeout" => Ok(Self::Container(ContainerScenario::StreamTimeout)),
            #[cfg(feature = "container-tests")]
            "stream_trickle" => Ok(Self::Container(ContainerScenario::StreamTrickle)),
            "wire_missing" => Ok(Self::Read(WireReadScenario::Missing)),
            "wire_head" => Ok(Self::Read(WireReadScenario::Head)),
            "wire_whole_read" => Ok(Self::Read(WireReadScenario::WholeRead)),
            "wire_range" => Ok(Self::Read(WireReadScenario::Range)),
            "wire_range_generation_changed" => Ok(Self::Read(WireReadScenario::RangeGenerationChanged)),
            "wire_range_total_mismatch" => Ok(Self::Read(WireReadScenario::RangeTotalMismatch)),
            "wire_range_missing_etag" => Ok(Self::Read(WireReadScenario::RangeMissingEtag)),
            "wire_empty_range" => Ok(Self::Read(WireReadScenario::EmptyRange)),
            "wire_verify" => Ok(Self::Read(WireReadScenario::Verify)),
            "wire_verify_mismatch" => Ok(Self::Read(WireReadScenario::VerifyMismatch)),
            "wire_truncated_body" => Ok(Self::Read(WireReadScenario::TruncatedBody)),
            "wire_materialize" => Ok(Self::Read(WireReadScenario::Materialize)),
            "wire_delete" => Ok(Self::Write(WireWriteScenario::Delete)),
            "wire_present" => Ok(Self::Write(WireWriteScenario::Present)),
            "wire_present_bound" => Ok(Self::Write(WireWriteScenario::PresentBound)),
            "wire_present_failure" => Ok(Self::Write(WireWriteScenario::PresentFailure)),
            "wire_small_put" => Ok(Self::Write(WireWriteScenario::SmallPut)),
            "wire_resumable" => Ok(Self::Write(WireWriteScenario::Resumable)),
            "wire_resumable_failure" => Ok(Self::Write(WireWriteScenario::ResumableFailure)),
            "wire_resumable_missing" => Ok(Self::Write(WireWriteScenario::ResumableMissing)),
            "wire_immutable" => Ok(Self::Write(WireWriteScenario::Immutable)),
            "wire_immutable_mismatch" => Ok(Self::Write(WireWriteScenario::ImmutableMismatch)),
            "wire_health_error" => Ok(Self::Failure(WireFailureScenario::Health)),
            "wire_head_error" => Ok(Self::Failure(WireFailureScenario::Head)),
            "wire_head_missing_length" => Ok(Self::Failure(WireFailureScenario::HeadMissingLength)),
            "wire_huge_timeout" => Ok(Self::Failure(WireFailureScenario::HugeTimeout)),
            "wire_send_timeout" => Ok(Self::Failure(WireFailureScenario::SendTimeout)),
            "wire_get_missing_length" => Ok(Self::Failure(WireFailureScenario::GetMissingLength)),
            "wire_get_error" => Ok(Self::Failure(WireFailureScenario::Get)),
            "wire_get_missing_bucket" => Ok(Self::Failure(WireFailureScenario::GetMissingBucket)),
            "wire_put_error" => Ok(Self::Failure(WireFailureScenario::Put)),
            "wire_delete_missing_bucket" => Ok(Self::Failure(WireFailureScenario::DeleteMissingBucket)),
            "wire_delete_not_found" => Ok(Self::Failure(WireFailureScenario::DeleteNotFound)),
            "wire_multipart" => Ok(Self::Multipart(WireMultipartScenario::Multipart)),
            "wire_parallel_multipart" => Ok(Self::Multipart(WireMultipartScenario::ParallelMultipart)),
            "wire_conflict" => Ok(Self::Multipart(WireMultipartScenario::Conflict)),
            "wire_create_failure" => Ok(Self::Multipart(WireMultipartScenario::CreateFailure)),
            "wire_create_missing_id" => Ok(Self::Multipart(WireMultipartScenario::CreateMissingId)),
            "wire_part_missing_etag" => Ok(Self::Multipart(WireMultipartScenario::PartMissingEtag)),
            "wire_part_missing_checksum" => Ok(Self::Multipart(WireMultipartScenario::PartMissingChecksum)),
            "wire_complete_failure" => Ok(Self::Multipart(WireMultipartScenario::CompleteFailure)),
            "wire_conflict_exhausted" => Ok(Self::Multipart(WireMultipartScenario::ConflictExhausted)),
            "wire_journal_failure" => Ok(Self::Multipart(WireMultipartScenario::JournalFailure)),
            "wire_complete_exists" => Ok(Self::Multipart(WireMultipartScenario::CompleteExists)),
            "wire_stale_upload" => Ok(Self::Multipart(WireMultipartScenario::StaleUpload)),
            "wire_abort_failure" => Ok(Self::Multipart(WireMultipartScenario::AbortFailure)),
            "wire_interrupted_multipart" => Ok(Self::Multipart(WireMultipartScenario::Interrupted)),
            "recover_none" => Ok(Self::Recover(RecoverScenario::Nothing)),
            "recover_one" => Ok(Self::Recover(RecoverScenario::Aborted)),
            "recover_error" => Ok(Self::Recover(RecoverScenario::Error)),
            _ => Err(format!("unknown S3 scenario: {value}")),
        }
    }
}

#[cfg(feature = "container-tests")]
struct JournalSignal;

#[cfg(feature = "container-tests")]
impl<Registry> Layer<Registry> for JournalSignal
where
    Registry: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, Registry>) {
        if event.metadata().target() == "peryx_storage::s3_journal" {
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{JOURNAL_WRITTEN}").unwrap();
            stdout.flush().unwrap();
        }
    }
}
