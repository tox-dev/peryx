use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

struct BearerChallenge {
    realm: String,
    requests: AtomicUsize,
    success: ResponseTemplate,
}

impl Respond for BearerChallenge {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if self.requests.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                format!("Bearer realm=\"{}\",service=\"registry\"", self.realm),
            )
        } else {
            self.success.clone()
        }
    }
}

#[test]
fn registry_blob_url_keeps_the_index_prefix() {
    assert_eq!(
        blob_url("https://registry.test/index/", "repo", "sha256:a").unwrap(),
        "https://registry.test/v2/index/repo/blobs/sha256:a"
    );
}

#[rstest::rstest]
#[case("aarch64", "arm64")]
#[case("x86_64", "amd64")]
#[case("riscv64", "riscv64")]
fn docker_arch_uses_oci_platform_names(#[case] input: &str, #[case] expected: &str) {
    assert_eq!(docker_arch_for(input), expected);
}

#[test]
fn registry_urls_reject_invalid_bases() {
    assert_eq!(
        blob_url("not a url", "repo", "sha256:a").unwrap_err().to_string(),
        "registry base is a valid URL"
    );
    assert_eq!(
        version_url("mailto:user@example.com").unwrap_err().to_string(),
        "registry base names a host"
    );
}

#[test]
fn environment_resolves_tools_from_a_directory() {
    let environment = BenchEnvironment::new(Some(std::path::Path::new("/tools")), None);
    assert_eq!(environment.tools.crane, std::path::Path::new("/tools/crane"));
}

#[tokio::test]
async fn finish_tasks_reports_panics() {
    let mut tasks: tokio::task::JoinSet<anyhow::Result<()>> = tokio::task::JoinSet::new();
    tasks.spawn(async { panic!("task panic") });

    assert_eq!(
        finish_tasks(tasks, "probe").await.unwrap_err().to_string(),
        "probe task failed"
    );
}

#[tokio::test]
async fn registry_auth_and_index_resolution() {
    let server = MockServer::start().await;
    let realm = format!("{}/token", server.uri());
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(query_param("service", "registry"))
        .and(query_param("scope", "repository:repo:pull"))
        .and(header("authorization", "Basic dXNlcjpzZWNyZXQ="))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"token": "token"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/auth"))
        .respond_with(BearerChallenge {
            realm,
            requests: AtomicUsize::new(0),
            success: ResponseTemplate::new(200).set_body_bytes(b"ok"),
        })
        .mount(&server)
        .await;

    let http = peryx_bench_core::servers::http_client().unwrap();
    let environment = authenticated_environment();
    assert_bearer_auth(&environment, &http, &server).await;

    let index = serde_json::json!({
        "manifests": [{
            "digest": "sha256:child",
            "platform": {"architecture": docker_arch(), "os": "linux"}
        }]
    });
    let manifest = serde_json::json!({"config": {"digest": "sha256:config"}});
    Mock::given(method("GET"))
        .and(path("/manifests/tag"))
        .respond_with(ResponseTemplate::new(200).set_body_json(index))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/manifests/sha256:child"))
        .respond_with(ResponseTemplate::new(200).set_body_json(manifest))
        .mount(&server)
        .await;
    assert_eq!(
        manifest_identity(&environment, &http, &format!("{}/manifests/tag", server.uri()), "repo",)
            .await
            .unwrap(),
        ("sha256:child".to_owned(), "sha256:config".to_owned())
    );

    Mock::given(method("GET"))
        .and(path("/v2/repo/blobs/digest"))
        .respond_with(BearerChallenge {
            realm: format!("{}/token", server.uri()),
            requests: AtomicUsize::new(0),
            success: ResponseTemplate::new(200).set_body_bytes(b"ok"),
        })
        .expect(2)
        .mount(&server)
        .await;
    stream_blob(&environment, &http, &server.uri(), "repo", "digest", 2)
        .await
        .unwrap();

    Mock::given(method("GET"))
        .and(path("/manifest-auth"))
        .respond_with(BearerChallenge {
            realm: format!("{}/token", server.uri()),
            requests: AtomicUsize::new(0),
            success: ResponseTemplate::new(200)
                .insert_header("docker-content-digest", "sha256:manifest")
                .set_body_json(serde_json::json!({"config": {"digest": "sha256:config"}})),
        })
        .mount(&server)
        .await;
    assert_eq!(
        fetch_manifest(&environment, &http, &format!("{}/manifest-auth", server.uri()), "repo",)
            .await
            .unwrap()
            .1,
        "sha256:manifest"
    );

    Mock::given(method("GET"))
        .and(path("/manifests/missing"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "manifests": [{
                "digest": "sha256:child",
                "platform": {"architecture": "missing", "os": "linux"}
            }]
        })))
        .mount(&server)
        .await;
    assert!(
        manifest_identity(
            &environment,
            &http,
            &format!("{}/manifests/missing", server.uri()),
            "repo",
        )
        .await
        .is_err()
    );
}

async fn assert_bearer_auth(environment: &BenchEnvironment, http: &reqwest::Client, server: &MockServer) {
    request(
        environment,
        http,
        &format!("{}/auth", server.uri()),
        Probe::get(""),
        "repo",
    )
    .await
    .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[2].headers.get("authorization").unwrap().to_str().unwrap(),
        "Bearer token"
    );
}

fn authenticated_environment() -> BenchEnvironment {
    BenchEnvironment::new(
        Some(std::path::Path::new("/tools")),
        Some(("user".to_owned(), "secret".to_owned())),
    )
}
