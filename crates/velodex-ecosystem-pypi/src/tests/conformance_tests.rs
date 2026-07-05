use axum::http::StatusCode;
use velodex_storage::blob::Digest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::http_tests::{
    fixture_sdist, fixture_wheel, get, harness, multipart_body, post_upload_response, upload_auth,
};

#[tokio::test]
async fn test_upload_conformance_accepts_wheel_and_sdist_with_metadata() {
    let harness = harness().await;
    let wheel = fixture_wheel();
    let sdist = fixture_sdist();

    for (filename, filetype, bytes) in [
        ("velodexpkg-1.0-py3-none-any.whl", "bdist_wheel", wheel.as_slice()),
        ("velodexpkg-1.0.tar.gz", "sdist", sdist.as_slice()),
    ] {
        let (content_type, body) = multipart_body(
            &[
                (":action", "file_upload"),
                ("name", "velodexpkg"),
                ("version", "1.0"),
                ("filetype", filetype),
            ],
            Some((filename, bytes)),
        );
        let (status, text) =
            post_upload_response(&harness.state, "/hosted/", Some(&upload_auth()), &content_type, body).await;
        assert_eq!((status, text.as_str()), (StatusCode::OK, "upload accepted"));
    }

    let (status, _, body) = get(&harness.state, "/hosted/simple/velodexpkg/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&body).unwrap();
    let files = detail["files"].as_array().unwrap();

    assert_eq!(status, StatusCode::OK);
    for (filename, bytes, metadata_prefix) in [
        (
            "velodexpkg-1.0-py3-none-any.whl",
            wheel.as_slice(),
            "Metadata-Version: 2.1\n",
        ),
        ("velodexpkg-1.0.tar.gz", sdist.as_slice(), "Metadata-Version: 2.2\n"),
    ] {
        let file = files
            .iter()
            .find(|file| file["filename"] == filename)
            .unwrap_or_else(|| panic!("{filename} missing"));
        let metadata_digest = file["core-metadata"]["sha256"].as_str().expect("metadata digest");
        assert_eq!(file["dist-info-metadata"]["sha256"], metadata_digest);

        let artifact_digest = Digest::of(bytes);
        let uri = format!("/hosted/files/{}/{filename}.metadata", artifact_digest.as_str());
        let (status, _, metadata) = get(&harness.state, &uri, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(metadata.starts_with(metadata_prefix));
    }
}

#[tokio::test]
async fn test_upload_conformance_rejects_legacy_and_weak_uploads() {
    let harness = harness().await;
    let wheel = fixture_wheel();

    for (filename, filetype, extra_field, expected) in [
        (
            "velodexpkg-1.0-py3-none-any.egg",
            "bdist_egg",
            None,
            "legacy .egg uploads are not accepted",
        ),
        (
            "velodexpkg-1.0-py3-none-any.whl",
            "bdist_wheel",
            Some(("md5_digest", "d41d8cd98f00b204e9800998ecf8427e")),
            "md5_digest is not accepted",
        ),
    ] {
        let mut fields = vec![
            (":action", "file_upload"),
            ("name", "velodexpkg"),
            ("version", "1.0"),
            ("filetype", filetype),
        ];
        if let Some(field) = extra_field {
            fields.push(field);
        }
        let (content_type, body) = multipart_body(&fields, Some((filename, &wheel)));

        let (status, text) =
            post_upload_response(&harness.state, "/hosted/", Some(&upload_auth()), &content_type, body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(text.contains(expected), "{text}");
    }
}

#[tokio::test]
async fn test_mirror_conformance_preserves_simple_fields_and_serves_eggs() {
    let harness = harness().await;
    let wheel = b"wheel-bytes";
    let egg = b"egg-bytes";
    let wheel_digest = Digest::of(wheel);
    let egg_digest = Digest::of(egg);
    let metadata_digest = Digest::of(b"Metadata-Version: 2.1\nName: flask\n");
    let wheel_url = format!("{}/files/flask.whl", harness.server.uri());
    let egg_url = format!("{}/files/flask.egg", harness.server.uri());
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\",\"project-status\":\"archived\",\
         \"project-status-reason\":\"read only\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{wheel_url}\",\
         \"hashes\":{{\"sha256\":\"{}\"}},\"core-metadata\":{{\"sha256\":\"{}\"}},\
         \"dist-info-metadata\":{{\"sha256\":\"{}\"}},\"yanked\":\"bad build\",\
         \"provenance\":\"https://example.test/flask.provenance\"}},\
         {{\"filename\":\"flask-1.0-py3-none-any.egg\",\"url\":\"{egg_url}\",\
         \"hashes\":{{\"sha256\":\"{}\"}}}}]}}",
        wheel_digest.as_str(),
        metadata_digest.as_str(),
        metadata_digest.as_str(),
        egg_digest.as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(page.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&harness.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.egg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(egg.to_vec()))
        .expect(1)
        .mount(&harness.server)
        .await;

    let (status, _, body) = get(&harness.state, "/pypi/simple/flask/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&body).unwrap();
    let files = detail["files"].as_array().unwrap();
    let wheel = files
        .iter()
        .find(|file| file["filename"] == "flask-1.0-py3-none-any.whl")
        .unwrap();
    let egg = files
        .iter()
        .find(|file| file["filename"] == "flask-1.0-py3-none-any.egg")
        .unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["meta"]["api-version"], "1.4");
    assert_eq!(detail["meta"]["project-status"], "archived");
    assert_eq!(detail["meta"]["project-status-reason"], "read only");
    assert_eq!(wheel["core-metadata"]["sha256"], metadata_digest.as_str());
    assert_eq!(wheel["dist-info-metadata"]["sha256"], metadata_digest.as_str());
    assert_eq!(wheel["yanked"], "bad build");
    assert_eq!(wheel["provenance"], "https://example.test/flask.provenance");
    assert_eq!(egg["core-metadata"], false);
    assert!(egg.get("dist-info-metadata").is_none());

    let egg_download_uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.egg", egg_digest.as_str());
    let (status, _, body) = get(&harness.state, &egg_download_uri, None).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "egg-bytes"));

    let metadata_uri = format!("{egg_download_uri}.metadata");
    let (status, ..) = get(&harness.state, &metadata_uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
