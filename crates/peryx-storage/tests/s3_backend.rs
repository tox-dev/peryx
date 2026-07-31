use std::path::{Path, PathBuf};
use std::process::Output;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use std::{io::Read as _, io::stdin};

use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use peryx_storage::blob::{BlobDurability, BlobErrorKind, BlobStorage, BlobSupport, Digest, S3Config, S3Settings};
use rstest::rstest;
use testcontainers::core::wait::{ExitWaitStrategy, HttpWaitStrategy};
use testcontainers::core::{CmdWaitFor, ExecCommand, ImageExt as _, IntoContainerPort as _, WaitFor};
use testcontainers::runners::AsyncRunner as _;
use testcontainers::{ContainerAsync, GenericImage};
use testcontainers_modules::minio::MinIO;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const BUCKET: &str = "peryx-tests";
const ROOT_ACCESS_KEY: &str = "peryx-minio";
const ROOT_SECRET_KEY: &str = "peryx-minio-secret";
const READONLY_ACCESS_KEY: &str = "peryx-readonly";
const READONLY_SECRET_KEY: &str = "peryx-readonly-secret";
const CHILD_SCENARIO: &str = "PERYX_S3_CHILD_SCENARIO";
const ENDPOINT: &str = "PERYX_S3_TEST_ENDPOINT";
const STAGING_DIR: &str = "PERYX_S3_TEST_STAGING_DIR";
const RUN_CONTAINERS: &str = "PERYX_RUN_CONTAINER_TESTS";
const STREAM_BYTES: usize = 8 << 20;
static NEXT_CONTAINER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
enum MultipartStage {
    Create,
    Part,
    Complete,
}

impl MultipartStage {
    fn matches(self, request: &Request) -> bool {
        let query: std::collections::HashMap<_, _> = request.url.query_pairs().collect();
        match self {
            Self::Create => request.method.as_str() == "POST" && query.contains_key("uploads"),
            Self::Part => request.method.as_str() == "PUT" && query.contains_key("partNumber"),
            Self::Complete => request.method.as_str() == "POST" && query.contains_key("uploadId"),
        }
    }
}

#[derive(Clone, Copy)]
enum WireBehavior {
    Missing,
    Head,
    WholeRead,
    Range,
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
    PutError,
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

impl WireBehavior {
    const fn scenario(self) -> &'static str {
        match self {
            Self::Missing => "wire_missing",
            Self::Head => "wire_head",
            Self::WholeRead => "wire_whole_read",
            Self::Range => "wire_range",
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
            Self::PutError => "wire_put_error",
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

struct Minio {
    _container: ContainerAsync<MinIO>,
    endpoint: String,
    network: String,
    name: String,
}

struct Toxiproxy {
    container: ContainerAsync<GenericImage>,
    endpoint: String,
    metrics: String,
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

async fn mount_multipart(server: &MockServer, delayed: Option<MultipartStage>) {
    let delay = Duration::from_millis(250);
    if matches!(delayed, Some(MultipartStage::Create)) {
        Mock::given(method("POST"))
            .and(query_param("uploads", ""))
            .respond_with(create_response("upload-1").set_delay(delay))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(query_param("uploads", ""))
        .respond_with(create_response("upload-1"))
        .mount(server)
        .await;
    if matches!(delayed, Some(MultipartStage::Part)) {
        Mock::given(method("PUT"))
            .and(query_param("uploadId", "upload-1"))
            .and(header("content-encoding", "aws-chunked"))
            .and(header("x-amz-trailer", "x-amz-checksum-sha256"))
            .respond_with(part_response().set_delay(delay))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(server)
            .await;
    }
    Mock::given(method("PUT"))
        .and(query_param("uploadId", "upload-1"))
        .and(header("content-encoding", "aws-chunked"))
        .and(header("x-amz-trailer", "x-amz-checksum-sha256"))
        .respond_with(part_response())
        .mount(server)
        .await;
    if matches!(delayed, Some(MultipartStage::Complete)) {
        Mock::given(method("POST"))
            .and(query_param("uploadId", "upload-1"))
            .respond_with(complete_response().set_delay(delay))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(server)
            .await;
    }
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

async fn wait_for_stage(server: &MockServer, stage: MultipartStage) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if server
                .received_requests()
                .await
                .is_some_and(|requests| requests.iter().any(|request| stage.matches(request)))
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn count_stage(requests: &[Request], stage: MultipartStage) -> usize {
    requests.iter().filter(|request| stage.matches(request)).count()
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
        WireBehavior::Missing
        | WireBehavior::Head
        | WireBehavior::WholeRead
        | WireBehavior::Range
        | WireBehavior::EmptyRange
        | WireBehavior::Verify
        | WireBehavior::VerifyMismatch
        | WireBehavior::Materialize => mount_wire_reads(server, behavior).await,
        WireBehavior::Delete | WireBehavior::Present | WireBehavior::SmallPut | WireBehavior::Immutable => {
            mount_wire_writes(server, behavior).await;
        }
        WireBehavior::Multipart
        | WireBehavior::HealthError
        | WireBehavior::HeadError
        | WireBehavior::HugeTimeout
        | WireBehavior::GetError
        | WireBehavior::PutError
        | WireBehavior::DeleteNotFound => mount_wire_failures(server, behavior).await,
        WireBehavior::CreateFailure
        | WireBehavior::CreateMissingId
        | WireBehavior::PartMissingEtag
        | WireBehavior::PartMissingChecksum
        | WireBehavior::CompleteExists
        | WireBehavior::CompleteFailure
        | WireBehavior::StaleUpload
        | WireBehavior::ConflictExhausted => mount_wire_multipart_failures(server, behavior).await,
    }
}

async fn mount_wire_reads(server: &MockServer, behavior: WireBehavior) {
    match behavior {
        WireBehavior::Missing => {
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(404).set_body_raw("<Error><Code>NoSuchKey</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
        }
        WireBehavior::Head | WireBehavior::EmptyRange => {
            Mock::given(method("HEAD"))
                .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "7"))
                .mount(server)
                .await;
        }
        WireBehavior::WholeRead | WireBehavior::Verify | WireBehavior::Materialize => {
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"package"))
                .mount(server)
                .await;
        }
        WireBehavior::Range => {
            Mock::given(method("HEAD"))
                .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "7"))
                .mount(server)
                .await;
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(206)
                        .insert_header("Content-Range", "bytes 1-4/7")
                        .set_body_bytes(b"acka"),
                )
                .mount(server)
                .await;
        }
        WireBehavior::VerifyMismatch => {
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(b"corrupt"))
                .mount(server)
                .await;
        }
        _ => unreachable!(),
    }
}

async fn mount_wire_writes(server: &MockServer, behavior: WireBehavior) {
    match behavior {
        WireBehavior::Delete => {
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
        WireBehavior::Present => {
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
        WireBehavior::SmallPut => {
            Mock::given(method("PUT"))
                .respond_with(ResponseTemplate::new(200).insert_header("ETag", "object"))
                .mount(server)
                .await;
        }
        WireBehavior::Immutable => {
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
        _ => unreachable!(),
    }
}

async fn mount_wire_failures(server: &MockServer, behavior: WireBehavior) {
    match behavior {
        WireBehavior::Multipart => mount_multipart(server, None).await,
        WireBehavior::HealthError => {
            Mock::given(method("GET"))
                .and(query_param("location", ""))
                .respond_with(
                    ResponseTemplate::new(403)
                        .set_body_raw("<Error><Code>AccessDenied</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
        }
        WireBehavior::HeadError => {
            Mock::given(method("HEAD"))
                .respond_with(
                    ResponseTemplate::new(500)
                        .set_body_raw("<Error><Code>InternalError</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
        }
        WireBehavior::HugeTimeout => {}
        WireBehavior::GetError => {
            Mock::given(method("GET"))
                .respond_with(
                    ResponseTemplate::new(500)
                        .set_body_raw("<Error><Code>InternalError</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
        }
        WireBehavior::PutError => {
            Mock::given(method("PUT"))
                .respond_with(
                    ResponseTemplate::new(500)
                        .set_body_raw("<Error><Code>InternalError</Code></Error>", "application/xml"),
                )
                .mount(server)
                .await;
        }
        WireBehavior::DeleteNotFound => {
            let missing =
                ResponseTemplate::new(404).set_body_raw("<Error><Code>NoSuchKey</Code></Error>", "application/xml");
            Mock::given(method("HEAD"))
                .respond_with(missing.clone())
                .mount(server)
                .await;
            Mock::given(method("DELETE")).respond_with(missing).mount(server).await;
        }
        _ => unreachable!(),
    }
}

async fn mount_wire_multipart_failures(server: &MockServer, behavior: WireBehavior) {
    match behavior {
        WireBehavior::CreateFailure => {
            Mock::given(method("POST"))
                .and(query_param("uploads", ""))
                .respond_with(service_error(500, "InternalError"))
                .mount(server)
                .await;
        }
        WireBehavior::CreateMissingId => {
            Mock::given(method("POST"))
                .and(query_param("uploads", ""))
                .respond_with(ResponseTemplate::new(200).set_body_raw(
                    "<InitiateMultipartUploadResult></InitiateMultipartUploadResult>",
                    "application/xml",
                ))
                .mount(server)
                .await;
        }
        WireBehavior::PartMissingEtag => {
            mount_multipart(server, None).await;
            Mock::given(method("PUT"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(ResponseTemplate::new(200))
                .up_to_n_times(1)
                .with_priority(1)
                .mount(server)
                .await;
        }
        WireBehavior::PartMissingChecksum => {
            mount_multipart(server, None).await;
            Mock::given(method("PUT"))
                .and(query_param("uploadId", "upload-1"))
                .respond_with(ResponseTemplate::new(200).insert_header("ETag", "part"))
                .up_to_n_times(1)
                .with_priority(1)
                .mount(server)
                .await;
        }
        WireBehavior::CompleteExists => {
            mount_multipart(server, None).await;
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
        WireBehavior::CompleteFailure => {
            mount_multipart(server, None).await;
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
        WireBehavior::StaleUpload => {
            mount_multipart(server, None).await;
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
        WireBehavior::ConflictExhausted => {
            mount_multipart(server, None).await;
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
        _ => unreachable!(),
    }
}

fn containers_enabled() -> bool {
    cfg!(target_os = "linux") || std::env::var_os(RUN_CONTAINERS).is_some()
}

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

async fn minio() -> Minio {
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        NEXT_CONTAINER.fetch_add(1, Ordering::Relaxed)
    );
    let network = format!("peryx-s3-{suffix}");
    let name = format!("peryx-minio-{suffix}");
    let container = MinIO::default()
        .with_tag("RELEASE.2025-04-22T22-12-26Z")
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

async fn toxiproxy(minio: &Minio) -> Toxiproxy {
    let container = GenericImage::new("ghcr.io/shopify/toxiproxy", "2.12.0")
        .with_exposed_port(8_474.tcp())
        .with_exposed_port(8_666.tcp())
        .with_wait_for(WaitFor::http(
            HttpWaitStrategy::new("/version")
                .with_port(8_474.tcp())
                .with_expected_status_code(200_u16),
        ))
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
        metrics: format!(
            "http://127.0.0.1:{}/metrics",
            container.get_host_port_ipv4(8_474.tcp()).await.unwrap()
        ),
        container,
    }
}

async fn metric(toxiproxy: &Toxiproxy, direction: &str) -> u64 {
    let metrics = reqwest::get(&toxiproxy.metrics).await.unwrap().text().await.unwrap();
    metrics
        .lines()
        .find(|line| {
            line.starts_with("toxiproxy_proxy_received_bytes_total{")
                && line.contains(&format!("direction=\"{direction}\""))
                && line.contains("proxy=\"s3\"")
        })
        .and_then(|line| line.split_whitespace().next_back())
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

async fn wait_for_metric_above(toxiproxy: &Toxiproxy, direction: &str, previous: u64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if metric(toxiproxy, direction).await > previous {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn child_command(endpoint: &str, staging: &Path, scenario: &str, access_key: &str, secret_key: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg("test_s3_default_credential_chain_child")
        .arg("--nocapture")
        .env(CHILD_SCENARIO, scenario)
        .env(ENDPOINT, endpoint)
        .env(STAGING_DIR, staging)
        .env("AWS_ACCESS_KEY_ID", access_key)
        .env("AWS_SECRET_ACCESS_KEY", secret_key)
        .env("AWS_REGION", "us-east-1")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_SHARED_CREDENTIALS_FILE");
    command
}

async fn child(endpoint: &str, staging: &Path, scenario: &str, access_key: &str, secret_key: &str) -> Output {
    tokio::time::timeout(
        Duration::from_secs(30),
        child_command(endpoint, staging, scenario, access_key, secret_key).output(),
    )
    .await
    .unwrap()
    .unwrap()
}

fn assert_child_succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[rstest]
#[case::missing(WireBehavior::Missing)]
#[case::head(WireBehavior::Head)]
#[case::whole_read(WireBehavior::WholeRead)]
#[case::range(WireBehavior::Range)]
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
#[case::put_error(WireBehavior::PutError)]
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
        assert_eq!(count_stage(&requests, MultipartStage::Create), 1);
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
        let (mut connection, _) = listener.accept().await.unwrap();
        assert_ne!(connection.read(&mut [0; 4096]).await.unwrap(), 0);
        received.send(()).unwrap();
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

    tokio::time::timeout(Duration::from_secs(5), request)
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
    mount_multipart(&server, None).await;
    Mock::given(method("POST"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(service_error(404, "NoSuchUpload"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "5242881"))
        .mount(&server)
        .await;
    let staging = tempfile::tempdir().unwrap();

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
    assert_eq!(count_stage(&requests, MultipartStage::Create), 1);
    assert!(!requests.iter().any(|request| request.method.as_str() == "DELETE"));
    assert!(!multipart_journal(staging.path()).exists());
}

#[tokio::test]
async fn test_s3_bounds_recovery_from_missing_multipart_uploads() {
    let server = MockServer::start().await;
    mount_multipart(&server, None).await;
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
    assert_eq!(count_stage(&requests, MultipartStage::Create), 2);
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
    mount_multipart(&server, None).await;
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
    let second_finished = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&second_finished);
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
                std::thread::sleep(Duration::from_millis(250));
                finished.store(true, Ordering::Release);
                part_response()
            }
        })
        .mount(&server)
        .await;
    let abort_before_finish = Arc::new(AtomicBool::new(false));
    let early_abort = Arc::clone(&abort_before_finish);
    Mock::given(method("DELETE"))
        .and(query_param("uploadId", "upload-1"))
        .respond_with(move |_: &Request| {
            early_abort.store(!second_finished.load(Ordering::Acquire), Ordering::Release);
            ResponseTemplate::new(204)
        })
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
    assert!(!abort_before_finish.load(Ordering::Acquire));
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

#[rstest]
#[case::create(MultipartStage::Create)]
#[case::part(MultipartStage::Part)]
#[case::complete(MultipartStage::Complete)]
#[tokio::test]
async fn test_s3_multipart_resumes_after_cancellation(#[case] stage: MultipartStage) {
    let server = MockServer::start().await;
    mount_multipart(&server, Some(stage)).await;
    let staging = tempfile::tempdir().unwrap();
    let mut process = child_command(
        &server.uri(),
        staging.path(),
        "wire_cancel",
        ROOT_ACCESS_KEY,
        ROOT_SECRET_KEY,
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    wait_for_stage(&server, stage).await;
    process.stdin.take().unwrap().write_all(b"cancel").await.unwrap();
    assert_child_succeeded(
        &tokio::time::timeout(Duration::from_secs(10), process.wait_with_output())
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(
        count_stage(&server.received_requests().await.unwrap(), MultipartStage::Create),
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
    mount_multipart(&server, None).await;

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
    mount_multipart(&server, None).await;
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
    assert_eq!(count_stage(&requests, MultipartStage::Create), 2);
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
    mount_multipart(&server, None).await;
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
    assert_eq!(
        count_stage(&server.received_requests().await.unwrap(), MultipartStage::Create),
        1
    );
}

struct InterruptedUpload {
    minio: Minio,
    toxiproxy: Toxiproxy,
    staging: tempfile::TempDir,
    key: String,
    upload_id: String,
}

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
            "latency",
            "-a",
            "latency=60000",
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
    let (key, upload_id) = wait_for_multipart_upload(&minio).await;
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
        ["/toxiproxy-cli", "toxic", "remove", "-n", "latency_downstream", "s3"],
    )
    .await;
    wait_for_file(&multipart_journal(staging.path())).await;
    process.stdin.take().unwrap().write_all(b"cancel").await.unwrap();
    assert_child_succeeded(
        &tokio::time::timeout(Duration::from_secs(10), process.wait_with_output())
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

async fn wait_for_multipart_upload(minio: &Minio) -> (String, String) {
    let client = admin_client(&minio.endpoint);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(upload) = client
                .list_multipart_uploads()
                .bucket(BUCKET)
                .send()
                .await
                .unwrap()
                .uploads
                .unwrap_or_default()
                .into_iter()
                .next()
            {
                return (upload.key.unwrap(), upload.upload_id.unwrap());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

async fn remove_timeout(upload: &InterruptedUpload) {
    exec(
        &upload.toxiproxy.container,
        ["/toxiproxy-cli", "toxic", "remove", "-n", "timeout_upstream", "s3"],
    )
    .await;
}

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

async fn wait_for_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !tokio::fs::try_exists(path).await.unwrap() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn add_toxic(container: &ContainerAsync<GenericImage>, toxic: &str, attributes: &[&str]) {
    let mut args = vec![
        "/toxiproxy-cli".to_owned(),
        "toxic".to_owned(),
        "add".to_owned(),
        "-t".to_owned(),
        toxic.to_owned(),
    ];
    for attribute in attributes {
        args.extend(["-a".to_owned(), (*attribute).to_owned()]);
    }
    args.push("s3".to_owned());
    exec(container, args).await;
}

async fn stream_failure(toxic: &str, attributes: &[&str], scenario: &str, after_open: bool) {
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
    if after_open {
        add_toxic(&toxiproxy.container, "bandwidth", &["rate=64"]).await;
    } else {
        add_toxic(&toxiproxy.container, toxic, attributes).await;
    }
    let staging = tempfile::tempdir().unwrap();
    let mut process = child_command(
        &toxiproxy.endpoint,
        staging.path(),
        scenario,
        ROOT_ACCESS_KEY,
        ROOT_SECRET_KEY,
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    wait_for_file(&staging.path().join("opened")).await;
    if after_open {
        add_toxic(&toxiproxy.container, toxic, attributes).await;
    }
    process.stdin.take().unwrap().write_all(b"collect").await.unwrap();
    assert_child_succeeded(
        &tokio::time::timeout(Duration::from_secs(10), process.wait_with_output())
            .await
            .unwrap()
            .unwrap(),
    );
}

#[tokio::test]
async fn test_s3_uses_the_default_credential_chain() {
    if !containers_enabled() {
        return;
    }
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
}

#[tokio::test]
async fn test_s3_surfaces_invalid_credentials() {
    if !containers_enabled() {
        return;
    }
    let minio = minio().await;
    let staging = tempfile::tempdir().unwrap();
    assert_child_succeeded(&child(&minio.endpoint, staging.path(), "invalid", "invalid", "invalid-secret").await);
}

#[tokio::test]
async fn test_s3_surfaces_a_valid_readonly_principal() {
    if !containers_enabled() {
        return;
    }
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
}

#[tokio::test]
async fn test_s3_retries_after_a_truncated_response() {
    if !containers_enabled() {
        return;
    }
    let minio = minio().await;
    let toxiproxy = toxiproxy(&minio).await;
    exec(
        &toxiproxy.container,
        [
            "/toxiproxy-cli",
            "toxic",
            "add",
            "-t",
            "limit_data",
            "-a",
            "bytes=1",
            "s3",
        ],
    )
    .await;
    let staging = tempfile::tempdir().unwrap();
    let endpoint = toxiproxy.endpoint.clone();
    let staging_path = staging.path().to_owned();
    let child =
        tokio::spawn(async move { child(&endpoint, &staging_path, "health", ROOT_ACCESS_KEY, ROOT_SECRET_KEY).await });
    wait_for_metric_above(&toxiproxy, "downstream", 0).await;
    exec(
        &toxiproxy.container,
        ["/toxiproxy-cli", "toxic", "remove", "-n", "limit_data_downstream", "s3"],
    )
    .await;
    assert_child_succeeded(&child.await.unwrap());
}

#[tokio::test]
async fn test_s3_resumes_a_cancelled_multipart_upload() {
    if !containers_enabled() {
        return;
    }
    resume_upload(&interrupted_upload().await).await;
}

#[tokio::test]
async fn test_s3_restarts_a_stale_multipart_journal() {
    if !containers_enabled() {
        return;
    }
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
    mount_multipart(&server, None).await;
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
    mount_multipart(&server, Some(MultipartStage::Create)).await;
    let staging = tempfile::tempdir().unwrap();
    let endpoint = server.uri();
    let staging_path = staging.path().to_owned();
    let upload = tokio::spawn(async move {
        child(
            &endpoint,
            &staging_path,
            "wire_journal_failure",
            ROOT_ACCESS_KEY,
            ROOT_SECRET_KEY,
        )
        .await
    });
    wait_for_stage(&server, MultipartStage::Create).await;
    std::fs::write(staging.path().join("s3-multipart"), []).unwrap();

    assert_child_succeeded(&upload.await.unwrap());
}

#[cfg(unix)]
#[tokio::test]
async fn test_s3_reports_an_unwritable_multipart_journal_directory() {
    use std::os::unix::fs::PermissionsExt as _;

    let server = MockServer::start().await;
    mount_multipart(&server, None).await;
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
    mount_multipart(&server, None).await;
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
    mount_multipart(&server, None).await;
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

#[tokio::test]
async fn test_s3_coordinates_simultaneous_multipart_acquisition() {
    if !containers_enabled() {
        return;
    }
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
}

#[tokio::test]
async fn test_s3_reports_a_reset_stream_body() {
    if !containers_enabled() {
        return;
    }
    stream_failure("reset_peer", &["timeout=0"], "stream_reset", true).await;
}

#[tokio::test]
async fn test_s3_times_out_a_stalled_stream_body() {
    if !containers_enabled() {
        return;
    }
    stream_failure("timeout", &["timeout=0"], "stream_timeout", true).await;
}

#[tokio::test]
async fn test_s3_times_out_a_trickling_stream_body() {
    if !containers_enabled() {
        return;
    }
    stream_failure(
        "slicer",
        &["average_size=1048576", "size_variation=0", "delay=100000"],
        "stream_trickle",
        false,
    )
    .await;
}

#[tokio::test]
async fn test_s3_default_credential_chain_child() {
    let Ok(scenario) = std::env::var(CHILD_SCENARIO) else {
        return;
    };
    let mut settings = settings(std::env::var(ENDPOINT).unwrap());
    if matches!(scenario.as_str(), "cancel" | "wire_cancel") {
        settings.upload_concurrency = 1;
    } else if scenario == "wire_multipart" {
        settings.upload_concurrency = 3;
    } else if matches!(
        scenario.as_str(),
        "wire_abort_failure" | "wire_conflict_exhausted" | "wire_create_failure"
    ) {
        settings.max_retries = 0;
    } else if scenario == "wire_huge_timeout" {
        settings.request_timeout = Duration::MAX;
    } else if matches!(
        scenario.as_str(),
        "wire_truncated_body" | "wire_head_missing_length" | "wire_get_missing_length"
    ) {
        settings.max_retries = 0;
    } else if scenario == "wire_send_timeout" {
        settings.request_timeout = Duration::from_millis(500);
        settings.max_retries = 0;
    } else if scenario.starts_with("stream_") {
        settings.request_timeout = Duration::from_millis(250);
        settings.max_retries = 0;
    }
    let staging_dir = PathBuf::from(std::env::var_os(STAGING_DIR).unwrap());
    let storage = BlobStorage::s3(S3Config::new(settings).unwrap(), staging_dir.clone());
    run_child_scenario(&storage, &staging_dir, &scenario).await;
}

async fn run_child_scenario(storage: &BlobStorage, staging_dir: &Path, scenario: &str) {
    match scenario {
        "health" | "invalid" | "readonly" | "cancel" | "multipart" | "concurrent" | "stream_reset"
        | "stream_timeout" | "stream_trickle" => run_container_child(storage, staging_dir, scenario).await,
        "wire_missing"
        | "wire_head"
        | "wire_whole_read"
        | "wire_range"
        | "wire_empty_range"
        | "wire_verify"
        | "wire_verify_mismatch"
        | "wire_truncated_body"
        | "wire_materialize" => run_wire_read_child(storage, scenario).await,
        "wire_delete" | "wire_present" | "wire_small_put" | "wire_immutable" => {
            run_wire_write_child(storage, scenario).await;
        }
        "wire_health_error"
        | "wire_head_error"
        | "wire_head_missing_length"
        | "wire_huge_timeout"
        | "wire_send_timeout"
        | "wire_get_missing_length"
        | "wire_get_error"
        | "wire_put_error"
        | "wire_delete_not_found" => {
            run_wire_failure_child(storage, scenario).await;
        }
        "wire_multipart"
        | "wire_conflict"
        | "wire_create_failure"
        | "wire_create_missing_id"
        | "wire_part_missing_etag"
        | "wire_part_missing_checksum"
        | "wire_complete_failure"
        | "wire_conflict_exhausted"
        | "wire_journal_failure"
        | "wire_complete_exists"
        | "wire_stale_upload"
        | "wire_cancel"
        | "wire_abort_failure" => run_wire_multipart_child(storage, scenario).await,
        _ => unreachable!(),
    }
}

async fn run_container_child(storage: &BlobStorage, staging_dir: &Path, scenario: &str) {
    match scenario {
        "health" => storage.health().await.unwrap(),
        "invalid" => assert_eq!(storage.health().await.unwrap_err().kind(), BlobErrorKind::Io),
        "readonly" => {
            storage.health().await.unwrap();
            assert_eq!(
                storage.put_bytes(b"denied").await.unwrap_err().kind(),
                BlobErrorKind::Io
            );
        }
        "cancel" => {
            let other = storage.clone();
            let upload = tokio::spawn(async move { other.put_bytes(&vec![7; (5 << 20) + 1]).await });
            tokio::task::spawn_blocking(|| stdin().read_exact(&mut [0]))
                .await
                .unwrap()
                .unwrap();
            upload.abort();
            assert!(upload.await.unwrap_err().is_cancelled());
        }
        "multipart" => {
            storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap();
        }
        "concurrent" => {
            let bytes = vec![7; (5 << 20) + 1];
            let other = storage.clone();
            let (first, second) = tokio::join!(storage.put_bytes(&bytes), other.put_bytes(&bytes));
            assert_eq!(first.unwrap(), second.unwrap());
        }
        "stream_reset" | "stream_timeout" | "stream_trickle" => {
            let bytes = vec![0x5a; STREAM_BYTES];
            let read = storage.open(&Digest::of(&bytes), None).await.unwrap();
            tokio::fs::write(staging_dir.join("opened"), []).await.unwrap();
            tokio::task::spawn_blocking(|| stdin().read_exact(&mut [0]))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                read.collect(STREAM_BYTES as u64).await.unwrap_err().kind(),
                BlobErrorKind::Io
            );
        }
        _ => unreachable!(),
    }
}

async fn run_wire_read_child(storage: &BlobStorage, scenario: &str) {
    match scenario {
        "wire_missing" => {
            assert_eq!(
                storage.open(&Digest::of(b"missing"), None).await.err().unwrap().kind(),
                BlobErrorKind::NotFound
            );
        }
        "wire_head" => {
            assert_eq!(storage.head(&Digest::of(b"package")).await.unwrap().unwrap().bytes, 7);
        }
        "wire_whole_read" => {
            assert_eq!(
                storage.read_bytes(&Digest::of(b"package"), 7).await.unwrap(),
                b"package"
            );
        }
        "wire_range" => {
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
        "wire_empty_range" => {
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
        "wire_verify" => assert!(storage.verify(&Digest::of(b"package")).await.unwrap()),
        "wire_verify_mismatch" => {
            let digest = Digest::of(b"expected");
            assert_eq!(storage.read_bytes(&digest, 7).await.unwrap(), b"corrupt");
            assert!(!storage.verify(&digest).await.unwrap());
        }
        "wire_truncated_body" => {
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
        "wire_materialize" => {
            assert_eq!(
                std::fs::read(storage.materialize(&Digest::of(b"package")).await.unwrap().path()).unwrap(),
                b"package"
            );
        }
        _ => unreachable!(),
    }
}

async fn run_wire_write_child(storage: &BlobStorage, scenario: &str) {
    match scenario {
        "wire_delete" => {
            let digest = Digest::of(b"package");
            assert!(storage.delete(&digest).await.unwrap());
            assert!(!storage.delete(&digest).await.unwrap());
        }
        "wire_present" => {
            let present = Digest::of(b"present");
            assert_eq!(
                storage
                    .present(vec![present.clone(), Digest::of(b"missing")])
                    .await
                    .unwrap(),
                std::collections::HashSet::from([present])
            );
        }
        "wire_small_put" => {
            assert_eq!(storage.put_bytes(b"package").await.unwrap(), Digest::of(b"package"));
        }
        "wire_immutable" => {
            let digest = storage.put_bytes(b"expected").await.unwrap();
            assert_eq!(storage.read_bytes(&digest, 8).await.unwrap(), b"existing");
        }
        _ => unreachable!(),
    }
}

async fn run_wire_failure_child(storage: &BlobStorage, scenario: &str) {
    match scenario {
        "wire_health_error" => assert_eq!(storage.health().await.unwrap_err().kind(), BlobErrorKind::Io),
        "wire_head_error" | "wire_head_missing_length" => assert_eq!(
            storage.head(&Digest::of(b"package")).await.unwrap_err().kind(),
            BlobErrorKind::Io
        ),
        "wire_huge_timeout" | "wire_get_error" | "wire_get_missing_length" => assert_eq!(
            storage.open(&Digest::of(b"package"), None).await.err().unwrap().kind(),
            BlobErrorKind::Io
        ),
        "wire_send_timeout" => {
            let error = storage.open(&Digest::of(b"package"), None).await.err().unwrap();
            assert_eq!(error.kind(), BlobErrorKind::Io);
            assert!(format!("{error:?}").contains("deadline has elapsed"), "{error:?}");
        }
        "wire_put_error" => assert_eq!(
            storage.put_bytes(b"package").await.unwrap_err().kind(),
            BlobErrorKind::Io
        ),
        "wire_delete_not_found" => assert!(!storage.delete(&Digest::of(b"missing")).await.unwrap()),
        _ => unreachable!(),
    }
}

async fn run_wire_multipart_child(storage: &BlobStorage, scenario: &str) {
    match scenario {
        "wire_multipart" | "wire_conflict" | "wire_complete_exists" | "wire_stale_upload" => {
            storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap();
        }
        "wire_create_failure" => {
            let error = storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap_err();
            assert_eq!(error.kind(), BlobErrorKind::Io);
            assert!(format!("{error:?}").contains("s3 request failed"), "{error:?}");
        }
        "wire_create_missing_id"
        | "wire_part_missing_etag"
        | "wire_part_missing_checksum"
        | "wire_complete_failure"
        | "wire_conflict_exhausted"
        | "wire_journal_failure" => {
            assert_eq!(
                storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap_err().kind(),
                BlobErrorKind::Io
            );
        }
        "wire_cancel" => {
            let bytes = vec![7; (5 << 20) + 1];
            let other = storage.clone();
            let upload = tokio::spawn(async move { other.put_bytes(&bytes).await });
            tokio::task::spawn_blocking(|| stdin().read_exact(&mut [0]))
                .await
                .unwrap()
                .unwrap();
            upload.abort();
            assert!(upload.await.unwrap_err().is_cancelled());
            storage.put_bytes(&vec![7; (5 << 20) + 1]).await.unwrap();
        }
        "wire_abort_failure" => {
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
        _ => unreachable!(),
    }
}
