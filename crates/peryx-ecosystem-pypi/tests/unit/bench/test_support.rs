use std::io::{Cursor, Write as _};
use std::sync::OnceLock;

use peryx_bench_core::context::BenchmarkContext;
use peryx_bench_core::servers::Server;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub(super) const WHEEL: &str = "sample_pkg-1.0-py3-none-any.whl";

static ENDPOINTS_GOOD: OnceLock<String> = OnceLock::new();
static ENDPOINTS_BAD: OnceLock<String> = OnceLock::new();
static FLEET_GOOD: OnceLock<String> = OnceLock::new();
static FLEET_BAD: OnceLock<String> = OnceLock::new();
static INSTALL_GOOD: OnceLock<String> = OnceLock::new();
static INSTALL_BAD: OnceLock<String> = OnceLock::new();
static LOAD_GOOD: OnceLock<String> = OnceLock::new();
static LOAD_BAD: OnceLock<String> = OnceLock::new();
static METADATA_GOOD: OnceLock<String> = OnceLock::new();
static METADATA_BAD: OnceLock<String> = OnceLock::new();
static THROUGHPUT_GOOD: OnceLock<String> = OnceLock::new();
static THROUGHPUT_BAD: OnceLock<String> = OnceLock::new();

pub(super) fn benchmark() -> (tempfile::TempDir, BenchmarkContext) {
    let directory = tempfile::tempdir().unwrap();
    let scratch = directory.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let context = BenchmarkContext::with_scratch("peryx".into(), directory.path().join("report.toml"), scratch);
    (directory, context)
}

pub(super) fn http_client() -> reqwest::Client {
    install_crypto_provider();
    reqwest::Client::new()
}

pub(super) fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub(super) fn server(name: &'static str, base_url: fn(u16) -> String) -> Server {
    Server {
        name,
        homepage: "https://example.invalid/",
        base_url,
        probe: identity_probe,
        command: None,
        setup: None,
        teardown: None,
    }
}

pub(super) async fn wheel_index() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/sample-pkg/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                serde_json::json!({
                    "meta": {"api-version": "1.4"},
                    "name": "sample-pkg",
                    "files": [{"filename": WHEEL, "url": format!("/files/{WHEEL}"), "hashes": {}}],
                    "versions": ["1.0"]
                })
                .to_string(),
                "application/vnd.pypi.simple.v1+json",
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{WHEEL}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel()))
        .mount(&server)
        .await;
    server
}

pub(super) fn endpoints_good_base(_: u16) -> String {
    ENDPOINTS_GOOD.get().unwrap().clone()
}

pub(super) fn endpoints_bad_base(_: u16) -> String {
    ENDPOINTS_BAD.get().unwrap().clone()
}

pub(super) fn fleet_good_base(_: u16) -> String {
    FLEET_GOOD.get().unwrap().clone()
}

pub(super) fn fleet_bad_base(_: u16) -> String {
    FLEET_BAD.get().unwrap().clone()
}

pub(super) fn install_good_base(_: u16) -> String {
    INSTALL_GOOD.get().unwrap().clone()
}

pub(super) fn install_bad_base(_: u16) -> String {
    INSTALL_BAD.get().unwrap().clone()
}

pub(super) fn load_good_base(_: u16) -> String {
    LOAD_GOOD.get().unwrap().clone()
}

pub(super) fn load_bad_base(_: u16) -> String {
    LOAD_BAD.get().unwrap().clone()
}

pub(super) fn metadata_good_base(_: u16) -> String {
    METADATA_GOOD.get().unwrap().clone()
}

pub(super) fn metadata_bad_base(_: u16) -> String {
    METADATA_BAD.get().unwrap().clone()
}

pub(super) fn throughput_good_base(_: u16) -> String {
    THROUGHPUT_GOOD.get().unwrap().clone()
}

pub(super) fn throughput_bad_base(_: u16) -> String {
    THROUGHPUT_BAD.get().unwrap().clone()
}

pub(super) fn set_endpoints_bases(good: &MockServer, bad: &MockServer) {
    ENDPOINTS_GOOD.set(format!("{}/simple/", good.uri())).unwrap();
    ENDPOINTS_BAD.set(format!("{}/invalid/", bad.uri())).unwrap();
}

pub(super) fn set_fleet_bases(good: &MockServer, bad: &MockServer) {
    FLEET_GOOD.set(format!("{}/simple/", good.uri())).unwrap();
    FLEET_BAD.set(format!("{}/simple/", bad.uri())).unwrap();
}

pub(super) fn set_install_bases(good: &MockServer, bad: &MockServer) {
    INSTALL_GOOD.set(format!("{}/simple/", good.uri())).unwrap();
    INSTALL_BAD.set(format!("{}/simple/", bad.uri())).unwrap();
}

pub(super) fn set_load_bases(good: &MockServer, bad: &MockServer) {
    LOAD_GOOD.set(format!("{}/simple/", good.uri())).unwrap();
    LOAD_BAD.set(format!("{}/simple/", bad.uri())).unwrap();
}

pub(super) fn set_metadata_bases(good: &MockServer, bad: &MockServer) {
    METADATA_GOOD.set(format!("{}/simple/", good.uri())).unwrap();
    METADATA_BAD.set(format!("{}/simple/", bad.uri())).unwrap();
}

pub(super) fn set_throughput_bases(good: &MockServer, bad: &MockServer) {
    THROUGHPUT_GOOD.set(format!("{}/simple/", good.uri())).unwrap();
    THROUGHPUT_BAD.set(format!("{}/simple/", bad.uri())).unwrap();
}

fn identity_probe(base: &str) -> String {
    base.to_owned()
}

fn wheel() -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut archive = zip::ZipWriter::new(Cursor::new(&mut bytes));
    let options = zip::write::SimpleFileOptions::default();
    for (name, content) in [
        ("sample_pkg/__init__.py", "__version__ = \"1.0\"\n"),
        (
            "sample_pkg-1.0.dist-info/METADATA",
            "Metadata-Version: 2.1\nName: sample-pkg\nVersion: 1.0\n",
        ),
        (
            "sample_pkg-1.0.dist-info/WHEEL",
            "Wheel-Version: 1.0\nGenerator: peryx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
        ),
        (
            "sample_pkg-1.0.dist-info/RECORD",
            "sample_pkg/__init__.py,,\nsample_pkg-1.0.dist-info/METADATA,,\n\
             sample_pkg-1.0.dist-info/WHEEL,,\nsample_pkg-1.0.dist-info/RECORD,,\n",
        ),
    ] {
        archive.start_file(name, options).unwrap();
        archive.write_all(content.as_bytes()).unwrap();
    }
    archive.finish().unwrap();
    bytes
}
