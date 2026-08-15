use futures_util::TryStreamExt as _;
use rstest::rstest;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::{ArtifactClient, NamedUpstream, RouteError, UpstreamClient, UpstreamError, UpstreamHealth, UpstreamRouter};

fn upstream(name: &str) -> NamedUpstream {
    NamedUpstream::new(
        name,
        UpstreamClient::new(&format!("https://{name}.example/api/")).unwrap(),
    )
}

fn router() -> UpstreamRouter {
    UpstreamRouter::new(vec![upstream("internal"), upstream("mirror"), upstream("alpha")]).unwrap()
}

fn names(router: &UpstreamRouter, key: &str) -> Vec<String> {
    router
        .candidates(key)
        .map(|upstream| upstream.name().to_owned())
        .collect()
}

#[test]
fn test_upstream_router_preserves_fallback_order() {
    assert_eq!(names(&router(), "demo"), ["internal", "mirror", "alpha"]);
}

#[test]
fn test_upstream_router_disables_key_fallback() {
    assert_eq!(names(&router().with_fallback(false), "demo"), ["internal"]);
}

#[test]
fn test_upstream_router_protects_one_key_from_fallback() {
    let router = router().protect("private").unwrap();
    assert_eq!(names(&router, "private"), ["internal"]);
    assert_eq!(names(&router, "public"), ["internal", "mirror", "alpha"]);
}

#[test]
fn test_upstream_router_pin_is_strict() {
    let router = router().pin("pinned-resource", "mirror").unwrap();
    assert_eq!(names(&router, "pinned-resource"), ["mirror"]);
    assert_eq!(names(&router, "fallback-resource"), ["internal", "mirror", "alpha"]);
}

#[test]
fn test_upstream_router_exposes_the_selected_client() {
    let router = router().pin("demo", "mirror").unwrap();
    let selected = router.candidates("demo").next().unwrap();
    assert_eq!(selected.client().redacted_base_url(), "https://mirror.example/api/");
}

#[test]
fn test_upstream_router_finds_a_source_by_name() {
    let router = router();
    assert_eq!(router.source("mirror").unwrap().name(), "mirror");
    assert!(router.source("missing").is_none());
}

#[test]
fn test_named_upstream_health_is_shared_between_router_clones() {
    let router = router();
    let cloned = router.clone();
    assert_eq!(
        router.sources().map(NamedUpstream::health).collect::<Vec<_>>(),
        [UpstreamHealth::Configured; 3]
    );

    router.sources().next().unwrap().mark_unhealthy();
    cloned.sources().nth(1).unwrap().mark_healthy();

    assert_eq!(
        router.sources().map(NamedUpstream::health).collect::<Vec<_>>(),
        [
            UpstreamHealth::Unhealthy,
            UpstreamHealth::Healthy,
            UpstreamHealth::Configured
        ]
    );
}

#[rstest]
#[case::configured(UpstreamHealth::Configured, "configured")]
#[case::healthy(UpstreamHealth::Healthy, "healthy")]
#[case::unhealthy(UpstreamHealth::Unhealthy, "unhealthy")]
fn test_upstream_health_has_stable_status_text(#[case] health: UpstreamHealth, #[case] expected: &str) {
    assert_eq!(health.as_str(), expected);
}

#[test]
fn test_upstream_router_rejects_no_sources() {
    assert_eq!(UpstreamRouter::new(Vec::new()).unwrap_err(), RouteError::Empty);
}

#[test]
fn test_upstream_router_rejects_an_empty_source_name() {
    let client = UpstreamClient::new("https://example.invalid/api/").unwrap();
    assert_eq!(
        UpstreamRouter::new(vec![NamedUpstream::new("", client)]).unwrap_err(),
        RouteError::EmptyName
    );
}

#[test]
fn test_upstream_router_rejects_duplicate_source_names() {
    assert_eq!(
        UpstreamRouter::new(vec![upstream("alpha"), upstream("alpha")]).unwrap_err(),
        RouteError::DuplicateName("alpha".to_owned())
    );
}

#[test]
fn test_upstream_router_rejects_an_empty_key_pin() {
    assert_eq!(router().pin("", "alpha").unwrap_err(), RouteError::EmptyKey);
}

#[test]
fn test_upstream_router_rejects_an_empty_protected_key() {
    assert_eq!(router().protect("").unwrap_err(), RouteError::EmptyKey);
}

#[test]
fn test_upstream_router_rejects_an_unknown_pin_source() {
    assert_eq!(
        router().pin("demo", "missing").unwrap_err(),
        RouteError::UnknownPin {
            key: "demo".to_owned(),
            upstream: "missing".to_owned(),
        }
    );
}

#[tokio::test]
async fn test_artifact_client_falls_back_for_range_reads() {
    let origin = MockServer::start().await;
    let mirror = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/mirror/files/artifact.bin"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&mirror)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/files/artifact.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", "5"),
        )
        .expect(1)
        .mount(&origin)
        .await;
    Mock::given(method("GET"))
        .and(path("/mirror/files/artifact.bin"))
        .and(header("range", "bytes=1-3"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&mirror)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .and(header("range", "bytes=1-3"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 1-3/5")
                .set_body_bytes(b"hee".to_vec()),
        )
        .expect(1)
        .mount(&origin)
        .await;
    let source = NamedUpstream::new(
        "origin",
        UpstreamClient::new(&format!("{}/api/", origin.uri())).unwrap(),
    )
    .with_artifact_mirror(UpstreamClient::new(&format!("{}/mirror/", mirror.uri())).unwrap(), true);
    let artifacts = source.artifacts();
    let url = format!("{}/files/artifact.bin?signature=origin", origin.uri());

    assert!(artifacts.may_support_ranges());
    assert_eq!(artifacts.head_file_for_range(&url).await.unwrap().len, 5);
    assert_eq!(&artifacts.fetch_range(&url, 1, 3).await.unwrap()[..], b"hee");

    artifacts.disable_ranges();
    assert!(!artifacts.may_support_ranges());
}

#[tokio::test]
async fn test_artifact_client_reads_ranges_from_mirror() {
    let mirror = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/files/artifact.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", "5"),
        )
        .expect(1)
        .mount(&mirror)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 1-3/5")
                .set_body_bytes(b"hee".to_vec()),
        )
        .expect(1)
        .mount(&mirror)
        .await;
    let source = NamedUpstream::new("origin", UpstreamClient::new("https://origin.example/api/").unwrap())
        .with_artifact_mirror(UpstreamClient::new(&mirror.uri()).unwrap(), false);
    let artifacts = source.artifacts();

    assert_eq!(
        artifacts
            .head_file_for_range("https://origin.example/files/artifact.bin")
            .await
            .unwrap()
            .len,
        5
    );
    assert_eq!(
        &artifacts
            .fetch_range("https://origin.example/files/artifact.bin", 1, 3)
            .await
            .unwrap()[..],
        b"hee"
    );
}

#[tokio::test]
async fn test_artifact_client_does_not_fallback_range_reads_when_disabled() {
    let mirror = MockServer::start().await;
    for request_method in ["HEAD", "GET"] {
        Mock::given(method(request_method))
            .and(path("/files/artifact.bin"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mirror)
            .await;
    }
    let source = NamedUpstream::new("origin", UpstreamClient::new("https://origin.example/api/").unwrap())
        .with_artifact_mirror(UpstreamClient::new(&mirror.uri()).unwrap(), false);
    let artifacts = source.artifacts();
    let url = "https://origin.example/files/artifact.bin";

    assert!(artifacts.head_file_for_range(url).await.is_err());
    assert!(artifacts.fetch_range(url, 1, 3).await.is_err());
}

#[tokio::test]
async fn test_artifact_client_rejects_an_invalid_advertised_url() {
    let client = UpstreamClient::new("https://origin.example/api/").unwrap();
    let source = NamedUpstream::new("origin", client)
        .with_artifact_mirror(UpstreamClient::new("https://mirror.example/").unwrap(), true);

    let err = source.artifacts().stream_bytes("not a url").await.err().unwrap();

    assert!(matches!(err, UpstreamError::Url(_)));
}

#[tokio::test]
async fn test_artifact_client_streams_from_its_mirror() {
    let mirror = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"mirror".to_vec()))
        .expect(1)
        .mount(&mirror)
        .await;
    let source = NamedUpstream::new("origin", UpstreamClient::new("https://origin.example/api/").unwrap())
        .with_artifact_mirror(UpstreamClient::new(&mirror.uri()).unwrap(), false);

    assert_eq!(
        source
            .artifacts()
            .stream_bytes("https://origin.example/files/artifact.bin")
            .await
            .unwrap()
            .try_fold(Vec::new(), |mut bytes, chunk| async move {
                bytes.extend_from_slice(&chunk);
                Ok(bytes)
            })
            .await
            .unwrap(),
        b"mirror"
    );
}

#[tokio::test]
async fn test_direct_artifact_client_streams_from_its_origin() {
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"origin".to_vec()))
        .expect(1)
        .mount(&origin)
        .await;
    let artifacts = ArtifactClient::from(UpstreamClient::new(&origin.uri()).unwrap());

    assert_eq!(
        artifacts
            .stream_bytes(&format!("{}/files/artifact.bin", origin.uri()))
            .await
            .unwrap()
            .try_fold(Vec::new(), |mut bytes, chunk| async move {
                bytes.extend_from_slice(&chunk);
                Ok(bytes)
            })
            .await
            .unwrap(),
        b"origin"
    );
}

#[rstest]
#[case::disabled(false, None)]
#[case::enabled(true, Some(b"origin".as_slice()))]
#[tokio::test]
async fn test_artifact_stream_fallback(#[case] fallback: bool, #[case] expected: Option<&[u8]>) {
    let origin = MockServer::start().await;
    let mirror = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&mirror)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"origin".to_vec()))
        .expect(u64::from(fallback))
        .mount(&origin)
        .await;
    let source = NamedUpstream::new("origin", UpstreamClient::new(&origin.uri()).unwrap())
        .with_artifact_mirror(UpstreamClient::new(&mirror.uri()).unwrap(), fallback);
    let result = source
        .artifacts()
        .stream_bytes(&format!("{}/files/artifact.bin", origin.uri()))
        .await;

    if let Some(expected) = expected {
        assert_eq!(
            result
                .unwrap()
                .try_fold(Vec::new(), |mut bytes, chunk| async move {
                    bytes.extend_from_slice(&chunk);
                    Ok(bytes)
                })
                .await
                .unwrap(),
            expected
        );
    } else {
        assert_eq!(result.err().unwrap().status(), Some(404));
    }
}

#[tokio::test]
async fn test_direct_artifact_client_reads_ranges() {
    let origin = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/files/artifact.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", "5"),
        )
        .expect(1)
        .mount(&origin)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 1-3/5")
                .set_body_bytes(b"hee".to_vec()),
        )
        .expect(1)
        .mount(&origin)
        .await;
    let artifacts = ArtifactClient::from(UpstreamClient::new(&origin.uri()).unwrap());
    let url = format!("{}/files/artifact.bin", origin.uri());

    assert_eq!(
        (
            artifacts.head_file_for_range(&url).await.unwrap().len,
            artifacts.fetch_range(&url, 1, 3).await.unwrap(),
        ),
        (5, bytes::Bytes::from_static(b"hee"))
    );
}

#[test]
fn test_direct_artifact_client_uses_origin_range_state() {
    let client = UpstreamClient::new("https://origin.example/api/").unwrap();
    let artifacts = ArtifactClient::from(client);
    assert!(artifacts.may_support_ranges());
    artifacts.disable_ranges();
    assert!(!artifacts.may_support_ranges());
}
