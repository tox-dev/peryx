//! Publishing to a hosted index through the multipart upload API.

use std::collections::BTreeSet;

use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};

use super::support::*;

#[tokio::test]
async fn test_upload_via_overlay_then_serve_and_download() {
    let h = harness().await;
    let wheel = fixture_wheel();
    assert_eq!(upload_peryxpkg(&h.state, "/root/pypi/", &wheel).await, StatusCode::OK);

    // Served through the virtual index, with the URL on the virtual index route.
    let (ds, _, detail) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(ds, StatusCode::OK);
    assert!(detail.contains("peryxpkg-1.0-py3-none-any.whl"));
    assert!(detail.contains("\"1.0\""));
    let digest = Digest::of(&wheel);
    assert!(detail.contains(&format!("/root/pypi/files/{}/peryxpkg", digest.as_str())));

    let uri = format!("/root/pypi/files/{}/peryxpkg-1.0-py3-none-any.whl", digest.as_str());
    let (fs, _, fbody) = get_bytes(&h.state, &uri, None).await;
    assert_eq!(fs, StatusCode::OK);
    assert_eq!(fbody, wheel);

    // The virtual index's project list includes the uploaded project.
    let (ls, _, list) = get(&h.state, "/root/pypi/simple/", Some("application/json")).await;
    assert_eq!(ls, StatusCode::OK);
    assert!(list.contains("peryxpkg"));
}
#[tokio::test]
async fn test_policy_rejects_upload() {
    let overlay_policy = policy(|neutral, _pypi| {
        neutral.max_file_size_bytes = Some(1);
    });
    let h = harness_with_policies(true, true, Policy::default(), Policy::default(), overlay_policy).await;
    let wheel = fixture_wheel();
    let (content_type, body) = multipart_body(&upload_fields(), Some(("peryxpkg-1.0-py3-none-any.whl", &wheel)));

    let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &content_type, body).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["action"], "upload");
    assert_eq!(denial["project"], "peryxpkg");
    assert_eq!(denial["rule"], "max-file-size");
}
#[tokio::test]
async fn test_upload_direct_to_local_route() {
    let h = harness().await;
    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );
    let (status, _, detail) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("peryxpkg"));
}
#[tokio::test]
async fn test_upload_sdist_gains_metadata_sibling() {
    let h = harness().await;
    let sdist = fixture_sdist();
    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("filetype", "sdist"),
    ];
    let (content_type, body) = multipart_body(&fields, Some(("peryxpkg-1.0.tar.gz", &sdist)));
    assert_eq!(
        post_upload(&h.state, "/hosted/", Some(&upload_auth()), &content_type, body).await,
        StatusCode::OK
    );

    let digest = Digest::of(&sdist);
    let (_, _, detail) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert!(detail.contains("\"core-metadata\":{\"sha256\""));
    let (status, _, body) = get(
        &h.state,
        &format!("/hosted/files/{}/peryxpkg-1.0.tar.gz.metadata", digest.as_str()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with("Metadata-Version: 2.2"));
}
#[tokio::test]
async fn test_upload_sdist_missing_pkg_info_is_bad_request() {
    let h = harness().await;
    let sdist = fixture_sdist_without_pkg_info();
    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("filetype", "sdist"),
    ];
    let (content_type, body) = multipart_body(&fields, Some(("peryxpkg-1.0.tar.gz", &sdist)));
    let (status, body) = post_upload_response(&h.state, "/hosted/", Some(&upload_auth()), &content_type, body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("uploaded content does not match the filename format: invalid sdist: missing required"));
}
#[tokio::test]
async fn test_upload_zip_sdist_is_served_and_listed() {
    let h = harness().await;
    let sdist = fixture_zip_sdist();
    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("filetype", "sdist"),
    ];
    let (content_type, body) = multipart_body(&fields, Some(("peryxpkg-1.0.zip", &sdist)));
    assert_eq!(
        post_upload(&h.state, "/hosted/", Some(&upload_auth()), &content_type, body).await,
        StatusCode::OK
    );

    let digest = Digest::of(&sdist);
    let (status, _, detail) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("peryxpkg-1.0.zip"));
    assert!(detail.contains("\"core-metadata\":{\"sha256\""));

    let uri = format!("/hosted/files/{}/peryxpkg-1.0.zip", digest.as_str());
    let (fs, _, fbody) = get_bytes(&h.state, &uri, None).await;
    assert_eq!(fs, StatusCode::OK);
    assert_eq!(fbody, sdist);

    let (ls, _, list) = get(&h.state, "/hosted/simple/", Some("application/json")).await;
    assert_eq!(ls, StatusCode::OK);
    assert!(list.contains("peryxpkg"));
}
#[tokio::test]
async fn test_upload_zip_sdist_pkg_info_name_mismatch_is_bad_request() {
    let h = harness().await;
    let sdist = fixture_zip_sdist_with_metadata(b"Metadata-Version: 2.2\nName: other\nVersion: 1.0\n");
    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("filetype", "sdist"),
    ];
    let (content_type, body) = multipart_body(&fields, Some(("peryxpkg-1.0.zip", &sdist)));
    let (status, body) = post_upload_response(&h.state, "/hosted/", Some(&upload_auth()), &content_type, body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("metadata Name \"other\" does not match upload name \"peryxpkg\""));
}
#[tokio::test]
async fn test_upload_same_file_is_idempotent() {
    let h = harness().await;
    let wheel = fixture_wheel();
    assert_eq!(upload_peryxpkg(&h.state, "/hosted/", &wheel).await, StatusCode::OK);
    assert_eq!(upload_peryxpkg(&h.state, "/hosted/", &wheel).await, StatusCode::OK);

    let (status, _, detail) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["files"].as_array().unwrap().len(), 1);
}
#[tokio::test]
async fn test_upload_same_filename_with_different_bytes_is_bad_request() {
    let h = harness().await;
    assert_eq!(
        upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await,
        StatusCode::OK
    );
    let wheel = fixture_wheel_with_body("1.0", b"VALUE = 2\n");
    let (ct, body) = multipart_body(&upload_fields(), Some(("peryxpkg-1.0-py3-none-any.whl", &wheel)));

    let (status, body) = post_upload_response(&h.state, "/hosted/", Some(&upload_auth()), &ct, body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        "File already exists: \"peryxpkg-1.0-py3-none-any.whl\" has different content; use a different filename"
    );
}
#[tokio::test]
async fn test_upload_duplicate_content_field_is_bad_request() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let (content_type, body) = multipart_body_with_content_parts(
        &upload_fields(),
        &[
            ("peryxpkg-1.0-py3-none-any.whl", &wheel),
            ("peryxpkg-1.0-py3-none-any.whl", &wheel),
        ],
    );
    let (status, body) = post_upload_response(&h.state, "/hosted/", Some(&upload_auth()), &content_type, body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "bad upload: duplicate content field");
}
#[rstest]
#[case::mirror_route("/pypi/", StatusCode::METHOD_NOT_ALLOWED)]
#[case::unknown_route("/nope/", StatusCode::NOT_FOUND)]
#[case::hosted_subpath("/hosted/simple/", StatusCode::NOT_FOUND)]
#[tokio::test]
async fn test_upload_to_invalid_route_is_rejected(#[case] route: &str, #[case] expected: StatusCode) {
    let h = harness().await;
    assert_eq!(upload_peryxpkg(&h.state, route, b"x").await, expected);
}
#[tokio::test]
async fn test_upload_without_auth_is_unauthorized() {
    let h = harness().await;
    let (ct, body) = multipart_body(&upload_fields(), Some(("x-1.0.whl", b"x")));
    assert_eq!(
        post_upload(&h.state, "/root/pypi/", None, &ct, body).await,
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn test_upload_disabled_without_token_is_forbidden() {
    let h = harness_with(false, true).await;
    assert_eq!(
        upload_peryxpkg(&h.state, "/root/pypi/", b"x").await,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn test_upload_wrong_action_is_bad_request() {
    let h = harness().await;
    let fields = vec![(":action", "submit"), ("name", "x"), ("version", "1.0")];
    let (ct, body) = multipart_body(&fields, Some(("x-1.0.whl", b"x")));
    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await,
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test]
async fn test_upload_missing_field_is_bad_request() {
    let h = harness().await;
    let fields = vec![(":action", "file_upload"), ("version", "1.0")];
    let (ct, body) = multipart_body(&fields, Some(("x-1.0.whl", b"x")));
    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await,
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test]
async fn test_upload_invalid_filename_is_bad_request() {
    let h = harness().await;
    let (ct, body) = multipart_body(&upload_fields(), Some(("peryxpkg/1.0.whl", b"x")));
    let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid filename"));
    assert!(body.contains("without separators"));
}
#[tokio::test]
async fn test_upload_invalid_distribution_filename_is_bad_request() {
    for (filename, expected) in [
        (
            "peryxpkg-1.0.tar.bz2",
            "invalid distribution filename \"peryxpkg-1.0.tar.bz2\": accepted upload formats are .whl, .tar.gz, and .zip",
        ),
        (
            "peryxpkg-1.0.egg",
            "invalid distribution filename \"peryxpkg-1.0.egg\": legacy .egg uploads are not accepted; upload a wheel or .tar.gz sdist",
        ),
        (
            "peryxpkg-1.0-py3-none.whl",
            "invalid distribution filename \"peryxpkg-1.0-py3-none.whl\": wheel filenames must use distribution-version(-build tag)?-python tag-abi tag-platform tag.whl",
        ),
        (
            "peryxpkg.tar.gz",
            "invalid distribution filename \"peryxpkg.tar.gz\": sdist filenames must use name-version.tar.gz",
        ),
        (
            "peryxpkg!-1.0-py3-none-any.whl",
            "invalid distribution filename \"peryxpkg!-1.0-py3-none-any.whl\": distribution name component \"peryxpkg!\" is not a valid PyPA project name",
        ),
        (
            "peryxpkg-bad-py3-none-any.whl",
            "invalid distribution filename \"peryxpkg-bad-py3-none-any.whl\": version component \"bad\" is not a PEP 440 version",
        ),
        (
            "peryxpkg-1.0-py3-*-any.whl",
            "invalid distribution filename \"peryxpkg-1.0-py3-*-any.whl\": wheel build/tag component \"*\" contains invalid characters",
        ),
    ] {
        let h = harness().await;
        let (ct, body) = multipart_body(&upload_fields(), Some((filename, b"x")));
        let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, expected);
    }
}
#[tokio::test]
async fn test_upload_form_validation_errors_include_actionable_body() {
    let h = harness().await;
    for (fields, content, expected_status, expected_body) in [
        (
            vec![(":action", "submit"), ("name", "peryxpkg"), ("version", "1.0")],
            Some(("peryxpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "unsupported :action",
        ),
        (
            vec![(":action", "file_upload"), ("version", "1.0")],
            Some(("peryxpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "missing required field: name",
        ),
        (
            vec![
                (":action", "file_upload"),
                ("name", "peryxpkg"),
                ("version", "1.0"),
                ("filetype", "bdist_wheel"),
            ],
            None,
            StatusCode::BAD_REQUEST,
            "missing required field: content",
        ),
        (
            vec![(":action", "file_upload"), ("name", "-bad"), ("version", "1.0")],
            Some(("peryxpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "invalid project name \"-bad\": names must start and end with an ASCII letter or digit and contain only letters, digits, '.', '_' or '-'",
        ),
        (
            vec![(":action", "file_upload"), ("name", "peryxpkg"), ("version", "bad")],
            Some(("peryxpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "invalid version \"bad\": expected a PEP 440 version",
        ),
        (
            vec![
                (":action", "file_upload"),
                ("name", "peryxpkg"),
                ("version", "1.0"),
                ("filetype", "sdist"),
            ],
            Some(("peryxpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "filetype \"sdist\" does not match filename; expected \"bdist_wheel\"",
        ),
        (
            upload_fields(),
            Some(("other-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "filename project \"other\" does not match upload name \"peryxpkg\"",
        ),
        (
            upload_fields(),
            Some(("peryxpkg-2.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "filename version \"2.0\" does not match upload version \"1.0\"",
        ),
        (
            vec![
                (":action", "file_upload"),
                ("name", "peryxpkg"),
                ("version", "1.0"),
                ("filetype", "bdist_wheel"),
                ("sha256_digest", "00"),
            ],
            Some(("peryxpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "sha256_digest value \"00\" is not lowercase hex with the expected length",
        ),
        (
            vec![
                (":action", "file_upload"),
                ("name", "peryxpkg"),
                ("version", "1.0"),
                ("filetype", "bdist_wheel"),
                ("md5_digest", "d41d8cd98f00b204e9800998ecf8427e"),
            ],
            Some(("peryxpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "md5_digest is not accepted without a sha256_digest or blake2_256_digest",
        ),
    ] {
        assert_upload_response(&h, &fields, content, expected_status, expected_body).await;
    }
}
#[tokio::test]
async fn test_upload_digest_and_requires_python_errors_include_actionable_body() {
    let h = harness().await;
    let wrong = "00".repeat(32);
    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("filetype", "bdist_wheel"),
        ("sha256_digest", wrong.as_str()),
    ];
    assert_upload_response(
        &h,
        &fields,
        Some(("peryxpkg-1.0-py3-none-any.whl", b"x")),
        StatusCode::BAD_REQUEST,
        "sha256_digest mismatch",
    )
    .await;

    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("filetype", "bdist_wheel"),
        ("requires_python", "=>3"),
    ];
    let wheel = fixture_wheel();
    assert_upload_response(
        &h,
        &fields,
        Some(("peryxpkg-1.0-py3-none-any.whl", &wheel)),
        StatusCode::BAD_REQUEST,
        "invalid Requires-Python value \"=>3\": expected PEP 440 version specifiers",
    )
    .await;
}
#[tokio::test]
async fn test_upload_content_validation_errors_include_actionable_body() {
    let h = harness().await;
    assert_upload_response(
        &h,
        &upload_fields(),
        Some(("peryxpkg-1.0-py3-none-any.whl", b"not a zip")),
        StatusCode::BAD_REQUEST,
        "uploaded content does not match the filename format: archive read failed: invalid Zip archive: Could not find EOCD",
    )
    .await;

    assert_upload_response(
        &h,
        &upload_fields(),
        Some((
            "peryxpkg-1.0-py3-none-any.whl",
            fixture_wheel_without_metadata().as_slice(),
        )),
        StatusCode::BAD_REQUEST,
        "uploaded content does not match the filename format: invalid wheel: missing required peryxpkg-1.0.dist-info/METADATA",
    )
    .await;

    assert_upload_response(
        &h,
        &upload_fields(),
        Some((
            "peryxpkg-1.0-py3-none-any.whl",
            fixture_wheel_with_metadata(b"\xff").as_slice(),
        )),
        StatusCode::BAD_REQUEST,
        "artifact metadata is not valid UTF-8",
    )
    .await;

    assert_upload_response(
        &h,
        &upload_fields(),
        Some((
            "peryxpkg-1.0-py3-none-any.whl",
            fixture_wheel_with_metadata(b"Metadata-Version: 2.1\nName: other\nVersion: 1.0\n").as_slice(),
        )),
        StatusCode::BAD_REQUEST,
        "metadata Name \"other\" does not match upload name \"peryxpkg\"",
    )
    .await;

    assert_upload_response(
        &h,
        &upload_fields(),
        Some((
            "peryxpkg-1.0-py3-none-any.whl",
            fixture_wheel_with_metadata(b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 2.0\n").as_slice(),
        )),
        StatusCode::BAD_REQUEST,
        "metadata Version \"2.0\" does not match upload version \"1.0\"",
    )
    .await;

    h.clock.store(i64::MAX, Ordering::Relaxed);
    let wheel = fixture_wheel();
    assert_upload_response(
        &h,
        &upload_fields(),
        Some(("peryxpkg-1.0-py3-none-any.whl", &wheel)),
        StatusCode::INTERNAL_SERVER_ERROR,
        "configured clock produced an invalid upload timestamp",
    )
    .await;
}
#[tokio::test]
async fn test_upload_metadata_form_fields_are_validated() {
    let h = harness().await;
    let fields = vec![
        (":action", "file_upload"),
        ("metadata_version", "2.1"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("requires_python", ">=3.8"),
        ("license", "MIT"),
        ("license_expression", "MIT"),
        ("license_file", "LICENSE"),
        ("provides_extra", "cli"),
        ("project_urls", "Source, https://example.test/source"),
        ("home_page", "https://example.test/home"),
        ("filetype", "bdist_wheel"),
    ];
    let wheel = fixture_wheel_with_metadata(
        b"Metadata-Version: 2.1\nName: peryxpkg\nVersion: 1.0\nRequires-Python: >=3.8\nLicense: MIT\nLicense-Expression: MIT\nLicense-File: LICENSE\nProvides-Extra: cli\nProject-URL: Source, https://example.test/source\nHome-Page: https://example.test/home\n",
    );
    let (content_type, body) = multipart_body(&fields, Some(("peryxpkg-1.0-py3-none-any.whl", &wheel)));
    assert_eq!(
        post_upload(&h.state, "/hosted/", Some(&upload_auth()), &content_type, body).await,
        StatusCode::OK
    );

    let mut fields = fields;
    fields[6] = ("license_expression", "Apache-2.0");
    assert_upload_response(
        &h,
        &fields,
        Some(("peryxpkg-1.0-py3-none-any.whl", &wheel)),
        StatusCode::BAD_REQUEST,
        "metadata License-Expression \"MIT\" does not match upload value \"Apache-2.0\"",
    )
    .await;
}
#[tokio::test]
async fn test_upload_non_utf8_field_is_bad_request() {
    let h = harness().await;
    let mut body = Vec::new();
    body.extend_from_slice(b"--b\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\n");
    body.extend_from_slice(&[0xff, 0xfe]);
    body.extend_from_slice(b"\r\n--b--\r\n");
    let status = post_upload(
        &h.state,
        "/root/pypi/",
        Some(&upload_auth()),
        "multipart/form-data; boundary=b",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
#[tokio::test]
async fn test_upload_large_text_field_is_bad_request() {
    let h = harness().await;
    let large_name = "x".repeat(64 * 1024 + 1);
    let fields = vec![(":action", "file_upload"), ("name", large_name.as_str())];
    let (ct, body) = multipart_body(&fields, Some(("x-1.0.whl", b"x")));
    let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("upload field \"name\" exceeds 65536 bytes"));
}
#[tokio::test]
async fn test_upload_malformed_multipart_is_bad_request() {
    let h = harness().await;
    let body = b"--b\r\nContent-Disposition: form-data; name=\"name\"\r\n".to_vec();
    let (status, body) = post_upload_response(
        &h.state,
        "/root/pypi/",
        Some(&upload_auth()),
        "multipart/form-data; boundary=b",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.starts_with("bad upload: "));
}
#[tokio::test]
async fn test_upload_declared_digest_mismatch_is_bad_request() {
    let h = harness().await;
    let wrong = "00".repeat(32);
    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("sha256_digest", wrong.as_str()),
    ];
    let (ct, body) = multipart_body(&fields, Some(("peryxpkg-1.0.whl", b"bytes")));
    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await,
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test]
async fn test_upload_with_declared_digest_and_extra_field() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let digest = Digest::of(&wheel);
    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("filetype", "bdist_wheel"),
        ("sha256_digest", digest.as_str()),
        ("blake2_256_digest", ""),
        ("summary", "ignored"),
    ];
    let (ct, body) = multipart_body(&fields, Some(("peryxpkg-1.0-py3-none-any.whl", &wheel)));
    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await,
        StatusCode::OK
    );
}
#[tokio::test]
async fn test_upload_rejects_archived_and_quarantined_projects() {
    for status in ["archived", "quarantined"] {
        let h = harness().await;
        let digest = Digest::of(b"upstream");
        let file_url = format!("{}/files/peryxpkg.whl", h.server.uri());
        mount_status_detail(&h.server, "peryxpkg", status, "policy", digest.as_str(), &file_url).await;

        let (content_type, body) = multipart_body(
            &upload_fields(),
            Some(("peryxpkg-1.0-py3-none-any.whl", &fixture_wheel())),
        );
        let (code, body) =
            post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &content_type, body).await;

        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(body, format!("project \"peryxpkg\" is {status}; uploads are disabled"));
    }
}
#[tokio::test]
async fn test_upload_allows_deprecated_project() {
    let h = harness().await;
    let digest = Digest::of(b"upstream");
    let file_url = format!("{}/files/peryxpkg.whl", h.server.uri());
    mount_status_detail(
        &h.server,
        "peryxpkg",
        "deprecated",
        "use another package",
        digest.as_str(),
        &file_url,
    )
    .await;

    assert_eq!(
        upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await,
        StatusCode::OK
    );
}
#[tokio::test]
async fn test_upload_storage_failure_is_server_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("blobs"), b"not a directory").unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let indexes = vec![Index {
        name: "hosted".to_owned(),
        route: "hosted".to_owned(),
        policy: Policy::default(),
        acl: IndexAcl::upload_token("s3cret".to_owned()),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Hosted { volatile: true },
    }];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));
    assert_eq!(
        upload_peryxpkg(&state, "/hosted/", b"data").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}
#[tokio::test]
async fn test_upload_corrupt_existing_record_is_server_error() {
    let h = harness().await;
    h.state
        .meta
        .put_upload("hosted", "peryxpkg", "peryxpkg-1.0-py3-none-any.whl", b"not-json")
        .unwrap();
    let wheel = fixture_wheel();
    let (content_type, body) = multipart_body(&upload_fields(), Some(("peryxpkg-1.0-py3-none-any.whl", &wheel)));

    let (status, body) = post_upload_response(&h.state, "/hosted/", Some(&upload_auth()), &content_type, body).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("upload storage on index \"hosted\" for project \"peryxpkg\""));
    assert!(body.contains("simple API document could not be parsed"));
}
#[tokio::test]
async fn test_upload_target_resolving_to_non_local_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    // A deliberately inconsistent virtual index whose upload target points at the cached.
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
        Index {
            name: "ov".to_owned(),
            route: "ov".to_owned(),
            policy: Policy::default(),
            acl: IndexAcl::default(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Virtual {
                layers: vec![0],
                upload: Some(0),
            },
        },
    ];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));
    assert_eq!(upload_peryxpkg(&state, "/ov/", b"x").await, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_upload_wheel_gains_metadata_sibling() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    // The simple page advertises the extracted PEP 658 sibling, and it is servable.
    let (_, _, detail) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert!(detail.contains("\"core-metadata\":{\"sha256\""));
    let uri = format!(
        "/hosted/files/{}/peryxpkg-1.0-py3-none-any.whl.metadata",
        digest.as_str()
    );
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with("Metadata-Version: 2.1"));
}

#[tokio::test]
async fn test_pypi_maintenance_scans_walk_real_records() {
    use peryx_driver::serving::EcosystemDriver as _;

    let h = harness().await;
    let meta = &h.state.serving.meta;
    let blobs = &h.state.serving.blobs;

    // Import a wheel into the hosted index: exercises the directory walk and seeds an upload record.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("peryxpkg-1.0-py3-none-any.whl"), fixture_wheel()).unwrap();
    let mut imported = Vec::new();
    crate::serving::PypiServing
        .import_dir(meta, blobs, "hosted", "hosted", dir.path(), &mut imported)
        .unwrap();
    assert!(String::from_utf8(imported).unwrap().contains("imported=1"));

    // Cache a project from the mock upstream so the scans also walk a cached page.
    let digest = Digest::of(b"a wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    // Seed one valid record in every remaining metadata table plus a blob, so every maintenance
    // scan walks a non-empty table and its per-record work runs.
    let sibling = Digest::of(b"the metadata sibling");
    blobs.write_verified(b"a wheel", &digest).unwrap();
    meta.put_file_url(digest.as_str(), &file_url, "pypi").unwrap();
    meta.put_metadata(digest.as_str(), &file_url, sibling.as_str(), "pypi")
        .unwrap();
    meta.put_project("pypi", "flask", "Flask").unwrap();
    meta.put_override("hosted", "peryxpkg", "peryxpkg-1.0-py3-none-any.whl", "yanked")
        .unwrap();

    assert!(
        !crate::serving::PypiServing
            .referenced_blob_digests(meta)
            .unwrap()
            .is_empty()
    );
    let mut report = Vec::new();
    crate::serving::PypiServing
        .fsck_metadata(meta, blobs, &mut report)
        .unwrap();
    // Purge the cached `pypi/flask` page so the project-reference walk runs over a real record.
    let report = crate::serving::PypiServing
        .purge_project(meta, "pypi", "flask", true)
        .unwrap();
    assert_eq!(report.project, "flask");
}

#[tokio::test]
async fn test_pypi_policy_dry_run_writes_a_denial() {
    use peryx_driver::serving::EcosystemDriver as _;

    let h = harness_with_policies(
        true,
        true,
        policy(|neutral, _pypi| neutral.block_projects = vec!["flask".to_owned()]),
        Policy::default(),
        Policy::default(),
    )
    .await;
    let digest = Digest::of(b"a wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    // Seed the cached page directly, so it holds flask unfiltered; the dry-run then previews the
    // block that a policy-filtered serve would have hidden.
    h.state
        .serving
        .meta
        .put_index(
            "pypi/flask",
            &crate::store::CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 1000,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: None,
                body: detail_json(digest.as_str(), &file_url).into_bytes(),
            },
        )
        .unwrap();

    let mut out = Vec::new();
    crate::serving::PypiServing
        .policy_dry_run(&h.state.serving.meta, &h.state.indexes, None, None, &mut out)
        .unwrap();
    assert!(
        String::from_utf8(out).unwrap().contains("flask"),
        "a blocked project should appear"
    );
}

/// A hosted index whose one credential is a `ci` token that may write the projects `glob` covers.
fn scoped(glob: &str) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let indexes = vec![Index {
        name: "hosted".to_owned(),
        route: "hosted".to_owned(),
        policy: Policy::default(),
        acl: IndexAcl {
            anonymous_read: true,
            tokens: vec![NamedToken {
                name: "ci".to_owned(),
                secret: "ci-s3cret".to_owned(),
                grants: vec![Grant {
                    projects: vec![Glob::new(glob)],
                    actions: BTreeSet::from([Action::Write]),
                }],
                expires_at: None,
            }],
        },
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Hosted { volatile: true },
    }];
    (dir, crate::tests::wired(AppState::new(meta, blobs, 60, indexes)))
}

fn scoped_auth() -> String {
    format!("Basic {}", STANDARD.encode("__token__:ci-s3cret"))
}

#[tokio::test]
async fn test_a_scoped_token_uploads_a_project_its_glob_covers() {
    let (_dir, state) = scoped("peryx*");
    let (content_type, body) = multipart_body(
        &upload_fields(),
        Some(("peryxpkg-1.0-py3-none-any.whl", &fixture_wheel())),
    );

    let (status, _) = post_upload_response(&state, "/hosted/", Some(&scoped_auth()), &content_type, body).await;

    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_a_scoped_token_may_not_upload_outside_its_glob() {
    let (_dir, state) = scoped("team/*");
    let (content_type, body) = multipart_body(
        &upload_fields(),
        Some(("peryxpkg-1.0-py3-none-any.whl", &fixture_wheel())),
    );

    let (status, body) = post_upload_response(&state, "/hosted/", Some(&scoped_auth()), &content_type, body).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, "token does not grant this action");
}

#[tokio::test]
async fn test_a_write_only_index_refuses_a_delete() {
    let (_dir, state) = scoped("peryx*");

    let status = request(&state, "DELETE", "/hosted/peryxpkg/", Some(&scoped_auth())).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}
