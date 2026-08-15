use super::support::*;

#[tokio::test]
async fn test_search_rebuilds_after_delete() {
    let h = harness().await;
    put_uploaded_package(&h.state.serving, "PeryxPkg", "peryxpkg", "Temporary upload");
    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=temporary&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 1);

    h.state
        .serving
        .meta
        .delete_upload(true, "hosted", "peryxpkg", "peryxpkg-1.0-py3-none-any.whl", 0)
        .unwrap();
    h.state.serving.bump_search_epoch();
    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=temporary&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 0);
}
#[tokio::test]
async fn test_scoped_refresh_retires_only_the_mutated_project() {
    let h = harness().await;
    put_uploaded_package(&h.state.serving, "Alpha", "alpha", "shared summary");
    put_uploaded_package(&h.state.serving, "Beta", "beta", "shared summary");

    assert_eq!(search_total(&h.state, "/hosted/+search?q=shared&page_size=25").await, 2);

    h.state
        .serving
        .meta
        .delete_upload(true, "hosted", "beta", "beta-1.0-py3-none-any.whl", 0)
        .unwrap();
    h.state.serving.invalidate_resource("beta");

    assert_eq!(
        search_total(&h.state, "/hosted/+search?q=alpha&page_size=25").await,
        1,
        "alpha was not re-derived and stays searchable"
    );
    assert_eq!(
        search_total(&h.state, "/hosted/+search?q=beta&page_size=25").await,
        0,
        "beta's document was retired incrementally"
    );
}

#[tokio::test]
async fn test_scoped_refresh_re_derives_the_named_project() {
    let h = harness().await;
    put_uploaded_package(&h.state.serving, "Alpha", "alpha", "alpha summary");
    assert_eq!(search_total(&h.state, "/hosted/+search?q=alpha&page_size=25").await, 1);

    h.state.serving.invalidate_resource("alpha");

    assert_eq!(search_total(&h.state, "/hosted/+search?q=alpha&page_size=25").await, 1);
}

#[tokio::test]
async fn test_search_uses_cached_epoch_until_mutation() {
    let h = harness().await;
    put_uploaded_package(&h.state.serving, "PeryxPkg", "peryxpkg", "Temporary upload");

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=temporary&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 1);

    let (status, _headers, body) = get(
        &h.state,
        "/hosted/+search?q=temporary&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 1);
}
#[tokio::test]
async fn test_search_rebuilds_after_yank_and_hide_overrides() {
    let h = harness().await;
    put_cached_package(
        &h.state.serving,
        "pypi/flask",
        "pypi",
        "flask",
        &ProjectDetail {
            meta: Meta::default(),
            name: "Flask".to_owned(),
            versions: vec!["1.0".to_owned()],
            files: vec![File {
                filename: "flask-1.0-py3-none-any.whl".to_owned(),
                url: "https://files.example/flask-1.0-py3-none-any.whl".to_owned(),
                hashes: BTreeMap::from([("sha256".to_owned(), "a".repeat(64))]),
                requires_python: None,
                size: Some(10),
                upload_time: None,
                yanked: Yanked::No,
                core_metadata: CoreMetadata::Absent,
                dist_info_metadata: CoreMetadata::Absent,
                gpg_sig: None,
                provenance: Provenance::Absent,
            }],
        },
    );

    cache::set_yanked(
        &h.state.serving,
        h.state.serving.index_at(2),
        "hosted",
        "flask",
        None,
        Yanked::Yes,
    )
    .await
    .unwrap();
    let (status, _headers, body) = get(
        &h.state,
        "/root/pypi/+search?q=flask&type=override&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["total"], 1);
    assert_eq!(value["results"][0]["type"], "override");

    let trash = cache::TrashContext {
        deleted_at_unix: 0,
        actor: None,
        reason: None,
    };
    cache::remove_files(
        &h.state.serving,
        h.state.serving.index_at(2),
        "hosted",
        true,
        "flask",
        None,
        trash,
    )
    .await
    .unwrap();
    let (status, _headers, body) = get(
        &h.state,
        "/root/pypi/+search?q=flask&type=override&page_size=25",
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["total"], 0);
}
