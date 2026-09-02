use std::collections::BTreeSet;

use axum::http::{Method, StatusCode, header};
use rstest::rstest;
use serde_json::Value;
use utoipa::openapi::PathsBuilder;

use super::{proxy, send};

/// A `/v2/` path template with a request URI that routes to it. The dispatcher answers a method it
/// does not serve with `405` and an `Allow` header, which is the accepted-method set the schema must
/// reproduce.
const ROUTE_SAMPLES: &[(&str, &str)] = &[
    ("/v2/_catalog", "/v2/_catalog"),
    ("/v2/{name}/blobs/uploads/", "/v2/hub/app/blobs/uploads/"),
    (
        "/v2/{name}/blobs/uploads/{session}",
        "/v2/hub/app/blobs/uploads/0000000000000000000000000000abcd",
    ),
    ("/v2/{name}/blobs/{digest}", "/v2/hub/app/blobs/sha256:2c3e"),
    (
        "/v2/{name}/blobs/{digest}/contents",
        "/v2/hub/app/blobs/sha256:2c3e/contents",
    ),
    ("/v2/{name}/manifests/{reference}", "/v2/hub/app/manifests/latest"),
    (
        "/v2/{name}/manifests/{reference}/restore",
        "/v2/hub/app/manifests/latest/restore",
    ),
    ("/v2/{name}/referrers/{digest}", "/v2/hub/app/referrers/sha256:2c3e"),
    ("/v2/{name}/tags/list", "/v2/hub/app/tags/list"),
];

fn documented_paths() -> Value {
    serde_json::to_value(crate::openapi::openapi_paths(PathsBuilder::new()).build()).unwrap()
}

/// An undocumented template yields no methods rather than panicking, so the caller's comparison names
/// the path it was looking for.
fn documented_methods(paths: &Value, template: &str) -> BTreeSet<String> {
    paths[template]
        .as_object()
        .into_iter()
        .flat_map(|methods| methods.keys().cloned())
        .collect()
}

fn operation<'a>(paths: &'a Value, template: &str, method: &str) -> &'a Value {
    &paths[template][method]
}

fn parameter_names(operation: &Value) -> BTreeSet<&str> {
    operation["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|parameter| parameter["name"].as_str().unwrap())
        .collect()
}

fn response_statuses(operation: &Value) -> BTreeSet<&str> {
    operation["responses"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

#[tokio::test]
async fn test_documented_methods_match_the_dispatcher_allow_header() {
    let paths = documented_paths();
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);

    for (template, uri) in ROUTE_SAMPLES {
        let (status, headers, _) = send(&app, Method::OPTIONS, uri).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{template}");
        let served: BTreeSet<String> = headers[header::ALLOW]
            .to_str()
            .unwrap()
            .split(", ")
            .map(str::to_ascii_lowercase)
            .collect();
        assert_eq!(documented_methods(&paths, template), served, "{template}");
    }
}

#[tokio::test]
async fn test_documented_paths_are_exactly_the_dispatched_routes() {
    let paths = documented_paths();
    let documented: BTreeSet<&str> = paths.as_object().unwrap().keys().map(String::as_str).collect();
    let dispatched: BTreeSet<&str> = ROUTE_SAMPLES
        .iter()
        .map(|(template, _)| *template)
        .chain(["/v2/", "/v2/token"])
        .collect();

    assert_eq!(documented, dispatched);
}

/// The version check and the token endpoint answer before the route table, so they carry no `Allow`
/// header and their methods are pinned against the dispatcher directly.
#[rstest]
#[case::version_get("/v2/", Method::GET)]
#[case::version_head("/v2/", Method::HEAD)]
#[case::token_get("/v2/token", Method::GET)]
#[tokio::test]
async fn test_prefix_routes_serve_their_documented_methods(#[case] path: &str, #[case] method: Method) {
    let paths = documented_paths();
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);

    assert!(
        documented_methods(&paths, path).contains(&method.as_str().to_ascii_lowercase()),
        "{path} documents {method}"
    );
    let (status, _, _) = send(&app, method, path).await;
    assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[rstest]
#[case::version_post("/v2/", Method::POST)]
#[case::token_post("/v2/token", Method::POST)]
#[tokio::test]
async fn test_prefix_routes_refuse_undocumented_methods(#[case] path: &str, #[case] method: Method) {
    let paths = documented_paths();
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);

    assert!(
        !documented_methods(&paths, path).contains(&method.as_str().to_ascii_lowercase()),
        "{path} does not document {method}"
    );
    let (status, _, body) = send(&app, method, path).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "NAME_UNKNOWN"), "{body:?}");
}

#[test]
fn test_upload_start_documents_the_mount_source_and_the_monolithic_body() {
    let paths = documented_paths();
    let start = operation(&paths, "/v2/{name}/blobs/uploads/", "post");

    assert_eq!(
        parameter_names(start),
        BTreeSet::from(["name", "digest", "mount", "from"])
    );
    assert_eq!(
        start["requestBody"]["content"]["application/octet-stream"]["schema"],
        serde_json::json!({"type": "string", "format": "binary"})
    );
}

#[test]
fn test_finishing_an_upload_documents_a_required_digest_and_a_body() {
    let paths = documented_paths();
    let finish = operation(&paths, "/v2/{name}/blobs/uploads/{session}", "put");
    let digest = finish["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|parameter| parameter["name"] == "digest")
        .unwrap();

    assert_eq!(digest["in"], "query");
    assert_eq!(digest["required"], true);
    assert_eq!(
        finish["requestBody"]["content"]["application/octet-stream"]["schema"],
        serde_json::json!({"type": "string", "format": "binary"})
    );
}

#[test]
fn test_layer_contents_documents_the_preview_offset() {
    let paths = documented_paths();
    let contents = operation(&paths, "/v2/{name}/blobs/{digest}/contents", "get");
    let offset = contents["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|parameter| parameter["name"] == "offset")
        .unwrap();

    assert_eq!(
        parameter_names(contents),
        BTreeSet::from(["name", "digest", "member", "offset"])
    );
    assert_eq!(offset["schema"], serde_json::json!({"type": "integer", "minimum": 0}));
}

#[test]
fn test_cancelling_an_upload_documents_its_no_content_and_unknown_session_responses() {
    let paths = documented_paths();
    let cancel = operation(&paths, "/v2/{name}/blobs/uploads/{session}", "delete");

    assert_eq!(response_statuses(cancel), BTreeSet::from(["204", "401", "403", "404"]));
    assert_eq!(
        cancel["responses"]["404"]["content"]["application/json"]["example"]["errors"][0]["code"],
        "BLOB_UPLOAD_UNKNOWN"
    );
}

#[rstest]
#[case::upload_start("/v2/{name}/blobs/uploads/", "post", "202")]
#[case::upload_status("/v2/{name}/blobs/uploads/{session}", "get", "204")]
#[case::upload_chunk("/v2/{name}/blobs/uploads/{session}", "patch", "202")]
#[case::out_of_order_chunk("/v2/{name}/blobs/uploads/{session}", "patch", "416")]
fn test_open_session_responses_document_their_resume_headers(
    #[case] template: &str,
    #[case] method: &str,
    #[case] status: &str,
) {
    let paths = documented_paths();
    let headers = &operation(&paths, template, method)["responses"][status]["headers"];

    assert_eq!(
        headers.as_object().unwrap().keys().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            &"Docker-Upload-UUID".to_owned(),
            &"Location".to_owned(),
            &"Range".to_owned()
        ])
    );
}

#[rstest]
#[case::manifest_push("/v2/{name}/manifests/{reference}", "put", "201")]
#[case::monolithic_push("/v2/{name}/blobs/uploads/", "post", "201")]
#[case::upload_finish("/v2/{name}/blobs/uploads/{session}", "put", "201")]
fn test_created_responses_document_location_and_the_content_digest(
    #[case] template: &str,
    #[case] method: &str,
    #[case] status: &str,
) {
    let paths = documented_paths();
    let headers = &operation(&paths, template, method)["responses"][status]["headers"];

    assert!(headers["Location"]["description"].is_string(), "{template}");
    assert!(
        headers["Docker-Content-Digest"]["description"].is_string(),
        "{template}"
    );
}

#[rstest]
#[case::version("/v2/", "get")]
#[case::catalog("/v2/_catalog", "get")]
#[case::manifest_pull("/v2/{name}/manifests/{reference}", "get")]
#[case::blob_pull("/v2/{name}/blobs/{digest}", "get")]
#[case::tags("/v2/{name}/tags/list", "get")]
#[case::referrers("/v2/{name}/referrers/{digest}", "get")]
fn test_challenged_operations_document_the_authenticate_header(#[case] template: &str, #[case] method: &str) {
    let paths = documented_paths();
    let unauthorized = &operation(&paths, template, method)["responses"]["401"];

    assert_eq!(
        unauthorized["content"]["application/json"]["example"]["errors"][0]["code"],
        "UNAUTHORIZED"
    );
    assert!(
        unauthorized["headers"]["WWW-Authenticate"]["description"].is_string(),
        "{template}"
    );
}

#[test]
fn test_the_token_endpoint_documents_its_service_and_scope_query() {
    let paths = documented_paths();
    let token = operation(&paths, "/v2/token", "get");
    let service = token["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|parameter| parameter["name"] == "service")
        .unwrap();

    assert_eq!(parameter_names(token), BTreeSet::from(["service", "scope"]));
    assert_eq!(service["required"], true);
    assert_eq!(
        token["responses"]["200"]["content"]["application/json"]["example"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            &"access_token".to_owned(),
            &"expires_in".to_owned(),
            &"token".to_owned()
        ])
    );
}

#[rstest]
#[case::restore("/v2/{name}/manifests/{reference}/restore", "put")]
#[case::layer_contents("/v2/{name}/blobs/{digest}/contents", "get")]
fn test_peryx_extensions_say_they_are_not_distribution_spec_routes(#[case] template: &str, #[case] method: &str) {
    let paths = documented_paths();
    let description = operation(&paths, template, method)["description"].as_str().unwrap();

    assert!(
        description.starts_with("A peryx extension, not a distribution-spec route"),
        "{description}"
    );
}
