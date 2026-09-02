use std::collections::BTreeSet;

use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};

use super::support::*;

const WHEEL: &str = "peryxpkg-1.0-py3-none-any.whl";

const ROUTES: &[&str] = &[
    "/vault/simple/",
    "/vault/simple/peryxpkg/",
    "/vault/peryxpkg/json",
    "/vault/peryxpkg/1.0/json",
];

fn private_acl(read_resources: &[&str]) -> IndexAcl {
    IndexAcl {
        anonymous_read: false,
        tokens: vec![
            NamedToken {
                name: "uploader".to_owned(),
                secret: "s3cret".to_owned(),
                grants: vec![Grant {
                    resources: vec![Glob::new("*")],
                    actions: BTreeSet::from([Action::Write, Action::Delete]),
                }],
                expires_at: None,
            },
            NamedToken {
                name: "reader".to_owned(),
                secret: "read-secret".to_owned(),
                grants: vec![Grant {
                    resources: read_resources.iter().copied().map(Glob::new).collect(),
                    actions: BTreeSet::from([Action::Read]),
                }],
                expires_at: None,
            },
        ],
    }
}

fn sealed_acl() -> IndexAcl {
    IndexAcl {
        anonymous_read: false,
        tokens: vec![NamedToken {
            name: "uploader".to_owned(),
            secret: "s3cret".to_owned(),
            grants: vec![Grant {
                resources: vec![Glob::new("*")],
                actions: BTreeSet::from([Action::Write]),
            }],
            expires_at: None,
        }],
    }
}

/// A public hosted index, a private one, and a public virtual route layering both, so one state
/// answers for the index a request names and for the layers behind it.
fn state_with(vault: IndexAcl) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let indexes = vec![
        Index {
            name: "open".to_owned(),
            route: "open".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: crate::tests::writer_acl("s3cret"),
        },
        Index {
            name: "vault".to_owned(),
            route: "vault".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: vault,
        },
        Index {
            name: "merged".to_owned(),
            route: "merged".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: vec![1, 0],
                write_target: Some(1),
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
    ];
    (dir, crate::tests::wired(AppState::new(meta, blobs, 60, indexes)))
}

fn reader() -> String {
    format!("Basic {}", STANDARD.encode("__token__:read-secret"))
}

async fn seeded(vault: IndexAcl) -> (tempfile::TempDir, Arc<AppState>, Digest) {
    let (dir, state) = state_with(vault);
    let wheel = fixture_wheel();
    assert_eq!(
        upload_peryxpkg(&state, "/vault/", &wheel).await,
        StatusCode::OK,
        "uploading through the private index needs the write token, not a read grant"
    );
    (dir, state, Digest::of(&wheel))
}

async fn get_bytes_auth(state: &Arc<AppState>, uri: &str, auth: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let headers = auth.map(|auth| (header::AUTHORIZATION.as_str(), auth));
    get_bytes_with_headers(state, uri, headers.as_slice()).await
}

async fn get_with_auth(state: &Arc<AppState>, uri: &str, auth: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let (status, headers, bytes) = get_bytes_auth(state, uri, auth).await;
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

#[rstest]
#[case::listing(ROUTES[0])]
#[case::project(ROUTES[1])]
#[case::legacy_json(ROUTES[2])]
#[case::legacy_release_json(ROUTES[3])]
#[case::listing_redirect("/vault/simple")]
#[case::project_redirect("/vault/simple/peryxpkg")]
#[tokio::test]
async fn test_private_index_page_reads_challenge_an_anonymous_client(#[case] uri: &str) {
    let (_dir, state, _) = seeded(private_acl(&["*"])).await;

    let (status, headers, body) = get_with_auth(&state, uri, None).await;

    assert_eq!(
        (
            status,
            headers[header::WWW_AUTHENTICATE].to_str().unwrap(),
            body.as_str()
        ),
        (StatusCode::UNAUTHORIZED, "Basic realm=\"peryx\"", "unauthorized")
    );
}

#[rstest]
#[case::file("")]
#[case::metadata_sibling(".metadata")]
#[case::provenance(".provenance")]
#[tokio::test]
async fn test_private_index_file_reads_challenge_an_anonymous_client(#[case] suffix: &str) {
    let (_dir, state, digest) = seeded(private_acl(&["*"])).await;
    let uri = format!("/vault/files/{}/{WHEEL}{suffix}", digest.as_str());

    let (status, _, body) = get_with_auth(&state, &uri, None).await;

    assert_eq!((status, body.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
}

#[tokio::test]
async fn test_private_index_archive_inspection_challenges_an_anonymous_client() {
    let (_dir, state, digest) = seeded(private_acl(&["*"])).await;
    let uri = format!("/vault/inspect/{}/{WHEEL}", digest.as_str());

    let (status, _, body) = get_with_auth(&state, &uri, None).await;

    assert_eq!((status, body.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
}

#[tokio::test]
async fn test_private_index_read_refusal_hides_whether_the_project_exists() {
    let (_dir, state, _) = seeded(private_acl(&["*"])).await;

    let present = get_with_auth(&state, "/vault/simple/peryxpkg/", None).await;
    let absent = get_with_auth(&state, "/vault/simple/absent/", None).await;

    assert_eq!((present.0, present.2), (absent.0, absent.2));
}

#[rstest]
#[case::listing(ROUTES[0])]
#[case::project(ROUTES[1])]
#[case::legacy_json(ROUTES[2])]
#[case::legacy_release_json(ROUTES[3])]
#[tokio::test]
async fn test_private_index_pages_serve_a_reader(#[case] uri: &str) {
    let (_dir, state, _) = seeded(private_acl(&["*"])).await;

    let (status, _, body) = get_with_auth(&state, uri, Some(&reader())).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("peryxpkg"), "{body}");
}

#[tokio::test]
async fn test_private_index_files_serve_a_reader() {
    let (_dir, state, digest) = seeded(private_acl(&["*"])).await;
    let uri = format!("/vault/files/{}/{WHEEL}", digest.as_str());

    let (status, _, body) = get_bytes_auth(&state, &uri, Some(&reader())).await;

    assert_eq!((status, body), (StatusCode::OK, fixture_wheel()));
}

#[tokio::test]
async fn test_private_index_archive_inspection_serves_a_reader() {
    let (_dir, state, digest) = seeded(private_acl(&["*"])).await;
    let uri = format!("/vault/inspect/{}/{WHEEL}", digest.as_str());

    let (status, _, body) = get_with_auth(&state, &uri, Some(&reader())).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("METADATA"), "{body}");
}

#[rstest]
#[case::project(ROUTES[1])]
#[case::legacy_json(ROUTES[2])]
#[tokio::test]
async fn test_private_index_forbids_a_reader_scoped_to_another_project(#[case] uri: &str) {
    let (_dir, state, _) = seeded(private_acl(&["other"])).await;

    let (status, _, body) = get_with_auth(&state, uri, Some(&reader())).await;

    assert_eq!(
        (status, body.as_str()),
        (StatusCode::FORBIDDEN, "credential does not grant this read")
    );
}

#[tokio::test]
async fn test_private_index_forbids_a_file_read_outside_a_reader_grant() {
    let (_dir, state, digest) = seeded(private_acl(&["other"])).await;
    let uri = format!("/vault/files/{}/{WHEEL}", digest.as_str());

    let (status, _, body) = get_with_auth(&state, &uri, Some(&reader())).await;

    assert_eq!(
        (status, body.as_str()),
        (StatusCode::FORBIDDEN, "credential does not grant this read")
    );
}

/// A project-scoped reader still lists the index: the listing asks only for a read of something.
#[tokio::test]
async fn test_private_index_lists_for_a_reader_scoped_to_one_project() {
    let (_dir, state, _) = seeded(private_acl(&["other"])).await;

    let (status, _, _) = get_with_auth(&state, ROUTES[0], Some(&reader())).await;

    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_private_index_challenges_a_credential_matching_no_token() {
    let (_dir, state, _) = seeded(private_acl(&["*"])).await;
    let wrong = format!("Basic {}", STANDARD.encode("__token__:not-the-secret"));

    let (status, headers, body) = get_with_auth(&state, ROUTES[1], Some(&wrong)).await;

    assert_eq!(
        (
            status,
            headers[header::WWW_AUTHENTICATE].to_str().unwrap(),
            body.as_str()
        ),
        (StatusCode::UNAUTHORIZED, "Basic realm=\"peryx\"", "unauthorized")
    );
}

/// An index whose tokens grant `read` to nobody answers the same challenge, rather than reporting
/// that no credential could ever read it.
#[tokio::test]
async fn test_index_granting_no_read_challenges_rather_than_reporting_its_acl() {
    let (_dir, state, _) = seeded(sealed_acl()).await;

    let (status, headers, body) = get_with_auth(&state, ROUTES[1], None).await;

    assert_eq!(
        (
            status,
            headers[header::WWW_AUTHENTICATE].to_str().unwrap(),
            body.as_str()
        ),
        (StatusCode::UNAUTHORIZED, "Basic realm=\"peryx\"", "unauthorized")
    );
}

/// A path this dispatch cannot read as a project is refused before it answers its own `400`.
#[rstest]
#[case::unknown_route("/vault/nonsense")]
#[case::undecodable_legacy_json("/vault/%zz/json")]
#[case::unsafe_filename("/vault/files/0000000000000000000000000000000000000000000000000000000000000000/..")]
#[tokio::test]
async fn test_private_index_challenges_before_reporting_a_path_error(#[case] uri: &str) {
    let (_dir, state, _) = seeded(private_acl(&["*"])).await;

    let (status, _, body) = get_with_auth(&state, uri, None).await;

    assert_eq!((status, body.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
}

#[rstest]
#[case::listing("/open/simple/")]
#[case::project("/open/simple/peryxpkg/")]
#[case::legacy_json("/open/peryxpkg/json")]
#[tokio::test]
async fn test_public_index_reads_stay_open_to_an_anonymous_client(#[case] uri: &str) {
    let (_dir, state) = state_with(private_acl(&["*"]));
    assert_eq!(
        upload_peryxpkg(&state, "/open/", &fixture_wheel()).await,
        StatusCode::OK
    );

    let (status, _, body) = get_with_auth(&state, uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("peryxpkg"), "{body}");
}

#[rstest]
#[case::listing("/merged/simple/")]
#[case::project("/merged/simple/peryxpkg/")]
#[tokio::test]
async fn test_virtual_route_over_a_private_layer_challenges_an_anonymous_client(#[case] uri: &str) {
    let (_dir, state, _) = seeded(private_acl(&["*"])).await;

    let (status, _, body) = get_with_auth(&state, uri, None).await;

    assert_eq!((status, body.as_str()), (StatusCode::UNAUTHORIZED, "unauthorized"));
}

#[tokio::test]
async fn test_virtual_route_over_a_private_layer_serves_a_layer_reader() {
    let (_dir, state, _) = seeded(private_acl(&["*"])).await;

    let (status, _, body) = get_with_auth(&state, "/merged/simple/peryxpkg/", Some(&reader())).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(WHEEL), "{body}");
}

#[tokio::test]
async fn test_virtual_route_over_public_layers_stays_open() {
    let (_dir, state) = state_with(crate::tests::writer_acl("s3cret"));
    assert_eq!(
        upload_peryxpkg(&state, "/merged/", &fixture_wheel()).await,
        StatusCode::OK
    );

    let (status, _, body) = get_with_auth(&state, "/merged/simple/peryxpkg/", None).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(WHEEL), "{body}");
}

/// Every documented read this ecosystem guards, with a request that reaches it. `<digest>` stands for
/// the seeded wheel's sha256.
const DOCUMENTED_READS: &[(&str, &str)] = &[
    ("/{route}/simple/", "/vault/simple/"),
    ("/{route}/simple/{project}/", "/vault/simple/peryxpkg/"),
    ("/{route}/{project}/json", "/vault/peryxpkg/json"),
    ("/{route}/{project}/{version}/json", "/vault/peryxpkg/1.0/json"),
    (
        "/{route}/files/{sha256}/{filename}",
        "/vault/files/<digest>/peryxpkg-1.0-py3-none-any.whl",
    ),
    (
        "/{route}/files/{sha256}/{filename}.metadata",
        "/vault/files/<digest>/peryxpkg-1.0-py3-none-any.whl.metadata",
    ),
    (
        "/{route}/inspect/{sha256}/{filename}",
        "/vault/inspect/<digest>/peryxpkg-1.0-py3-none-any.whl",
    ),
    (
        "/{route}/inspect/{sha256}/{filename}/{member}",
        "/vault/inspect/<digest>/peryxpkg-1.0-py3-none-any.whl/peryxpkg-1.0.dist-info/METADATA",
    ),
];

fn guarded_read_templates(paths: &serde_json::Value) -> BTreeSet<&str> {
    paths
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, methods)| {
            methods["get"]["security"].as_array().is_some_and(|requirements| {
                requirements.iter().any(|requirement| {
                    requirement
                        .get(peryx_driver::route_auth::ApiScheme::IndexAccessToken.name())
                        .is_some()
                })
            })
        })
        .map(|(template, _)| template.as_str())
        .collect()
}

/// The document declares an index access token on exactly the reads a private index challenges, and
/// the challenge it sends is the `Basic` scheme that token arrives in. Both sides read one description
/// of what the route takes, and this is what keeps them from drifting apart.
#[tokio::test]
async fn test_documented_read_security_matches_what_a_private_index_challenges() {
    let paths = serde_json::to_value(
        peryx_driver::serving::EcosystemOpenApi::paths(
            &crate::PypiPlugin,
            utoipa::openapi::PathsBuilder::new(),
            peryx_driver::route_auth::ReadExposure::Protected,
        )
        .build(),
    )
    .unwrap();
    let (_dir, state, digest) = seeded(private_acl(&["*"])).await;

    assert_eq!(
        guarded_read_templates(&paths),
        DOCUMENTED_READS.iter().map(|(template, _)| *template).collect()
    );
    for (template, uri) in DOCUMENTED_READS {
        let (status, headers, _) = get_with_auth(&state, &uri.replace("<digest>", digest.as_str()), None).await;
        let challenged = headers[header::WWW_AUTHENTICATE].to_str().unwrap();
        assert_eq!(
            (status, challenged),
            (StatusCode::UNAUTHORIZED, peryx_driver::route_auth::BASIC_CHALLENGE),
            "{template}"
        );
        assert_eq!(
            peryx_driver::route_auth::ApiScheme::IndexAccessToken.auth_scheme(),
            challenged.split(' ').next().unwrap(),
            "{template}"
        );
    }
}
