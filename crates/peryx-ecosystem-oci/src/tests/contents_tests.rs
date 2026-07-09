//! peryx's own layer browser: `GET /v2/<name>/blobs/<digest>/contents` lists a stored layer's tar
//! members and previews one text member, reusing the neutral archive engine so the web UI's file
//! browser opens a layer the way it opens a wheel.

use std::io::Write as _;

use axum::http::{Method, StatusCode};

use super::{auth, hosted_writable, oci_digest, proxy, send, send_body};

const TOKEN: &str = "s3cret";

/// A tar carrying one text file and one binary file, so a listing sees both member kinds.
fn tar_layer() -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut builder = tar::Builder::new(&mut bytes);
    append(&mut builder, "app/config.toml", b"name = \"peryx\"\nport = 8080\n");
    append(
        &mut builder,
        "app/logo.png",
        &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
    );
    append(&mut builder, "app/big.txt", &vec![b'x'; 300 * 1024]);
    builder.into_inner().unwrap();
    bytes
}

/// The same tar, gzip-framed, as a real image layer ships.
fn gzip_layer() -> Vec<u8> {
    let mut gz = Vec::new();
    let mut encoder = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
    encoder.write_all(&tar_layer()).unwrap();
    encoder.finish().unwrap();
    gz
}

fn append(builder: &mut tar::Builder<&mut Vec<u8>>, path: &str, bytes: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, path, bytes).unwrap();
}

/// Upload `blob` monolithically to a writable hosted store and return its OCI digest.
async fn upload(app: &axum::Router, blob: &[u8]) -> String {
    let digest = oci_digest(blob);
    let (status, _, _) = send_body(
        app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        blob.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    digest
}

#[tokio::test]
async fn test_contents_lists_a_gzip_layer_members() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload(&app, &gzip_layer()).await;

    let (status, headers, body) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}/contents")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"].to_str().unwrap().contains("json"));
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let members = doc["members"].as_array().unwrap();
    let paths: Vec<&str> = members.iter().filter_map(|m| m["path"].as_str()).collect();
    assert!(paths.contains(&"app/config.toml"), "{paths:?}");
    assert!(paths.contains(&"app/logo.png"), "{paths:?}");
    let toml = members.iter().find(|m| m["path"] == "app/config.toml").unwrap();
    assert_eq!(toml["kind"], "text");
    assert_eq!(toml["previewable"], true);
    let png = members.iter().find(|m| m["path"] == "app/logo.png").unwrap();
    assert_eq!(png["kind"], "binary");
    assert_eq!(png["previewable"], false);
}

#[tokio::test]
async fn test_contents_lists_an_uncompressed_layer() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload(&app, &tar_layer()).await;

    let (status, _, body) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}/contents")).await;
    assert_eq!(status, StatusCode::OK);
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(!doc["members"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_contents_previews_a_text_member() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload(&app, &gzip_layer()).await;

    // An unrecognized query key is ignored rather than rejected.
    let (status, headers, body) = send(
        &app,
        Method::GET,
        &format!("/v2/store/app/blobs/{digest}/contents?member=app%2Fconfig.toml&offset=0&trace=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"].to_str().unwrap().starts_with("text/plain"));
    assert_eq!(headers["x-peryx-member-size"], "29");
    assert_eq!(headers["x-peryx-member-offset"], "0");
    assert!(headers.get("x-peryx-next-offset").is_none());
    assert_eq!(&body[..], b"name = \"peryx\"\nport = 8080\n");
}

#[tokio::test]
async fn test_contents_pages_a_large_member() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload(&app, &gzip_layer()).await;

    // big.txt is 300 KiB, past the 256 KiB chunk, so the first page reports a next offset.
    let (status, headers, body) = send(
        &app,
        Method::GET,
        &format!("/v2/store/app/blobs/{digest}/contents?member=app%2Fbig.txt"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-peryx-member-size"], (300 * 1024).to_string());
    assert_eq!(headers["x-peryx-next-offset"], (256 * 1024).to_string());
    assert_eq!(body.len(), 256 * 1024);
}

#[tokio::test]
async fn test_contents_rejects_a_binary_member_preview() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload(&app, &gzip_layer()).await;

    let (status, _, _) = send(
        &app,
        Method::GET,
        &format!("/v2/store/app/blobs/{digest}/contents?member=app%2Flogo.png"),
    )
    .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn test_contents_missing_member_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload(&app, &gzip_layer()).await;

    let (status, _, _) = send(
        &app,
        Method::GET,
        &format!("/v2/store/app/blobs/{digest}/contents?member=nope.txt"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_contents_of_an_absent_blob_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = oci_digest(b"never uploaded");

    let (status, _, _) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}/contents")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_contents_on_an_unknown_index_route_is_name_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = oci_digest(b"anything");
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/ghost/app/blobs/{digest}/contents")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "NAME_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_contents_of_a_non_sha256_digest_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (status, _, _) = send(&app, Method::GET, "/v2/store/app/blobs/md5:abc/contents").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_contents_of_a_corrupt_layer_is_unprocessable() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload(&app, b"not a tar at all").await;

    let (status, _, _) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}/contents")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn test_contents_offset_past_the_member_is_range_not_satisfiable() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload(&app, &gzip_layer()).await;

    let (status, _, _) = send(
        &app,
        Method::GET,
        &format!("/v2/store/app/blobs/{digest}/contents?member=app%2Fconfig.toml&offset=9999"),
    )
    .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
}

#[tokio::test]
async fn test_contents_tolerates_a_non_numeric_offset() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let digest = upload(&app, &gzip_layer()).await;

    // A garbled offset falls back to 0 rather than failing the request.
    let (status, _, body) = send(
        &app,
        Method::GET,
        &format!("/v2/store/app/blobs/{digest}/contents?member=app%2Fconfig.toml&offset=abc"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"name = \"peryx\"\nport = 8080\n");
}

#[tokio::test]
async fn test_contents_of_a_missing_upstream_layer_is_not_found() {
    // A proxy with no reachable upstream cannot fetch the layer, so the browse reports it absent.
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let digest = oci_digest(b"whatever");
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/hub/app/blobs/{digest}/contents")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
