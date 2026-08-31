//! Every route that releases artifact bytes proves the digest belongs to the index in the URL.
//!
//! The blob store and the digest-to-source locator are process-wide, so a digest one index cached
//! used to answer from any other index's route. See #1308.

use std::collections::BTreeSet;

use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};

use super::support::*;

const WHEEL: &str = "peryxpkg-1.0-py3-none-any.whl";

const FOREIGN: &str = "another project's wheel";

/// Two indexes over one process: `private` holds the wheel and answers nobody without its token,
/// `public` never listed the wheel and answers everybody.
fn membership_state(dir: &tempfile::TempDir) -> Arc<AppState> {
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let indexes = vec![
        Index {
            name: "private".to_owned(),
            route: "private".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: IndexAcl {
                anonymous_read: false,
                tokens: vec![NamedToken {
                    name: "owner".to_owned(),
                    secret: "s3cret".to_owned(),
                    grants: vec![Grant {
                        resources: vec![Glob::new("*")],
                        actions: BTreeSet::from([Action::Read, Action::Write]),
                    }],
                    expires_at: None,
                }],
            },
        },
        Index {
            name: "public".to_owned(),
            route: "public".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: UpstreamClient::new("https://example.invalid/simple/").unwrap(),
                offline: true,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
    ];
    crate::tests::wired(AppState::new(meta, blobs, 60, indexes))
}

/// Publish `bytes` as `WHEEL` on the private index, the way an upload does.
fn publish_privately(state: &AppState, bytes: &[u8]) -> Digest {
    let digest = Digest::of(bytes);
    state.serving.blobs.blocking().put_bytes_as(bytes, &digest).unwrap();
    let uploaded = upload_record(
        WHEEL,
        "1.0",
        local_artifact_url("private", digest.as_str(), WHEEL),
        BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
        Some(bytes.len() as u64),
    );
    state
        .serving
        .meta
        .put_upload("private", "peryxpkg", WHEEL, &to_json(&uploaded).into_bytes())
        .unwrap();
    state
        .serving
        .meta
        .put_project("private", "peryxpkg", "peryxpkg")
        .unwrap();
    digest
}

/// What every route answers a pair the addressed index does not publish. The digest and the
/// filename are the caller's own words, so two refusals differ only where the caller differed.
fn not_found_body(route: &str, digest: &str) -> String {
    format!(
        "file download on index {route:?} for file {WHEEL:?} with digest {digest}: \
         no matching cached file or upstream source was found"
    )
}

/// A `HEAD` carries the `GET`'s headers and none of its body.
fn refusal_body(verb: &str, route: &str, digest: &str) -> String {
    if verb == "HEAD" {
        String::new()
    } else {
        not_found_body(route, digest)
    }
}

/// The `If-None-Match` field a request sends: the artifact's own entity tag, or the wildcard.
fn entity_tag(exact: bool, digest: &Digest) -> String {
    if exact {
        format!("\"{}\"", digest.as_str())
    } else {
        "*".to_owned()
    }
}

/// The `If-None-Match` field a request sends: the artifact's own entity tag, or another one.
fn validator_for(matches: bool, digest: &Digest) -> String {
    if matches {
        format!("\"{}\"", digest.as_str())
    } else {
        "\"other\"".to_owned()
    }
}

fn unknown_digest() -> Digest {
    Digest::of(b"nothing ever stored this")
}

/// The bypass: the private wheel's digest, asked for on the public index's file route, used to
/// answer with the private bytes. It now answers exactly as an unknown digest does.
#[rstest]
#[case::get("GET")]
#[case::head("HEAD")]
#[tokio::test]
async fn test_file_route_refuses_a_digest_another_index_published(#[case] verb: &str) {
    let dir = tempfile::tempdir().unwrap();
    let state = membership_state(&dir);
    let wheel = fixture_wheel();
    let digest = publish_privately(&state, &wheel);

    let foreign = send_bytes(&state, verb, &format!("/public/files/{}/{WHEEL}", digest.as_str()), &[]).await;
    let unknown = unknown_digest();
    let absent = send_bytes(
        &state,
        verb,
        &format!("/public/files/{}/{WHEEL}", unknown.as_str()),
        &[],
    )
    .await;

    assert_eq!(
        (foreign.0, String::from_utf8(foreign.2).unwrap()),
        (StatusCode::NOT_FOUND, refusal_body(verb, "public", digest.as_str()))
    );
    assert_eq!(
        (absent.0, String::from_utf8(absent.2).unwrap()),
        (StatusCode::NOT_FOUND, refusal_body(verb, "public", unknown.as_str()))
    );
    assert_eq!(foreign.1, absent.1);
}

/// The index that published the wheel still serves it to a credential that may read it.
#[tokio::test]
async fn test_file_route_serves_the_publishing_index() {
    let dir = tempfile::tempdir().unwrap();
    let state = membership_state(&dir);
    let wheel = fixture_wheel();
    let digest = publish_privately(&state, &wheel);

    let uri = format!("/private/files/{}/{WHEEL}", digest.as_str());
    let (status, _, body) = get_bytes_with_headers(&state, &uri, &[("authorization", &upload_auth())]).await;

    assert_eq!((status, body), (StatusCode::OK, wheel));
}

/// The bytes the public route used to hand out are ones their own index refuses to anonymous
/// callers, which is what makes the cross-index reach a disclosure rather than a routing quirk.
#[tokio::test]
async fn test_publishing_index_refuses_an_unauthorized_read() {
    let dir = tempfile::tempdir().unwrap();
    let state = membership_state(&dir);
    let digest = publish_privately(&state, &fixture_wheel());

    let uri = format!("/private/files/{}/{WHEEL}", digest.as_str());
    let (status, _, _) = get_bytes(&state, &uri, None).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// RFC 9110 s13.1.2 evaluates a validator against a selected representation. A pair the index does
/// not publish selects none, so no condition can turn it into a `304`.
#[rstest]
#[case::get_exact("GET", true)]
#[case::get_wildcard("GET", false)]
#[case::head_exact("HEAD", true)]
#[case::head_wildcard("HEAD", false)]
#[tokio::test]
async fn test_file_route_refuses_a_foreign_digest_before_a_conditional(#[case] verb: &str, #[case] exact: bool) {
    let dir = tempfile::tempdir().unwrap();
    let state = membership_state(&dir);
    let digest = publish_privately(&state, &fixture_wheel());
    let validator = entity_tag(exact, &digest);

    let (status, _, body) = send_bytes(
        &state,
        verb,
        &format!("/public/files/{}/{WHEEL}", digest.as_str()),
        &[("if-none-match", &validator)],
    )
    .await;

    assert_eq!(
        (status, String::from_utf8(body).unwrap()),
        (StatusCode::NOT_FOUND, refusal_body(verb, "public", digest.as_str()))
    );
}

/// A published pair keeps its conditional handling: a matching validator answers `304`, a
/// non-matching one serves the representation.
#[rstest]
#[case::matching(true, StatusCode::NOT_MODIFIED)]
#[case::non_matching(false, StatusCode::OK)]
#[tokio::test]
async fn test_file_route_answers_a_conditional_for_a_published_pair(
    #[case] matches: bool,
    #[case] expected: StatusCode,
) {
    let dir = tempfile::tempdir().unwrap();
    let state = membership_state(&dir);
    let digest = publish_privately(&state, &fixture_wheel());
    let validator = validator_for(matches, &digest);

    let (status, _, _) = get_bytes_with_headers(
        &state,
        &format!("/private/files/{}/{WHEEL}", digest.as_str()),
        &[("authorization", &upload_auth()), ("if-none-match", &validator)],
    )
    .await;

    assert_eq!(status, expected);
}

/// A digest carries its filename. Pairing a published digest with a filename of the caller's
/// choosing - the move that presents policy with a name the stored artifact never had - names a
/// pair the index does not publish.
#[tokio::test]
async fn test_file_route_refuses_a_published_digest_under_another_filename() {
    let dir = tempfile::tempdir().unwrap();
    let state = membership_state(&dir);
    let digest = publish_privately(&state, &fixture_wheel());

    let uri = format!("/private/files/{}/peryxpkg-2.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get_bytes_with_headers(&state, &uri, &[("authorization", &upload_auth())]).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        String::from_utf8(body).unwrap(),
        format!(
            "file download on index \"private\" for file \"peryxpkg-2.0-py3-none-any.whl\" with digest {}: \
             no matching cached file or upstream source was found",
            digest.as_str()
        )
    );
}

/// A sidecar rides on its artifact's digest, so it is refused wherever the artifact is.
#[rstest]
#[case::metadata(".metadata")]
#[case::provenance(".provenance")]
#[tokio::test]
async fn test_sidecar_routes_refuse_a_digest_another_index_published(#[case] suffix: &str) {
    let dir = tempfile::tempdir().unwrap();
    let state = membership_state(&dir);
    let digest = publish_privately(&state, &fixture_wheel());

    let uri = format!("/public/files/{}/{WHEEL}{suffix}", digest.as_str());
    let (status, _, body) = get_bytes(&state, &uri, None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        String::from_utf8(body).unwrap(),
        format!(
            "file download on index \"public\" for file \"{WHEEL}{suffix}\" with digest {}: \
             no matching cached file or upstream source was found",
            digest.as_str()
        )
    );
}

/// Archive inspection reaches the same bytes by digest, so it inherits the same proof.
#[tokio::test]
async fn test_inspect_route_refuses_a_digest_another_index_published() {
    let dir = tempfile::tempdir().unwrap();
    let state = membership_state(&dir);
    let digest = publish_privately(&state, &fixture_wheel());

    let uri = format!("/public/inspect/{}/{WHEEL}", digest.as_str());
    let (status, _, body) = get_bytes(&state, &uri, None).await;

    assert_eq!(
        (status, String::from_utf8(body).unwrap()),
        (StatusCode::NOT_FOUND, not_found_body("public", digest.as_str()))
    );
}

/// The publishing index's own inspection is untouched.
#[tokio::test]
async fn test_inspect_route_serves_the_publishing_index() {
    let dir = tempfile::tempdir().unwrap();
    let state = membership_state(&dir);
    let digest = publish_privately(&state, &fixture_wheel());

    let uri = format!("/private/inspect/{}/{WHEEL}", digest.as_str());
    let (status, _, body) = get_bytes_with_headers(&state, &uri, &[("authorization", &upload_auth())]).await;
    let listing: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(listing["members"][0]["path"], "peryxpkg-1.0.dist-info/METADATA");
}

/// A store that cannot answer whether the index publishes the pair refuses the download rather
/// than falling through to the bytes.
#[tokio::test]
async fn test_file_route_reports_a_publication_store_error() {
    let dir = tempfile::tempdir().unwrap();
    let state = membership_state(&dir);
    let digest = publish_privately(&state, &fixture_wheel());
    state
        .serving
        .meta
        .put_upload("private", "peryxpkg", WHEEL, b"not json")
        .unwrap();

    let uri = format!("/private/files/{}/{WHEEL}", digest.as_str());
    let (status, _, body) = get_bytes_with_headers(&state, &uri, &[("authorization", &upload_auth())]).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        String::from_utf8(body)
            .unwrap()
            .contains("simple API document could not be parsed")
    );
}

/// A cached index's own publication is proof enough, and purging the project withdraws it.
#[tokio::test]
async fn test_purging_a_cached_project_withdraws_its_publication() {
    let dir = tempfile::tempdir().unwrap();
    let state = membership_state(&dir);
    let wheel = fixture_wheel();
    let digest = Digest::of(&wheel);
    state.serving.blobs.blocking().put_bytes_as(&wheel, &digest).unwrap();
    crate::tests::register_publication(&state.serving.meta, "public", WHEEL, digest.as_str(), None);

    let uri = format!("/public/files/{}/{WHEEL}", digest.as_str());
    let (served, _, body) = get_bytes(&state, &uri, None).await;
    state
        .serving
        .meta
        .retire_cached_project("public/peryxpkg", "public", "peryxpkg")
        .unwrap();
    let (purged, _, purged_body) = get_bytes(&state, &uri, None).await;

    assert_eq!((served, body), (StatusCode::OK, wheel));
    assert_eq!(
        (purged, String::from_utf8(purged_body).unwrap()),
        (StatusCode::NOT_FOUND, not_found_body("public", digest.as_str()))
    );
}

/// A virtual index answers for the layer that published the file, and for nothing else.
#[tokio::test]
async fn test_virtual_index_inherits_its_layers_publications() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let digest = upload_wheel(&h.state, WHEEL, &wheel).await;
    let foreign = Digest::of(FOREIGN.as_bytes());
    h.state
        .serving
        .blobs
        .blocking()
        .put_bytes_as(FOREIGN.as_bytes(), &foreign)
        .unwrap();
    h.state
        .serving
        .meta
        .put_file_url(foreign.as_str(), "https://files.example/other.whl", "pypi")
        .unwrap();

    let (layered, _, body) = get_bytes(&h.state, &format!("/root/pypi/files/{}/{WHEEL}", digest.as_str()), None).await;
    let (unlisted, _, _) = get_bytes(
        &h.state,
        &format!("/root/pypi/files/{}/{WHEEL}", foreign.as_str()),
        None,
    )
    .await;

    assert_eq!((layered, body), (StatusCode::OK, wheel));
    assert_eq!(unlisted, StatusCode::NOT_FOUND);
}
