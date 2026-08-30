use std::path::{Path, PathBuf};
use std::process::Output;
#[cfg(feature = "container-tests")]
use std::process::Stdio;
#[cfg(feature = "container-tests")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

#[cfg(feature = "container-tests")]
use aws_config::BehaviorVersion;
#[cfg(feature = "container-tests")]
use aws_sdk_s3::config::{Credentials, Region};
#[cfg(feature = "container-tests")]
use aws_sdk_s3::primitives::ByteStream;
use peryx_storage::blob::{BlobDurability, BlobStorage, BlobSupport, Digest, S3Config, S3Settings};
use rstest::rstest;
#[cfg(feature = "container-tests")]
use testcontainers::core::wait::ExitWaitStrategy;
#[cfg(feature = "container-tests")]
use testcontainers::core::{CmdWaitFor, ExecCommand, ImageExt as _, IntoContainerPort as _, WaitFor};
#[cfg(feature = "container-tests")]
use testcontainers::runners::AsyncRunner as _;
#[cfg(feature = "container-tests")]
use testcontainers::{ContainerAsync, GenericImage};
#[cfg(feature = "container-tests")]
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const BUCKET: &str = "peryx-tests";
const ROOT_ACCESS_KEY: &str = "peryx-minio";
const ROOT_SECRET_KEY: &str = "peryx-minio-secret";
#[cfg(feature = "container-tests")]
const READONLY_ACCESS_KEY: &str = "peryx-readonly";
#[cfg(feature = "container-tests")]
const READONLY_SECRET_KEY: &str = "peryx-readonly-secret";
#[cfg(feature = "container-tests")]
const STREAM_BYTES: usize = 8 << 20;
#[cfg(feature = "container-tests")]
const JOURNAL_WRITTEN: &str = "PERYX_JOURNAL_WRITTEN";
#[cfg(feature = "container-tests")]
const STREAM_OPENED: &str = "PERYX_STREAM_OPENED";
static CHILD_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);
#[cfg(feature = "container-tests")]
static NEXT_CONTAINER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
enum WireBehavior {
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
    Materialize,
    Delete,
    Present,
    SmallPut,
    Immutable,
    Multipart,
    HealthError,
    HeadError,
    HugeTimeout,
    GetError,
    GetMissingBucket,
    PutError,
    DeleteMissingBucket,
    DeleteNotFound,
    CreateFailure,
    CreateMissingId,
    PartMissingEtag,
    PartMissingChecksum,
    CompleteExists,
    CompleteFailure,
    StaleUpload,
    ConflictExhausted,
}

#[derive(Clone, Copy)]
enum WireReadBehavior {
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
    Materialize,
}

#[derive(Clone, Copy)]
enum WireWriteBehavior {
    Delete,
    Present,
    SmallPut,
    Immutable,
}

#[derive(Clone, Copy)]
enum WireFailureBehavior {
    Multipart,
    Health,
    Head,
    HugeTimeout,
    Get,
    GetMissingBucket,
    Put,
    DeleteMissingBucket,
    DeleteNotFound,
}

#[derive(Clone, Copy)]
enum WireMultipartFailureBehavior {
    Create,
    MissingUploadId,
    MissingPartEtag,
    MissingPartChecksum,
    CompleteExists,
    Complete,
    StaleUpload,
    ConflictExhausted,
}

impl WireBehavior {
    const fn scenario(self) -> &'static str {
        match self {
            Self::Missing => "wire_missing",
            Self::Head => "wire_head",
            Self::WholeRead => "wire_whole_read",
            Self::Range => "wire_range",
            Self::RangeGenerationChanged => "wire_range_generation_changed",
            Self::RangeTotalMismatch => "wire_range_total_mismatch",
            Self::RangeMissingEtag => "wire_range_missing_etag",
            Self::EmptyRange => "wire_empty_range",
            Self::Verify => "wire_verify",
            Self::VerifyMismatch => "wire_verify_mismatch",
            Self::Materialize => "wire_materialize",
            Self::Delete => "wire_delete",
            Self::Present => "wire_present",
            Self::SmallPut => "wire_small_put",
            Self::Immutable => "wire_immutable",
            Self::Multipart => "wire_multipart",
            Self::HealthError => "wire_health_error",
            Self::HeadError => "wire_head_error",
            Self::HugeTimeout => "wire_huge_timeout",
            Self::GetError => "wire_get_error",
            Self::GetMissingBucket => "wire_get_missing_bucket",
            Self::PutError => "wire_put_error",
            Self::DeleteMissingBucket => "wire_delete_missing_bucket",
            Self::DeleteNotFound => "wire_delete_not_found",
            Self::CreateFailure => "wire_create_failure",
            Self::CreateMissingId => "wire_create_missing_id",
            Self::PartMissingEtag => "wire_part_missing_etag",
            Self::PartMissingChecksum => "wire_part_missing_checksum",
            Self::CompleteExists => "wire_complete_exists",
            Self::CompleteFailure => "wire_complete_failure",
            Self::StaleUpload => "wire_stale_upload",
            Self::ConflictExhausted => "wire_conflict_exhausted",
        }
    }
}

#[cfg(feature = "container-tests")]
struct Minio {
    _container: ContainerAsync<GenericImage>,
    endpoint: String,
    network: String,
    name: String,
}

#[cfg(feature = "container-tests")]
struct Toxiproxy {
    container: ContainerAsync<GenericImage>,
    endpoint: String,
}

fn settings(endpoint: String) -> S3Settings {
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

fn create_response(upload_id: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        format!(
            "<InitiateMultipartUploadResult><Bucket>{BUCKET}</Bucket><Key>key</Key><UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>"
        ),
        "application/xml",
    )
}

fn complete_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        format!(
            "<CompleteMultipartUploadResult><Bucket>{BUCKET}</Bucket><Key>key</Key><ETag>etag</ETag></CompleteMultipartUploadResult>"
        ),
        "application/xml",
    )
}

fn service_error(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(format!("<Error><Code>{code}</Code></Error>"), "application/xml")
}

fn part_response() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("ETag", "part")
        .insert_header("x-amz-checksum-sha256", "checksum")
}

async fn mount_multipart(server: &MockServer) {
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(create_response("upload-1"))
        .mount(server)
        .await;
    Mock::given(method("PUT"))
        .and(query_param("uploadId", "upload-1"))
        .and(header("content-encoding", "aws-chunked"))
        .and(header("x-amz-trailer", "x-amz-checksum-sha256"))
        .respond_with(part_response())
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(complete_response())
        .mount(server)
        .await;
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;
}

fn count_creates(requests: &[Request]) -> usize {
    requests
        .iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.query_pairs().any(|(key, _)| key == "uploads")
        })
        .count()
}

fn object_path(bytes: &[u8]) -> String {
    format!("/{BUCKET}/cache/sha256/{}", Digest::of(bytes).as_str())
}

fn multipart_journal(staging: &Path) -> PathBuf {
    staging
        .join("s3-multipart")
        .join(Digest::of(&vec![7; (5 << 20) + 1]).as_str())
}

async fn mount_wire_behavior(server: &MockServer, behavior: WireBehavior) {
    match behavior {
        WireBehavior::Missing => mount_wire_reads(server, WireReadBehavior::Missing).await,
        WireBehavior::Head => mount_wire_reads(server, WireReadBehavior::Head).await,
        WireBehavior::WholeRead => mount_wire_reads(server, WireReadBehavior::WholeRead).await,
        WireBehavior::Range => mount_wire_reads(server, WireReadBehavior::Range).await,
        WireBehavior::RangeGenerationChanged => {
            mount_wire_reads(server, WireReadBehavior::RangeGenerationChanged).await;
        }
        WireBehavior::RangeTotalMismatch => {
            mount_wire_reads(server, WireReadBehavior::RangeTotalMismatch).await;
        }
        WireBehavior::RangeMissingEtag => mount_wire_reads(server, WireReadBehavior::RangeMissingEtag).await,
        WireBehavior::EmptyRange => mount_wire_reads(server, WireReadBehavior::EmptyRange).await,
        WireBehavior::Verify => mount_wire_reads(server, WireReadBehavior::Verify).await,
        WireBehavior::VerifyMismatch => mount_wire_reads(server, WireReadBehavior::VerifyMismatch).await,
        WireBehavior::Materialize => mount_wire_reads(server, WireReadBehavior::Materialize).await,
        WireBehavior::Delete => mount_wire_writes(server, WireWriteBehavior::Delete).await,
        WireBehavior::Present => mount_wire_writes(server, WireWriteBehavior::Present).await,
        WireBehavior::SmallPut => mount_wire_writes(server, WireWriteBehavior::SmallPut).await,
        WireBehavior::Immutable => mount_wire_writes(server, WireWriteBehavior::Immutable).await,
        WireBehavior::Multipart => mount_wire_failures(server, WireFailureBehavior::Multipart).await,
        WireBehavior::HealthError => mount_wire_failures(server, WireFailureBehavior::Health).await,
        WireBehavior::HeadError => mount_wire_failures(server, WireFailureBehavior::Head).await,
        WireBehavior::HugeTimeout => mount_wire_failures(server, WireFailureBehavior::HugeTimeout).await,
        WireBehavior::GetError => mount_wire_failures(server, WireFailureBehavior::Get).await,
        WireBehavior::GetMissingBucket => mount_wire_failures(server, WireFailureBehavior::GetMissingBucket).await,
        WireBehavior::PutError => mount_wire_failures(server, WireFailureBehavior::Put).await,
        WireBehavior::DeleteMissingBucket => {
            mount_wire_failures(server, WireFailureBehavior::DeleteMissingBucket).await;
        }
        WireBehavior::DeleteNotFound => mount_wire_failures(server, WireFailureBehavior::DeleteNotFound).await,
        WireBehavior::CreateFailure => {
            mount_wire_multipart_failures(server, WireMultipartFailureBehavior::Create).await;
        }
        WireBehavior::CreateMissingId => {
            mount_wire_multipart_failures(server, WireMultipartFailureBehavior::MissingUploadId).await;
        }
        WireBehavior::PartMissingEtag => {
            mount_wire_multipart_failures(server, WireMultipartFailureBehavior::MissingPartEtag).await;
        }
        WireBehavior::PartMissingChecksum => {
            mount_wire_multipart_failures(server, WireMultipartFailureBehavior::MissingPartChecksum).await;
        }
        WireBehavior::CompleteExists => {
            mount_wire_multipart_failures(server, WireMultipartFailureBehavior::CompleteExists).await;
        }
        WireBehavior::CompleteFailure => {
            mount_wire_multipart_failures(server, WireMultipartFailureBehavior::Complete).await;
        }
        WireBehavior::StaleUpload => {
            mount_wire_multipart_failures(server, WireMultipartFailureBehavior::StaleUpload).await;
        }
        WireBehavior::ConflictExhausted => {
            mount_wire_multipart_failures(server, WireMultipartFailureBehavior::ConflictExhausted).await;
        }
    }
}

async fn mount_wire_reads(server: &MockServer, behavior: WireReadBehavior) {
    match behavior {
        WireReadBehavior::Missing => {
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(404).set_body_raw("<Error><Code>NoSuchKey</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
        }
        WireReadBehavior::Head | WireReadBehavior::EmptyRange => {
            Mock::given(method("HEAD"))
                .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "7"))
                .mount(server)
                .await;
        }
        WireReadBehavior::WholeRead | WireReadBehavior::Verify | WireReadBehavior::Materialize => {
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"package"))
                .mount(server)
                .await;
        }
        WireReadBehavior::Range => {
            mount_range_head(server, Some("\"generation-a\"")).await;
            Mock::given(method("GET"))
                .and(header("If-Match", "\"generation-a\""))
                .respond_with(
                    ResponseTemplate::new(206)
                        .insert_header("Content-Range", "bytes 1-4/7")
                        .set_body_bytes(b"acka"),
                )
                .mount(server)
                .await;
        }
        WireReadBehavior::RangeGenerationChanged => {
            mount_range_head(server, Some("\"generation-a\"")).await;
            Mock::given(method("GET"))
                .and(header("If-Match", "\"generation-a\""))
                .respond_with(service_error(412, "PreconditionFailed"))
                .mount(server)
                .await;
        }
        WireReadBehavior::RangeTotalMismatch => {
            mount_range_head(server, Some("\"generation-a\"")).await;
            Mock::given(method("GET"))
                .and(header("If-Match", "\"generation-a\""))
                .respond_with(
                    ResponseTemplate::new(206)
                        .insert_header("Content-Range", "bytes 1-4/8")
                        .set_body_bytes(b"acka"),
                )
                .mount(server)
                .await;
        }
        WireReadBehavior::RangeMissingEtag => mount_range_head(server, None).await,
        WireReadBehavior::VerifyMismatch => {
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"corrupt"))
                .mount(server)
                .await;
        }
    }
}

async fn mount_range_head(server: &MockServer, etag: Option<&str>) {
    let mut response = ResponseTemplate::new(200).insert_header("Content-Length", "7");
    if let Some(etag) = etag {
        response = response.insert_header("ETag", etag);
    }
    Mock::given(method("HEAD")).respond_with(response).mount(server).await;
}

async fn mount_wire_writes(server: &MockServer, behavior: WireWriteBehavior) {
    match behavior {
        WireWriteBehavior::Delete => {
            Mock::given(method("HEAD"))
                .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "7"))
                .up_to_n_times(1)
                .with_priority(1)
                .mount(server)
                .await;
            Mock::given(method("HEAD"))
                .respond_with(
                    ResponseTemplate::new(404).set_body_raw("<Error><Code>NoSuchKey</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
            Mock::given(method("DELETE"))
                .respond_with(ResponseTemplate::new(204))
                .mount(server)
                .await;
        }
        WireWriteBehavior::Present => {
            Mock::given(method("HEAD"))
                .and(path(object_path(b"present")))
                .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "7"))
                .mount(server)
                .await;
            Mock::given(method("HEAD"))
                .and(path(object_path(b"missing")))
                .respond_with(
                    ResponseTemplate::new(404).set_body_raw("<Error><Code>NoSuchKey</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
        }
        WireWriteBehavior::SmallPut => {
            Mock::given(method("PUT"))
                .respond_with(ResponseTemplate::new(200).insert_header("ETag", "object"))
                .mount(server)
                .await;
        }
        WireWriteBehavior::Immutable => {
            Mock::given(method("PUT"))
                .respond_with(
                    ResponseTemplate::new(412)
                        .set_body_raw("<Error><Code>PreconditionFailed</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"existing"))
                .mount(server)
                .await;
        }
    }
}

async fn mount_wire_failures(server: &MockServer, behavior: WireFailureBehavior) {
    match behavior {
        WireFailureBehavior::Multipart => mount_multipart(server).await,
        WireFailureBehavior::Health => {
            Mock::given(method("GET"))
                .and(query_param("location", ""))
                .respond_with(
                    ResponseTemplate::new(403)
                        .set_body_raw("<Error><Code>AccessDenied</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
        }
        WireFailureBehavior::Head => {
            Mock::given(method("HEAD"))
                .respond_with(
                    ResponseTemplate::new(500)
                        .set_body_raw("<Error><Code>InternalError</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
        }
        WireFailureBehavior::HugeTimeout => {}
        WireFailureBehavior::Get => {
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(500)
                        .set_body_raw("<Error><Code>InternalError</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
        }
        WireFailureBehavior::GetMissingBucket => {
            Mock::given(method("GET"))
                .respond_with(service_error(404, "NoSuchBucket"))
                .mount(server)
                .await;
        }
        WireFailureBehavior::Put => {
            Mock::given(method("PUT"))
                .respond_with(
                    ResponseTemplate::new(500)
                        .set_body_raw("<Error><Code>InternalError</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
        }
        WireFailureBehavior::DeleteMissingBucket => {
            Mock::given(method("HEAD"))
                .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "7"))
                .mount(server)
                .await;
            Mock::given(method("DELETE"))
                .respond_with(service_error(404, "NoSuchBucket"))
                .mount(server)
                .await;
        }
        WireFailureBehavior::DeleteNotFound => {
            let missing =
                ResponseTemplate::new(404).set_body_raw("<Error><Code>NoSuchKey</Code></Error>", "application/xml");
            Mock::given(method("HEAD"))
                .respond_with(missing.clone())
                .mount(server)
                .await;
            Mock::given(method("DELETE")).respond_with(missing).mount(server).await;
        }
    }
}

async fn mount_wire_multipart_failures(server: &MockServer, behavior: WireMultipartFailureBehavior) {
    match behavior {
        WireMultipartFailureBehavior::Create => {
            Mock::given(method("POST"))
                .and(query_param("uploads", ""))
                .respond_with(service_error(500, "InternalError"))
                .mount(server)
                .await;
        }
        WireMultipartFailureBehavior::MissingUploadId => {
            Mock::given(method("POST"))
                .and(query_param("uploads", ""))
                .respond_with(ResponseTemplate::new(200).set_body_raw(
                    "<InitiateMultipartUploadResult></InitiateMultipartUploadResult>",
                    "application/xml",
                ))
                .mount(server)
                .await;
        }
        WireMultipartFailureBehavior::MissingPartEtag => {
            mount_multipart(server).await;
            Mock::given(method("PUT"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(ResponseTemplate::new(200))
                .up_to_n_times(1)
                .with_priority(1)
                .mount(server)
                .await;
        }
        WireMultipartFailureBehavior::MissingPartChecksum => {
            mount_multipart(server).await;
            Mock::given(method("PUT"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(ResponseTemplate::new(200).insert_header("ETag", "part"))
                .up_to_n_times(1)
                .with_priority(1)
                .mount(server)
                .await;
        }
        WireMultipartFailureBehavior::CompleteExists => {
            mount_multipart(server).await;
            Mock::given(method("POST"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(
                    ResponseTemplate::new(412)
                        .set_body_raw("<Error><Code>PreconditionFailed</Code></Error>", "application/xml"),
                )
                .up_to_n_times(1)
                .with_priority(1)
                .mount(server)
                .await;
        }
        WireMultipartFailureBehavior::Complete => {
            mount_multipart(server).await;
            Mock::given(method("POST"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(
                    ResponseTemplate::new(500)
                        .set_body_raw("<Error><Code>InternalError</Code></Error>", "application/xml"),
                )
                .with_priority(1)
                .mount(server)
                .await;
        }
        WireMultipartFailureBehavior::StaleUpload => {
            mount_multipart(server).await;
            Mock::given(method("PUT"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(
                    ResponseTemplate::new(404)
                        .set_body_raw("<Error><Code>NoSuchUpload</Code></Error>", "application/xml"),
                )
                .up_to_n_times(1)
                .with_priority(1)
                .mount(server)
                .await;
        }
        WireMultipartFailureBehavior::ConflictExhausted => {
            mount_multipart(server).await;
            Mock::given(method("POST"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(ResponseTemplate::new(409).set_body_raw(
                    "<Error><Code>ConditionalRequestConflict</Code></Error>",
                    "application/xml",
                ))
                .up_to_n_times(1)
                .with_priority(1)
                .mount(server)
                .await;
        }
    }
}

#[cfg(feature = "container-tests")]
fn admin_client(endpoint: &str) -> aws_sdk_s3::Client {
    aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .credentials_provider(Credentials::new(ROOT_ACCESS_KEY, ROOT_SECRET_KEY, None, None, "test"))
            .region(Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .build(),
    )
}

#[cfg(feature = "container-tests")]
async fn minio() -> Minio {
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        NEXT_CONTAINER.fetch_add(1, Ordering::Relaxed)
    );
    let network = format!("peryx-s3-{suffix}");
    let name = format!("peryx-minio-{suffix}");
    let container = GenericImage::new("minio/minio", "RELEASE.2025-04-22T22-12-26Z")
        .with_wait_for(WaitFor::message_on_stderr("API:"))
        .with_cmd(["server", "/data"])
        .with_env_var("MINIO_CONSOLE_ADDRESS", ":9001")
        .with_env_var("MINIO_ROOT_USER", ROOT_ACCESS_KEY)
        .with_env_var("MINIO_ROOT_PASSWORD", ROOT_SECRET_KEY)
        .with_network(&network)
        .with_container_name(&name)
        .start()
        .await
        .unwrap();
    let endpoint = format!(
        "http://127.0.0.1:{}",
        container.get_host_port_ipv4(9_000).await.unwrap()
    );
    admin_client(&endpoint)
        .create_bucket()
        .bucket(BUCKET)
        .send()
        .await
        .unwrap();
    Minio {
        _container: container,
        endpoint,
        network,
        name,
    }
}

#[cfg(feature = "container-tests")]
async fn run_mc(minio: &Minio, args: &[&str]) {
    let container = GenericImage::new("minio/mc", "RELEASE.2025-04-16T18-13-26Z")
        .with_wait_for(WaitFor::exit(ExitWaitStrategy::new().with_exit_code(0)))
        .with_network(&minio.network)
        .with_env_var(
            "MC_HOST_local",
            format!("http://{ROOT_ACCESS_KEY}:{ROOT_SECRET_KEY}@{}:9000", minio.name),
        )
        .with_cmd(args.iter().copied())
        .start()
        .await
        .unwrap();
    assert_eq!(container.exit_code().await.unwrap(), Some(0));
}

#[cfg(feature = "container-tests")]
async fn exec<Args, Item>(container: &ContainerAsync<GenericImage>, args: Args)
where
    Args: IntoIterator<Item = Item> + Send,
    Args::IntoIter: Send,
    Item: Into<String> + Send,
{
    container
        .exec(ExecCommand::new(args).with_cmd_ready_condition(CmdWaitFor::exit_code(0)))
        .await
        .unwrap();
}

#[cfg(feature = "container-tests")]
async fn toxiproxy(minio: &Minio) -> Toxiproxy {
    let container = GenericImage::new("ghcr.io/shopify/toxiproxy", "2.12.0")
        .with_wait_for(WaitFor::message_on_stdout("Starting Toxiproxy HTTP server"))
        .with_mapped_port(0, 8_666.tcp())
        .with_network(&minio.network)
        .with_cmd(["-host=0.0.0.0", "-proxy-metrics"])
        .start()
        .await
        .unwrap();
    exec(
        &container,
        [
            "/toxiproxy-cli".to_owned(),
            "create".to_owned(),
            "-l".to_owned(),
            "0.0.0.0:8666".to_owned(),
            "-u".to_owned(),
            format!("{}:9000", minio.name),
            "s3".to_owned(),
        ],
    )
    .await;
    Toxiproxy {
        endpoint: format!(
            "http://127.0.0.1:{}",
            container.get_host_port_ipv4(8_666.tcp()).await.unwrap()
        ),
        container,
    }
}

#[cfg(feature = "container-tests")]
#[tokio::test]
async fn test_s3_container_toxiproxy_publishes_only_the_data_port() {
    let minio = minio().await;
    let toxiproxy = toxiproxy(&minio).await;
    let ports = toxiproxy.container.ports().await.unwrap();
    let data_port = toxiproxy.endpoint.rsplit_once(':').unwrap().1.parse().unwrap();

    assert_eq!(
        (
            ports.map_to_host_port_ipv4(8_474.tcp()),
            ports.map_to_host_port_ipv4(8_666.tcp())
        ),
        (None, Some(data_port))
    );
}

fn child_command(endpoint: &str, staging: &Path, scenario: &str, access_key: &str, secret_key: &str) -> Command {
    let mut command = Command::new(s3_fixture());
    command
        .arg("integration")
        .arg(scenario)
        .arg(endpoint)
        .arg(staging)
        .env("AWS_ACCESS_KEY_ID", access_key)
        .env("AWS_SECRET_ACCESS_KEY", secret_key)
        .env("AWS_REGION", "us-east-1")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_SHARED_CREDENTIALS_FILE");
    command.kill_on_drop(true);
    command
}

async fn child(endpoint: &str, staging: &Path, scenario: &str, access_key: &str, secret_key: &str) -> Output {
    let slot = CHILD_SLOTS.acquire().await.unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        child_command(endpoint, staging, scenario, access_key, secret_key).output(),
    )
    .await
    .unwrap()
    .unwrap();
    drop(slot);
    output
}

fn assert_child_succeeded(output: &Output) {
    assert!(output.status.success(), "child failed: {output:?}");
}

#[rstest]
#[case::missing(WireBehavior::Missing)]
#[case::head(WireBehavior::Head)]
#[case::whole_read(WireBehavior::WholeRead)]
#[case::range(WireBehavior::Range)]
#[case::range_generation_changed(WireBehavior::RangeGenerationChanged)]
#[case::range_total_mismatch(WireBehavior::RangeTotalMismatch)]
#[case::range_missing_etag(WireBehavior::RangeMissingEtag)]
#[case::empty_range(WireBehavior::EmptyRange)]
#[case::verify(WireBehavior::Verify)]
#[case::verify_mismatch(WireBehavior::VerifyMismatch)]
#[case::materialize(WireBehavior::Materialize)]
#[case::delete(WireBehavior::Delete)]
#[case::present(WireBehavior::Present)]
#[case::small_put(WireBehavior::SmallPut)]
#[case::immutable(WireBehavior::Immutable)]
#[case::multipart(WireBehavior::Multipart)]
#[case::health_error(WireBehavior::HealthError)]
#[case::head_error(WireBehavior::HeadError)]
#[case::huge_timeout(WireBehavior::HugeTimeout)]
#[case::get_error(WireBehavior::GetError)]
#[case::get_missing_bucket(WireBehavior::GetMissingBucket)]
#[case::put_error(WireBehavior::PutError)]
#[case::delete_missing_bucket(WireBehavior::DeleteMissingBucket)]
#[case::delete_not_found(WireBehavior::DeleteNotFound)]
#[case::create_failure(WireBehavior::CreateFailure)]
#[case::create_missing_id(WireBehavior::CreateMissingId)]
#[case::part_missing_etag(WireBehavior::PartMissingEtag)]
#[case::part_missing_checksum(WireBehavior::PartMissingChecksum)]
#[case::complete_exists(WireBehavior::CompleteExists)]
#[case::complete_failure(WireBehavior::CompleteFailure)]
#[case::stale_upload(WireBehavior::StaleUpload)]
#[case::conflict_exhausted(WireBehavior::ConflictExhausted)]
#[tokio::test]
async fn test_s3_wire_behavior_uses_the_public_backend(#[case] behavior: WireBehavior) {
    let server = MockServer::start().await;
    mount_wire_behavior(&server, behavior).await;
    let staging = tempfile::tempdir().unwrap();
    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            behavior.scenario(),
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    if matches!(behavior, WireBehavior::CreateFailure) {
        let requests = server.received_requests().await.unwrap();
        assert_eq!(count_creates(&requests), 1);
        assert!(!requests.iter().any(|request| request.method.as_str() == "DELETE"));
        assert!(!multipart_journal(staging.path()).exists());
    }
}

#[test]
fn test_s3_reports_capabilities_through_the_public_backend() {
    let staging = tempfile::tempdir().unwrap();
    let storage = BlobStorage::s3(
        S3Config::new(settings("http://127.0.0.1:1".to_owned())).unwrap(),
        staging.path().to_owned(),
    );
    let capabilities = storage.capabilities();
    assert_eq!(capabilities.durability, BlobDurability::ObjectStore);
    assert_eq!(capabilities.durability.as_str(), "object-store");
    assert_eq!(capabilities.create_if_absent, BlobSupport::Native);
    assert_eq!(capabilities.checksum, BlobSupport::Emulated);
}

#[tokio::test]
async fn test_s3_sdk_retries_a_transient_bucket_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("location", ""))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_raw("<Error><Code>ServiceUnavailable</Code></Error>", "application/xml"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("location", ""))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<LocationConstraint xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"></LocationConstraint>",
            "application/xml",
        ))
        .mount(&server)
        .await;
    let staging = tempfile::tempdir().unwrap();

    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "health",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn test_s3_retries_a_conditional_whole_put_conflict() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(409).set_body_raw(
            "<Error><Code>ConditionalRequestConflict</Code></Error>",
            "application/xml",
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).insert_header("ETag", "object"))
        .mount(&server)
        .await;
    let staging = tempfile::tempdir().unwrap();

    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_small_put",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn test_s3_preserves_the_endpoint_base_path() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).insert_header("ETag", "object"))
        .expect(1)
        .mount(&server)
        .await;
    let staging = tempfile::tempdir().unwrap();

    assert_child_succeeded(
        &child(
            &format!("{}/api", server.uri()),
            staging.path(),
            "wire_small_put",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    assert_eq!(
        server.received_requests().await.unwrap()[0].url.path(),
        format!("/api{}", object_path(b"package"))
    );
}

#[tokio::test]
async fn test_s3_surfaces_a_truncated_response_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        assert_ne!(connection.read(&mut [0; 4096]).await.unwrap(), 0);
        connection
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nshort")
            .await
            .unwrap();
    });
    let staging = tempfile::tempdir().unwrap();

    assert_child_succeeded(
        &child(
            &endpoint,
            staging.path(),
            "wire_truncated_body",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    server.await.unwrap();
}

#[tokio::test]
async fn test_s3_times_out_before_response_headers() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (received, request) = tokio::sync::oneshot::channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        // Accept before the 500 ms request deadline can race the client.
        let (mut connection, _) = listener.accept().await.unwrap();
        received.send(()).unwrap();
        let _ = connection.read(&mut [0; 4096]).await;
        wait.await.unwrap();
        let _ = connection
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nlate")
            .await;
    });
    let staging = tempfile::tempdir().unwrap();
    let staging_path = staging.path().to_owned();
    let client = tokio::spawn(async move {
        child(
            &endpoint,
            &staging_path,
            "wire_send_timeout",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(30), request)
        .await
        .unwrap()
        .unwrap();
    let output = client.await.unwrap();
    release.send(()).unwrap();
    server.await.unwrap();
    assert_child_succeeded(&output);
}

#[derive(Clone, Copy)]
enum MissingLengthOperation {
    Head,
    Get,
}

impl MissingLengthOperation {
    const fn scenario(self) -> &'static str {
        match self {
            Self::Head => "wire_head_missing_length",
            Self::Get => "wire_get_missing_length",
        }
    }

    const fn response(self) -> &'static [u8] {
        match self {
            Self::Head => b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n",
            Self::Get => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n",
        }
    }
}

#[rstest]
#[case::head(MissingLengthOperation::Head)]
#[case::get(MissingLengthOperation::Get)]
#[tokio::test]
async fn test_s3_rejects_a_response_without_an_object_length(#[case] operation: MissingLengthOperation) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        assert_ne!(connection.read(&mut [0; 4096]).await.unwrap(), 0);
        connection.write_all(operation.response()).await.unwrap();
    });
    let staging = tempfile::tempdir().unwrap();

    assert_child_succeeded(
        &child(
            &endpoint,
            staging.path(),
            operation.scenario(),
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    server.await.unwrap();
}

#[tokio::test]
async fn test_s3_accepts_a_peer_completed_multipart_upload() {
    let server = MockServer::start().await;
    mount_multipart(&server).await;
    Mock::given(method("POST"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(service_error(404, "NoSuchUpload"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "20971521"))
        .mount(&server)
        .await;
    let staging = tempfile::tempdir().unwrap();

    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_parallel_multipart",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(count_creates(&requests), 1);
    assert!(!requests.iter().any(|request| request.method.as_str() == "DELETE"));
    assert!(!multipart_journal(staging.path()).exists());
}

#[tokio::test]
async fn test_s3_bounds_recovery_from_missing_multipart_uploads() {
    let server = MockServer::start().await;
    mount_multipart(&server).await;
    Mock::given(method("POST"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(service_error(404, "NoSuchUpload"))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .respond_with(service_error(404, "NoSuchKey"))
        .mount(&server)
        .await;
    let staging = tempfile::tempdir().unwrap();

    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_complete_failure",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(count_creates(&requests), 2);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method.as_str() == "DELETE")
            .count(),
        1
    );
    assert!(!multipart_journal(staging.path()).exists());
}

#[tokio::test]
async fn test_s3_cleans_up_when_peer_completion_status_cannot_be_checked() {
    let server = MockServer::start().await;
    mount_multipart(&server).await;
    Mock::given(method("POST"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(service_error(404, "NoSuchUpload"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .respond_with(service_error(500, "InternalError"))
        .mount(&server)
        .await;
    let staging = tempfile::tempdir().unwrap();

    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_complete_failure",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.method.as_str() == "DELETE")
            .count(),
        1
    );
    assert!(!multipart_journal(staging.path()).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_s3_drains_started_parts_before_aborting() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(create_response("upload-1"))
        .mount(&server)
        .await;
    let (part_started, mut part_started_rx) = tokio::sync::mpsc::unbounded_channel();
    let part_release = Arc::new(Barrier::new(2));
    let responder_release = Arc::clone(&part_release);
    Mock::given(method("PUT"))
        .and(query_param("uploadId", "upload-1"))
        .and(header("content-encoding", "aws-chunked"))
        .and(header("x-amz-trailer", "x-amz-checksum-sha256"))
        .respond_with(move |request: &Request| {
            if request
                .url
                .query_pairs()
                .any(|(key, value)| key == "partNumber" && value == "1")
            {
                service_error(400, "InvalidRequest")
            } else {
                part_started.send(()).unwrap();
                responder_release.wait();
                part_response()
            }
        })
        .mount(&server)
        .await;
    let (abort_started, mut abort_started_rx) = tokio::sync::mpsc::unbounded_channel();
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(move |_: &Request| {
            abort_started.send(()).unwrap();
            ResponseTemplate::new(204)
        })
        .mount(&server)
        .await;
    let staging = tempfile::tempdir().unwrap();
    let endpoint = server.uri();
    let staging_path = staging.path().to_owned();
    let process = tokio::spawn(async move {
        child(
            &endpoint,
            &staging_path,
            "wire_complete_failure",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(30), part_started_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        abort_started_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    );
    tokio::task::spawn_blocking(move || part_release.wait()).await.unwrap();
    assert_child_succeeded(&process.await.unwrap());
    tokio::time::timeout(Duration::from_secs(30), abort_started_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.method.as_str() == "DELETE")
            .count(),
        1
    );
}

#[tokio::test]
async fn test_s3_multipart_adopts_a_journal_created_by_another_process() {
    let server = MockServer::start().await;
    let staging = tempfile::tempdir().unwrap();
    let journal = multipart_journal(staging.path());
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(move |_: &Request| {
            std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
            std::fs::write(&journal, "upload-1").unwrap();
            create_response("upload-2")
        })
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", "upload-2"))
        .respond_with(ResponseTemplate::new(204))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    mount_multipart(&server).await;

    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_multipart",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    let requests = server.received_requests().await.unwrap();
    let count_upload_requests = |methods: &[&str], upload_id: &str| {
        requests
            .iter()
            .filter(|request| {
                methods.contains(&request.method.as_str())
                    && request
                        .url
                        .query_pairs()
                        .any(|(key, value)| key == "uploadId" && value == upload_id)
            })
            .count()
    };
    assert_eq!(
        (
            count_upload_requests(&["DELETE"], "upload-2"),
            count_upload_requests(&["PUT", "POST"], "upload-1"),
        ),
        (1, 3)
    );
}

#[tokio::test]
async fn test_s3_reports_an_abort_failure_after_losing_a_journal_race() {
    let server = MockServer::start().await;
    let staging = tempfile::tempdir().unwrap();
    let journal = multipart_journal(staging.path());
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(move |_: &Request| {
            std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
            std::fs::write(&journal, "upload-1").unwrap();
            create_response("upload-2")
        })
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", "upload-2"))
        .respond_with(service_error(500, "InternalError"))
        .mount(&server)
        .await;

    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_journal_failure",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
}

#[tokio::test]
async fn test_s3_reports_a_read_failure_after_losing_a_journal_race() {
    let server = MockServer::start().await;
    let staging = tempfile::tempdir().unwrap();
    let journal = multipart_journal(staging.path());
    let created_journal = journal.clone();
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(move |_: &Request| {
            std::fs::create_dir_all(created_journal.parent().unwrap()).unwrap();
            std::fs::write(&created_journal, "upload-1").unwrap();
            create_response("upload-2")
        })
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", "upload-2"))
        .respond_with(move |_: &Request| {
            std::fs::remove_file(&journal).unwrap();
            std::fs::create_dir(&journal).unwrap();
            ResponseTemplate::new(204)
        })
        .mount(&server)
        .await;

    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_journal_failure",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
}

#[tokio::test]
async fn test_s3_multipart_restarts_after_a_conditional_conflict() {
    let server = MockServer::start().await;
    mount_multipart(&server).await;
    Mock::given(method("POST"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(ResponseTemplate::new(409).set_body_raw(
            "<Error><Code>ConditionalRequestConflict</Code><Message>race</Message></Error>",
            "application/xml",
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    let staging = tempfile::tempdir().unwrap();

    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_conflict",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(count_creates(&requests), 2);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method.as_str() == "DELETE")
            .count(),
        1
    );
}

#[tokio::test]
async fn test_s3_failed_abort_preserves_the_upload_for_recovery() {
    let server = MockServer::start().await;
    mount_multipart(&server).await;
    Mock::given(method("PUT"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(ResponseTemplate::new(500).set_body_raw(
            "<Error><Code>InternalError</Code><Message>part failed</Message></Error>",
            "application/xml",
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(ResponseTemplate::new(500).set_body_raw(
            "<Error><Code>InternalError</Code><Message>abort failed</Message></Error>",
            "application/xml",
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    let staging = tempfile::tempdir().unwrap();

    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_abort_failure",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    assert_eq!(count_creates(&server.received_requests().await.unwrap()), 1);
}

#[cfg(feature = "container-tests")]
struct InterruptedUpload {
    minio: Minio,
    toxiproxy: Toxiproxy,
    staging: tempfile::TempDir,
    key: String,
    upload_id: String,
}

#[cfg(feature = "container-tests")]
async fn interrupted_upload() -> InterruptedUpload {
    let minio = minio().await;
    let toxiproxy = toxiproxy(&minio).await;
    exec(
        &toxiproxy.container,
        [
            "/toxiproxy-cli",
            "toxic",
            "add",
            "-t",
            "bandwidth",
            "-u",
            "-a",
            "rate=64",
            "s3",
        ],
    )
    .await;
    let staging = tempfile::tempdir().unwrap();
    let mut process = child_command(
        &toxiproxy.endpoint,
        staging.path(),
        "cancel",
        ROOT_ACCESS_KEY,
        ROOT_SECRET_KEY,
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    process.stdout = Some(wait_for_child_signal(&mut process, JOURNAL_WRITTEN).await);
    let upload = admin_client(&minio.endpoint)
        .list_multipart_uploads()
        .bucket(BUCKET)
        .send()
        .await
        .unwrap()
        .uploads
        .unwrap_or_default()
        .into_iter()
        .next()
        .unwrap();
    let key = upload.key.unwrap();
    let upload_id = upload.upload_id.unwrap();
    exec(
        &toxiproxy.container,
        [
            "/toxiproxy-cli",
            "toxic",
            "add",
            "-t",
            "timeout",
            "-u",
            "-a",
            "timeout=0",
            "s3",
        ],
    )
    .await;
    exec(
        &toxiproxy.container,
        ["/toxiproxy-cli", "toxic", "remove", "-n", "bandwidth_upstream", "s3"],
    )
    .await;
    process.stdin.take().unwrap().write_all(b"cancel").await.unwrap();
    assert_child_succeeded(
        &tokio::time::timeout(Duration::from_secs(30), process.wait_with_output())
            .await
            .unwrap()
            .unwrap(),
    );
    assert!(
        admin_client(&minio.endpoint)
            .list_parts()
            .bucket(BUCKET)
            .key(&key)
            .upload_id(&upload_id)
            .send()
            .await
            .unwrap()
            .parts
            .unwrap_or_default()
            .is_empty()
    );
    InterruptedUpload {
        minio,
        toxiproxy,
        staging,
        key,
        upload_id,
    }
}

#[cfg(feature = "container-tests")]
async fn remove_timeout(upload: &InterruptedUpload) {
    exec(
        &upload.toxiproxy.container,
        ["/toxiproxy-cli", "toxic", "remove", "-n", "timeout_upstream", "s3"],
    )
    .await;
}

#[cfg(feature = "container-tests")]
async fn resume_upload(upload: &InterruptedUpload) {
    remove_timeout(upload).await;
    assert_child_succeeded(
        &child(
            &upload.toxiproxy.endpoint,
            upload.staging.path(),
            "multipart",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
}

#[cfg(feature = "container-tests")]
async fn abort_upload(upload: &InterruptedUpload) {
    admin_client(&upload.minio.endpoint)
        .abort_multipart_upload()
        .bucket(BUCKET)
        .key(&upload.key)
        .upload_id(&upload.upload_id)
        .send()
        .await
        .unwrap();
}

#[cfg(feature = "container-tests")]
async fn wait_for_signal<R: AsyncRead + Unpin>(reader: R, expected: &str) -> std::io::Result<R> {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    while reader.read_line(&mut line).await? != 0 {
        if line.trim_end() == expected {
            return Ok(reader.into_inner());
        }
        line.clear();
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        format!("child exited before signaling {expected}"),
    ))
}

#[cfg(feature = "container-tests")]
#[tokio::test]
async fn test_wait_for_signal_skips_other_events() {
    assert!(
        wait_for_signal(&b"OTHER_EVENT\nEXPECTED_EVENT\n"[..], "EXPECTED_EVENT")
            .await
            .is_ok()
    );
}

#[cfg(unix)]
#[test]
fn test_s3_fixture_rejects_a_non_utf8_command() {
    use std::os::unix::ffi::OsStringExt as _;

    let output = std::process::Command::new(s3_fixture())
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("command is not valid UTF-8"));
}

#[test]
fn test_s3_fixture_rejects_an_incomplete_unit_scenario() {
    let output = std::process::Command::new(s3_fixture())
        .arg("unit")
        .arg("scenario")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("unit scenarios require name, endpoint, and staging directory")
    );
}

#[test]
fn test_s3_fixture_requires_a_command() {
    let output = std::process::Command::new(s3_fixture()).output().unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing command"));
}

#[test]
fn test_s3_fixture_rejects_an_invalid_unit_config() {
    let output = std::process::Command::new(s3_fixture())
        .args(["unit", "health", "not a url"])
        .arg(std::env::temp_dir())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("s3 endpoint is not a valid URL"));
}

#[cfg(feature = "container-tests")]
#[test]
fn test_s3_fixture_rejects_an_invalid_integration_config() {
    let output = std::process::Command::new(s3_fixture())
        .args(["integration", "cancel", "not a url"])
        .arg(std::env::temp_dir())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("s3 endpoint is not a valid URL"));
}

#[cfg(feature = "container-tests")]
#[tokio::test]
async fn test_wait_for_child_signal_reports_child_stderr() {
    let staging = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let mut process = child_command(
        &server.uri(),
        staging.path(),
        "unknown",
        ROOT_ACCESS_KEY,
        ROOT_SECRET_KEY,
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

    let error = tokio::spawn(async move { wait_for_child_signal(&mut process, STREAM_OPENED).await })
        .await
        .unwrap_err();

    assert!(error.to_string().contains("unknown S3 scenario: unknown"));
}

#[cfg(feature = "container-tests")]
async fn wait_for_child_signal(process: &mut tokio::process::Child, expected: &str) -> tokio::process::ChildStdout {
    match tokio::time::timeout(
        Duration::from_secs(30),
        wait_for_signal(process.stdout.take().unwrap(), expected),
    )
    .await
    .unwrap()
    {
        Ok(stdout) => stdout,
        Err(error) => {
            let mut stderr = Vec::new();
            process.stderr.take().unwrap().read_to_end(&mut stderr).await.unwrap();
            let status = process.wait().await.unwrap();
            panic!(
                "{error}; child status: {status}; stderr: {}",
                String::from_utf8_lossy(&stderr)
            );
        }
    }
}

#[cfg(feature = "container-tests")]
async fn trickling_stream() {
    let minio = minio().await;
    let bytes = vec![0x5a; STREAM_BYTES];
    let digest = Digest::of(&bytes);
    let config = S3Config::new(settings(minio.endpoint.clone())).unwrap();
    admin_client(&minio.endpoint)
        .put_object()
        .bucket(BUCKET)
        .key(config.key_for(digest.as_str()))
        .body(ByteStream::from(bytes))
        .send()
        .await
        .unwrap();
    let toxiproxy = toxiproxy(&minio).await;
    exec(
        &toxiproxy.container,
        [
            "/toxiproxy-cli",
            "toxic",
            "add",
            "-t",
            "slicer",
            "-a",
            "average_size=32768",
            "-a",
            "size_variation=0",
            "-a",
            "delay=20000",
            "s3",
        ],
    )
    .await;
    let staging = tempfile::tempdir().unwrap();
    collect_stream(opened_stream_child(&toxiproxy.endpoint, staging.path(), "stream_trickle").await).await;
    drop(minio);
}

#[cfg(feature = "container-tests")]
async fn interrupted_stream(interruption: StreamInterruption) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (release, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_interrupted_stream(listener, wait, interruption));
    let staging = tempfile::tempdir().unwrap();
    let process = opened_stream_child(&endpoint, staging.path(), interruption.scenario()).await;

    release.send(()).unwrap();
    match interruption {
        StreamInterruption::Closed => {
            server.await.unwrap();
            collect_stream(process).await;
        }
        StreamInterruption::Stalled => {
            collect_stream(process).await;
            server.await.unwrap();
        }
    }
}

#[cfg(feature = "container-tests")]
async fn opened_stream_child(endpoint: &str, staging: &Path, scenario: &str) -> tokio::process::Child {
    let mut process = child_command(endpoint, staging, scenario, ROOT_ACCESS_KEY, ROOT_SECRET_KEY)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    process.stdout = Some(wait_for_child_signal(&mut process, STREAM_OPENED).await);
    process
}

#[cfg(feature = "container-tests")]
async fn collect_stream(mut process: tokio::process::Child) {
    process.stdin.take().unwrap().write_all(b"collect").await.unwrap();
    assert_child_succeeded(
        &tokio::time::timeout(Duration::from_secs(30), process.wait_with_output())
            .await
            .unwrap()
            .unwrap(),
    );
}

#[cfg(feature = "container-tests")]
async fn serve_interrupted_stream(
    listener: tokio::net::TcpListener,
    wait: tokio::sync::oneshot::Receiver<()>,
    interruption: StreamInterruption,
) {
    let (mut connection, _) = listener.accept().await.unwrap();
    assert_ne!(connection.read(&mut [0; 4096]).await.unwrap(), 0);
    connection
        .write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {STREAM_BYTES}\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    wait.await.unwrap();
    if matches!(interruption, StreamInterruption::Stalled) {
        assert_eq!(connection.read(&mut [0; 1]).await.unwrap(), 0);
    }
}

#[cfg(feature = "container-tests")]
#[derive(Clone, Copy)]
enum StreamInterruption {
    Closed,
    Stalled,
}

#[cfg(feature = "container-tests")]
impl StreamInterruption {
    const fn scenario(self) -> &'static str {
        match self {
            Self::Closed => "stream_reset",
            Self::Stalled => "stream_timeout",
        }
    }
}

#[cfg(feature = "container-tests")]
#[tokio::test]
async fn test_s3_container_uses_the_default_credential_chain() {
    let minio = minio().await;
    let staging = tempfile::tempdir().unwrap();
    assert_child_succeeded(
        &child(
            &minio.endpoint,
            staging.path(),
            "health",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    drop(minio);
}

#[cfg(feature = "container-tests")]
#[tokio::test]
async fn test_s3_container_surfaces_invalid_credentials() {
    let minio = minio().await;
    let staging = tempfile::tempdir().unwrap();
    assert_child_succeeded(&child(&minio.endpoint, staging.path(), "invalid", "invalid", "invalid-secret").await);
    drop(minio);
}

#[cfg(feature = "container-tests")]
#[tokio::test]
async fn test_s3_container_surfaces_a_valid_readonly_principal() {
    let minio = minio().await;
    run_mc(
        &minio,
        &[
            "admin",
            "user",
            "add",
            "local",
            READONLY_ACCESS_KEY,
            READONLY_SECRET_KEY,
        ],
    )
    .await;
    run_mc(
        &minio,
        &[
            "admin",
            "policy",
            "attach",
            "local",
            "readonly",
            "--user",
            READONLY_ACCESS_KEY,
        ],
    )
    .await;
    let staging = tempfile::tempdir().unwrap();
    assert_child_succeeded(
        &child(
            &minio.endpoint,
            staging.path(),
            "readonly",
            READONLY_ACCESS_KEY,
            READONLY_SECRET_KEY,
        )
        .await,
    );
    drop(minio);
}

#[tokio::test]
async fn test_s3_retries_after_a_truncated_response() {
    let location = "<LocationConstraint xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"></LocationConstraint>";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        assert_ne!(connection.read(&mut [0; 4096]).await.unwrap(), 0);
        connection
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 512\r\nConnection: close\r\n\r\n<LocationConstraint>")
            .await
            .unwrap();
        drop(connection);
        let (mut connection, _) = listener.accept().await.unwrap();
        assert_ne!(connection.read(&mut [0; 4096]).await.unwrap(), 0);
        connection
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{location}",
                    location.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let staging = tempfile::tempdir().unwrap();

    assert_child_succeeded(&child(&endpoint, staging.path(), "health", ROOT_ACCESS_KEY, ROOT_SECRET_KEY).await);
    server.await.unwrap();
}

#[cfg(feature = "container-tests")]
#[tokio::test]
async fn test_s3_container_resumes_a_cancelled_multipart_upload() {
    resume_upload(&interrupted_upload().await).await;
}

#[cfg(feature = "container-tests")]
#[tokio::test]
async fn test_s3_container_restarts_a_stale_multipart_journal() {
    let upload = interrupted_upload().await;
    abort_upload(&upload).await;
    resume_upload(&upload).await;
}

#[derive(Clone, Copy)]
enum JournalDamage {
    Empty,
    NonUtf8,
    Oversized,
}

#[rstest]
#[case::empty(JournalDamage::Empty)]
#[case::non_utf8(JournalDamage::NonUtf8)]
#[case::oversized(JournalDamage::Oversized)]
#[tokio::test]
async fn test_s3_replaces_a_malformed_multipart_journal(#[case] damage: JournalDamage) {
    let server = MockServer::start().await;
    mount_multipart(&server).await;
    let staging = tempfile::tempdir().unwrap();
    let journal = multipart_journal(staging.path());
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    std::fs::write(
        journal,
        match damage {
            JournalDamage::Empty => Vec::new(),
            JournalDamage::NonUtf8 => vec![0xff],
            JournalDamage::Oversized => vec![b'x'; 4_097],
        },
    )
    .unwrap();
    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_multipart",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
}

#[tokio::test]
async fn test_s3_reports_an_unreadable_multipart_journal() {
    let server = MockServer::start().await;
    let staging = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(multipart_journal(staging.path())).unwrap();
    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_journal_failure",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
}

#[tokio::test]
async fn test_s3_reports_a_multipart_journal_parent_race() {
    let server = MockServer::start().await;
    let staging = tempfile::tempdir().unwrap();
    let blocker = staging.path().join("s3-multipart");
    // Replace the journal after create to make the write failure independent of timing.
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(move |_: &Request| {
            std::fs::write(&blocker, []).unwrap();
            create_response("upload-1")
        })
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    mount_multipart(&server).await;

    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_journal_failure",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_s3_reports_an_unwritable_multipart_journal_directory() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = MockServer::start().await;
    mount_multipart(&server).await;
    let staging = tempfile::tempdir().unwrap();
    let directory = staging.path().join("s3-multipart");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500)).unwrap();
    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_journal_failure",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn test_s3_reports_a_multipart_journal_removal_failure() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = MockServer::start().await;
    mount_multipart(&server).await;
    let staging = tempfile::tempdir().unwrap();
    let journal = multipart_journal(staging.path());
    let directory = journal.parent().unwrap();
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(&journal, "upload-1").unwrap();
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o500)).unwrap();
    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_journal_failure",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[derive(Clone, Copy)]
enum JournalCleanupPoint {
    StalePart,
    Conflict,
    MissingCompletion,
}

#[rstest]
#[case::stale_part(JournalCleanupPoint::StalePart)]
#[case::conflict(JournalCleanupPoint::Conflict)]
#[case::missing_completion(JournalCleanupPoint::MissingCompletion)]
#[tokio::test]
async fn test_s3_reports_a_structural_journal_cleanup_failure(#[case] point: JournalCleanupPoint) {
    let server = MockServer::start().await;
    mount_multipart(&server).await;
    let staging = tempfile::tempdir().unwrap();
    let journal = multipart_journal(staging.path());
    std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
    std::fs::write(&journal, "upload-1").unwrap();
    let failed_journal = journal.clone();
    let fail_cleanup = move || {
        std::fs::remove_file(&failed_journal).unwrap();
        std::fs::create_dir(&failed_journal).unwrap();
    };
    match point {
        JournalCleanupPoint::StalePart => {
            Mock::given(method("PUT"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(move |_: &Request| {
                    fail_cleanup();
                    service_error(404, "NoSuchUpload")
                })
                .up_to_n_times(1)
                .with_priority(1)
                .mount(&server)
                .await;
        }
        JournalCleanupPoint::Conflict => {
            Mock::given(method("POST"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(move |_: &Request| {
                    fail_cleanup();
                    service_error(409, "ConditionalRequestConflict")
                })
                .up_to_n_times(1)
                .with_priority(1)
                .mount(&server)
                .await;
        }
        JournalCleanupPoint::MissingCompletion => {
            Mock::given(method("POST"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(move |_: &Request| {
                    fail_cleanup();
                    service_error(404, "NoSuchUpload")
                })
                .up_to_n_times(1)
                .with_priority(1)
                .mount(&server)
                .await;
            Mock::given(method("HEAD"))
                .respond_with(service_error(404, "NoSuchKey"))
                .mount(&server)
                .await;
        }
    }

    assert_child_succeeded(
        &child(
            &server.uri(),
            staging.path(),
            "wire_journal_failure",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
}

#[cfg(feature = "container-tests")]
#[tokio::test]
async fn test_s3_container_coordinates_simultaneous_multipart_acquisition() {
    let minio = minio().await;
    let staging = tempfile::tempdir().unwrap();
    assert_child_succeeded(
        &child(
            &minio.endpoint,
            staging.path(),
            "concurrent",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await,
    );
    drop(minio);
}

#[cfg(feature = "container-tests")]
#[rstest]
#[case::closed(StreamInterruption::Closed)]
#[case::stalled(StreamInterruption::Stalled)]
#[tokio::test]
async fn test_s3_stream_interruptions(#[case] interruption: StreamInterruption) {
    interrupted_stream(interruption).await;
}

#[cfg(feature = "container-tests")]
#[tokio::test]
async fn test_s3_container_accepts_a_trickling_stream() {
    trickling_stream().await;
}

#[tokio::test]
async fn test_child_dispatch_rejects_unknown_scenarios() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let output = child(&server.uri(), dir.path(), "unknown", ROOT_ACCESS_KEY, ROOT_SECRET_KEY).await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown S3 scenario: unknown"));
}

#[cfg(feature = "container-tests")]
#[tokio::test]
async fn test_wait_for_signal_reports_eof() {
    let error = wait_for_signal(&b""[..], STREAM_OPENED).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    assert_eq!(error.to_string(), "child exited before signaling PERYX_STREAM_OPENED");
}

fn s3_fixture() -> PathBuf {
    PathBuf::from(std::env::var_os("CARGO_BIN_EXE_peryx-storage-s3-fixture").expect("Cargo S3 fixture binary"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicScenario {
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

const PUBLIC_SCENARIOS: [PublicScenario; 39] = [
    PublicScenario::Health,
    PublicScenario::Put,
    PublicScenario::Head,
    PublicScenario::WholeRead,
    PublicScenario::Range,
    PublicScenario::EmptyRange,
    PublicScenario::InvalidRange,
    PublicScenario::RangeMissing,
    PublicScenario::RangeError,
    PublicScenario::Verify,
    PublicScenario::VerifyMissing,
    PublicScenario::VerifyError,
    PublicScenario::Materialize,
    PublicScenario::MaterializeMissing,
    PublicScenario::MaterializeError,
    PublicScenario::Present,
    PublicScenario::Delete,
    PublicScenario::DeleteHeadError,
    PublicScenario::DeleteError,
    PublicScenario::Multipart,
    PublicScenario::AbortMissing,
    PublicScenario::PutMissingStage,
    PublicScenario::MultipartMissingStage,
    PublicScenario::BeginError,
    PublicScenario::WriteFlush,
    PublicScenario::WriteTail,
    PublicScenario::WriteCommit,
    PublicScenario::WriteAbort,
    PublicScenario::StagedLen,
    PublicScenario::StagedEmpty,
    PublicScenario::StagedMaterialized,
    PublicScenario::StagedAbort,
    PublicScenario::BlockingStage,
    PublicScenario::BlockingHead,
    PublicScenario::BlockingRead,
    PublicScenario::BlockingMaterialize,
    PublicScenario::BlockingVerify,
    PublicScenario::BlockingDelete,
    PublicScenario::BlockingVisit,
];

impl PublicScenario {
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

async fn mount_public(server: &MockServer, scenario: PublicScenario) {
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
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "7")
                .insert_header("ETag", "\"generation-a\""),
        )
        .with_priority(2)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(header("Range", "bytes=1-4"))
        .and(header("If-Match", "\"generation-a\""))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "Bytes 1-4/7")
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
    mount_public_scenario(server, scenario).await;
}

async fn mount_public_scenario(server: &MockServer, scenario: PublicScenario) {
    match scenario {
        PublicScenario::RangeMissing => {
            Mock::given(method("HEAD"))
                .respond_with(service_error(404, "NoSuchKey"))
                .with_priority(1)
                .mount(server)
                .await;
        }
        PublicScenario::RangeError | PublicScenario::DeleteHeadError => {
            Mock::given(method("HEAD"))
                .respond_with(service_error(500, "InternalError"))
                .with_priority(1)
                .mount(server)
                .await;
        }
        PublicScenario::VerifyMissing | PublicScenario::MaterializeMissing => {
            Mock::given(method("GET"))
                .respond_with(service_error(404, "NoSuchKey"))
                .with_priority(1)
                .mount(server)
                .await;
        }
        PublicScenario::VerifyError | PublicScenario::MaterializeError => {
            Mock::given(method("GET"))
                .respond_with(service_error(500, "InternalError"))
                .with_priority(1)
                .mount(server)
                .await;
        }
        PublicScenario::DeleteError => {
            Mock::given(method("DELETE"))
                .respond_with(service_error(500, "InternalError"))
                .with_priority(1)
                .mount(server)
                .await;
        }
        PublicScenario::AbortMissing => {
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

#[tokio::test]
async fn test_s3_public_surface_in_fixture() {
    for scenario in PUBLIC_SCENARIOS {
        let server = MockServer::start().await;
        mount_public(&server, scenario).await;
        let staging = tempfile::tempdir().unwrap();
        let staging_path = if scenario == PublicScenario::BeginError {
            let blocked = staging.path().join("blocked");
            std::fs::write(&blocked, []).unwrap();
            blocked
        } else {
            staging.path().to_owned()
        };
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            Command::new(s3_fixture())
                .arg("unit")
                .arg(scenario.name())
                .arg(server.uri())
                .arg(staging_path)
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
        assert!(output.status.success());
    }
}

#[tokio::test]
async fn test_filesystem_surface_in_fixture() {
    assert!(
        Command::new(s3_fixture())
            .arg("filesystem")
            .output()
            .await
            .unwrap()
            .status
            .success()
    );
}

#[tokio::test]
async fn test_s3_fixture_rejects_unknown_commands() {
    let output = Command::new(s3_fixture()).arg("unknown").output().await.unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown S3 fixture command: unknown"));
}

#[tokio::test]
async fn test_s3_fixture_rejects_unknown_unit_scenarios() {
    let staging = tempfile::tempdir().unwrap();
    let output = Command::new(s3_fixture())
        .args(["unit", "unknown", "http://127.0.0.1:1"])
        .arg(staging.path())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown unit S3 scenario: unknown"));
}
