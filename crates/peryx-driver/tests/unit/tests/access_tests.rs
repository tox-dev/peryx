use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, HeaderValue, header};
use peryx_core::Ecosystem;
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken, Principal, Signer};
use peryx_search::{SearchAccess, SearchAccessPattern};
use rstest::rstest;

use crate::access::ReadAccess;
use crate::{AppState, Index, IndexKind};

#[rstest]
#[case::root("", "app")]
#[case::nested("images", "images/app")]
fn test_bearer_read_access_joins_index_routes(#[case] route: &str, #[case] resource: &str) {
    let (_dir, state, headers) = app(route, resource);
    let access = ReadAccess::from_headers(&state, &headers);

    assert_eq!(access.for_index(state.index_at(0)).authorize_project("app"), Ok(()));
}

#[rstest]
#[case::root("", "app")]
#[case::nested("images", "images/app")]
fn test_bearer_read_access_finds_projects_under_index_routes(#[case] route: &str, #[case] resource: &str) {
    let (_dir, state, headers) = app(route, resource);
    let access = ReadAccess::from_headers(&state, &headers);

    assert_eq!(access.for_index(state.index_at(0)).authorize_any_project(), Ok(()));
}

#[rstest]
#[case::lower("bearer")]
#[case::mixed("bEaReR")]
fn test_bearer_read_access_accepts_case_insensitive_scheme(#[case] scheme: &str) {
    let (_dir, state, mut headers) = app("", "app");
    let token = headers[header::AUTHORIZATION]
        .to_str()
        .unwrap()
        .split_once(' ')
        .unwrap()
        .1;
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("{scheme} {token}")).unwrap(),
    );
    let access = ReadAccess::from_headers(&state, &headers);

    assert_eq!(access.for_index(state.index_at(0)).authorize_project("app"), Ok(()));
}

#[rstest]
#[case::any(None)]
#[case::named(Some("app"))]
fn test_public_read_access_allows_projects(#[case] project: Option<&str>) {
    let (_dir, mut state, _) = app("", "app");
    state.indexes[0].acl.anonymous_read = true;
    let access = ReadAccess::from_headers(&state, &HeaderMap::new());
    let access = access.for_index(state.index_at(0));

    assert_eq!(
        project.map_or_else(
            || access.authorize_any_project(),
            |project| access.authorize_project(project),
        ),
        Ok(())
    );
}

#[test]
fn test_missing_credential_denies_private_index_reads() {
    let (_dir, state, _) = app("images", "images/app");
    let access = ReadAccess::from_headers(&state, &HeaderMap::new());
    let access = access.for_index(state.index_at(0));

    assert!(access.authorize_any_project().is_err());
    assert!(access.authorize_project("app").is_err());
}

#[test]
fn test_bearer_search_access_removes_the_index_route() {
    let (_dir, state, headers) = app("images", "images/app");

    assert_eq!(
        ReadAccess::from_headers(&state, &headers).search_access(&state.indexes),
        SearchAccess::new(vec![SearchAccessPattern {
            route: "images".to_owned(),
            glob: "app".to_owned(),
        }])
    );
}

#[test]
fn test_public_search_access_covers_the_whole_index() {
    let (_dir, mut state, _) = app("images", "images/app");
    state.indexes[0].acl.anonymous_read = true;

    assert_eq!(
        ReadAccess::from_headers(&state, &HeaderMap::new()).search_access(&state.indexes),
        SearchAccess::new(vec![SearchAccessPattern {
            route: "images".to_owned(),
            glob: "*".to_owned(),
        }])
    );
}

#[test]
fn test_acl_search_access_uses_the_principals_read_globs() {
    let (_dir, mut state, _) = app("images", "images/app");
    state.indexes[0].acl.tokens.push(NamedToken {
        name: "reader".to_owned(),
        secret: "secret".to_owned(),
        grants: vec![
            Grant {
                projects: vec![Glob::new("ignored/*")],
                actions: BTreeSet::from([Action::Write]),
            },
            Grant {
                projects: vec![Glob::new("team/*")],
                actions: BTreeSet::from([Action::Read]),
            },
        ],
        expires_at: None,
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Basic dXNlcjpzZWNyZXQ="),
    );

    assert_eq!(
        ReadAccess::from_headers(&state, &headers).search_access(&state.indexes),
        SearchAccess::new(vec![SearchAccessPattern {
            route: "images".to_owned(),
            glob: "team/*".to_owned(),
        }])
    );
}

fn app(route: &str, resource: &str) -> (tempfile::TempDir, AppState, HeaderMap) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "images".to_owned(),
            route: route.to_owned(),
            ecosystem: Ecosystem::new("other"),
            kind: IndexKind::Hosted { volatile: true },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl {
                anonymous_read: false,
                tokens: Vec::new(),
            },
        }],
    );
    let signer = Signer::new(b"signing-secret", "peryx");
    let token = signer.mint(
        &Principal::Named {
            subject: "reader".to_owned(),
        },
        &[
            Grant {
                projects: vec![Glob::new("ignored")],
                actions: BTreeSet::from([Action::Write]),
            },
            Grant {
                projects: vec![Glob::new(resource)],
                actions: BTreeSet::from([Action::Read]),
            },
        ],
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .cast_signed(),
        300,
    );
    state.set_token_realm(signer, 300);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    (dir, state, headers)
}
