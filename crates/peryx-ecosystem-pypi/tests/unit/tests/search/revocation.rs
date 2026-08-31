use super::support::*;

async fn search(state: &Arc<AppState>, uri: &str) -> serde_json::Value {
    let (status, _headers, body) = get(state, uri, Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn test_search_drops_a_project_whose_only_file_is_revoked() {
    let h = harness().await;
    put_uploaded_package(&h.state.serving, "EvilPkg", "evilpkg", "A compromised upload");
    let uri = "/hosted/+search?q=evilpkg&type=uploaded&page_size=25";
    assert_eq!(search(&h.state, uri).await["total"], 1);

    revoke_digest(&h.state, &Digest::of(b"evilpkg-1.0-py3-none-any.whl"));

    assert_eq!(search(&h.state, uri).await["total"], 0);
}

#[tokio::test]
async fn test_search_restores_a_project_when_its_revocation_is_lifted() {
    let h = harness().await;
    put_uploaded_package(&h.state.serving, "LiftedPkg", "liftedpkg", "A cleared upload");
    let digest = Digest::of(b"liftedpkg-1.0-py3-none-any.whl");
    let uri = "/hosted/+search?q=liftedpkg&type=uploaded&page_size=25";
    revoke_digest(&h.state, &digest);
    assert_eq!(search(&h.state, uri).await["total"], 0);

    lift_digest(&h.state, &digest);

    let restored = search(&h.state, uri).await;
    assert_eq!(restored["total"], 1);
    assert_eq!(restored["results"][0]["display_label"], "LiftedPkg");
}

#[tokio::test]
async fn test_search_keeps_a_live_release_when_a_sibling_is_revoked() {
    let h = harness().await;
    put_uploaded_package(&h.state.serving, "MixedPkg", "mixedpkg", "A partly revoked project");
    let revoked = put_uploaded_file(&h.state.serving, "mixedpkg", "2.0");

    revoke_digest(&h.state, &Digest::of(revoked.as_bytes()));

    let project = search(&h.state, "/hosted/+search?q=mixedpkg&type=uploaded&page_size=25").await;
    assert_eq!(project["total"], 1);
    assert_eq!(project["results"][0]["display_label"], "MixedPkg");
    assert_eq!(project["results"][0]["available"], true);
    let by_revoked_filename = search(
        &h.state,
        "/hosted/+search?q=mixedpkg-2.0-py3-none-any.whl&type=uploaded&page_size=25",
    )
    .await;
    assert_eq!(by_revoked_filename["total"], 0);
}

#[tokio::test]
async fn test_search_availability_excludes_a_revoked_hosted_upload() {
    let h = harness().await;
    put_uploaded_package(&h.state.serving, "MixedAvail", "mixedavail", "A revoked local upload");
    put_cached_package(
        &h.state.serving,
        "pypi/mixedavail",
        "pypi",
        "mixedavail",
        &ProjectDetail {
            meta: Meta::default(),
            name: "MixedAvail".to_owned(),
            versions: vec!["2.0".to_owned()],
            files: vec![file_with_hash(
                "mixedavail-2.0-py3-none-any.whl",
                Digest::of(b"mixedavail upstream").as_str(),
                None,
            )],
        },
    );

    revoke_digest(&h.state, &Digest::of(b"mixedavail-1.0-py3-none-any.whl"));

    let all = search(
        &h.state,
        "/root/pypi/+search?q=mixedavail&availability=all&page_size=25",
    )
    .await;
    assert_eq!(all["total"], 1);
    assert_eq!(all["results"][0]["available"], false);
    let local = search(
        &h.state,
        "/root/pypi/+search?q=mixedavail&availability=local&page_size=25",
    )
    .await;
    assert_eq!(local["total"], 0);
}

#[tokio::test]
async fn test_search_keeps_a_mirrored_file_that_carries_no_digest() {
    let h = harness().await;
    put_cached_package(
        &h.state.serving,
        "pypi/nodigest",
        "pypi",
        "nodigest",
        &ProjectDetail {
            meta: Meta::default(),
            name: "NoDigest".to_owned(),
            versions: vec!["1.0".to_owned(), "2.0".to_owned()],
            files: vec![
                file_with_hash(
                    "nodigest-1.0-py3-none-any.whl",
                    Digest::of(b"nodigest wheel").as_str(),
                    None,
                ),
                File {
                    hashes: BTreeMap::new(),
                    ..file_with_hash("nodigest-2.0.tar.gz", "", None)
                },
            ],
        },
    );

    revoke_digest(&h.state, &Digest::of(b"nodigest wheel"));

    let project = search(&h.state, "/pypi/+search?q=nodigest&type=cached&page_size=25").await;
    assert_eq!(project["total"], 1);
    assert_eq!(project["results"][0]["display_label"], "NoDigest");
    let by_kept_filename = search(&h.state, "/pypi/+search?q=nodigest-2.0.tar.gz&type=cached&page_size=25").await;
    assert_eq!(by_kept_filename["total"], 1);
    let by_revoked_filename = search(
        &h.state,
        "/pypi/+search?q=nodigest-1.0-py3-none-any.whl&type=cached&page_size=25",
    )
    .await;
    assert_eq!(by_revoked_filename["total"], 0);
}
