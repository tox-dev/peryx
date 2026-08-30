use std::collections::BTreeSet;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, header};
use peryx_core::Ecosystem;
use peryx_identity::{
    Action, BasicCredentials, Denial, Glob, Grant, GrantScope, IndexAcl, NamedToken, Principal, ResourceMatch, Role,
    SESSION_COOKIE, ServerUser, SessionSealer, Signer,
};
use peryx_search::{SearchAccess, SearchAccessPattern};
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use crate::access::{HeaderCredential, InvalidCredential, ReadAccess, VerifiedCredential, read_cookie};
use crate::authz::AuthorizationService;
use crate::users::UserService;
use crate::{AppState, Index, IndexKind, ServingState};

const SESSION_KEY: &[u8] = b"a-token-realm-signing-secret-here";
const FAR_FUTURE: i64 = 4_102_444_800;
const FAR_PAST: i64 = 0;

#[rstest]
#[case::root("", "app")]
#[case::nested("images", "images/app")]
fn test_bearer_read_access_joins_index_routes(#[case] route: &str, #[case] resource: &str) {
    let (_dir, state, headers) = app(route, resource);
    let access = ReadAccess::from_headers(&state, &headers);

    assert_eq!(
        access
            .for_index(state.index_at(0))
            .authorize_resource(ResourceMatch::Pattern("app")),
        Ok(())
    );
}

#[rstest]
#[case::root("", "app")]
#[case::nested("images", "images/app")]
fn test_bearer_read_access_finds_resources_under_index_routes(#[case] route: &str, #[case] resource: &str) {
    let (_dir, state, headers) = app(route, resource);
    let access = ReadAccess::from_headers(&state, &headers);

    let access = access.for_index(state.index_at(0));
    assert_eq!(
        (
            access.authorize_any_resource(),
            access.authorize_resource(ResourceMatch::Any),
            access.authorize_resource(ResourceMatch::Exact("app")),
        ),
        (Ok(()), Ok(()), Ok(()))
    );
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

    assert_eq!(
        access
            .for_index(state.index_at(0))
            .authorize_resource(ResourceMatch::Pattern("app")),
        Ok(())
    );
}

#[rstest]
#[case::any(None)]
#[case::named(Some("app"))]
fn test_public_read_access_allows_resources(#[case] resource: Option<&str>) {
    let (_dir, state, _) = app_with_acl(
        "",
        "app",
        IndexAcl {
            anonymous_read: true,
            tokens: Vec::new(),
        },
    );
    let access = ReadAccess::from_headers(&state, &HeaderMap::new());
    let access = access.for_index(state.index_at(0));

    assert_eq!(
        resource.map_or_else(
            || access.authorize_any_resource(),
            |resource| access.authorize_resource(ResourceMatch::Pattern(resource)),
        ),
        Ok(())
    );
}

#[test]
fn test_missing_credential_denies_private_index_reads() {
    let (_dir, state, _) = app("images", "images/app");
    let access = ReadAccess::from_headers(&state, &HeaderMap::new());
    let access = access.for_index(state.index_at(0));

    assert!(access.authorize_any_resource().is_err());
    assert!(access.authorize_resource(ResourceMatch::Pattern("app")).is_err());
}

#[test]
fn test_bearer_search_access_removes_the_index_route() {
    let (_dir, state, headers) = app("images", "images/app");

    assert_eq!(
        ReadAccess::from_headers(&state, &headers).search_access(std::slice::from_ref(state.index_at(0))),
        SearchAccess::new(vec![SearchAccessPattern {
            route: "images".to_owned(),
            glob: "app".to_owned(),
        }])
    );
}

#[test]
fn test_public_search_access_covers_the_whole_index() {
    let (_dir, state, _) = app_with_acl(
        "images",
        "images/app",
        IndexAcl {
            anonymous_read: true,
            tokens: Vec::new(),
        },
    );

    assert_eq!(
        ReadAccess::from_headers(&state, &HeaderMap::new()).search_access(std::slice::from_ref(state.index_at(0))),
        SearchAccess::new(vec![SearchAccessPattern {
            route: "images".to_owned(),
            glob: "*".to_owned(),
        }])
    );
}

#[test]
fn test_acl_search_access_uses_the_principals_read_globs() {
    let (_dir, state, _) = app_with_acl(
        "images",
        "images/app",
        IndexAcl {
            anonymous_read: false,
            tokens: vec![NamedToken {
                name: "reader".to_owned(),
                secret: "secret".to_owned(),
                grants: vec![
                    Grant {
                        resources: vec![Glob::new("ignored/*")],
                        actions: BTreeSet::from([Action::Write]),
                    },
                    Grant {
                        resources: vec![Glob::new("team/*")],
                        actions: BTreeSet::from([Action::Read]),
                    },
                ],
                expires_at: None,
            }],
        },
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Basic dXNlcjpzZWNyZXQ="),
    );

    assert_eq!(
        ReadAccess::from_headers(&state, &headers).search_access(std::slice::from_ref(state.index_at(0))),
        SearchAccess::new(vec![SearchAccessPattern {
            route: "images".to_owned(),
            glob: "team/*".to_owned(),
        }])
    );
}

fn app(route: &str, resource: &str) -> (tempfile::TempDir, Arc<ServingState>, HeaderMap) {
    app_with_acl(
        route,
        resource,
        IndexAcl {
            anonymous_read: false,
            tokens: Vec::new(),
        },
    )
}

fn app_with_acl(route: &str, resource: &str, acl: IndexAcl) -> (tempfile::TempDir, Arc<ServingState>, HeaderMap) {
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
            acl,
        }],
    );
    let signer = Signer::new(b"signing-secret", "peryx");
    let token = signer.mint(
        &Principal::Named {
            subject: "reader".to_owned(),
        },
        &[
            Grant {
                resources: vec![Glob::new("ignored")],
                actions: BTreeSet::from([Action::Write]),
            },
            Grant {
                resources: vec![Glob::new(resource)],
                actions: BTreeSet::from([Action::Read]),
            },
        ],
        4_102_444_500,
        300,
    );
    state.set_token_realm(signer, 300).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    (dir, state.serving, headers)
}

#[rstest]
#[case::server_administrator(Role::Administrator, GrantScope::Server, true)]
#[case::repository_reader(Role::RepositoryReader, repository_scope("images"), true)]
#[case::repository_publisher(Role::RepositoryPublisher, repository_scope("images"), true)]
#[case::another_repository(Role::RepositoryReader, repository_scope("other"), false)]
#[case::operator_only(Role::Operator, GrantScope::Server, false)]
fn test_session_read_access_follows_the_stored_role_grant(
    #[case] role: Role,
    #[case] scope: GrantScope,
    #[case] allowed: bool,
) {
    let (_dir, state) = browser_app(Sealer::Configured);
    let user = state.serving.users.create("Alice").unwrap();
    state.serving.authorization.grant(&user.id, role, scope).unwrap();

    let access = ReadAccess::for_request(&state.serving, &session_cookie(&user, FAR_FUTURE));
    let index = access.for_index(state.serving.index_at(0));

    assert_eq!(
        (
            index.authorize_any_resource().is_ok(),
            index.authorize_resource(ResourceMatch::Pattern("app")).is_ok(),
            index.authorize_resource(ResourceMatch::Exact("app")).is_ok(),
        ),
        (allowed, allowed, allowed)
    );
}

#[test]
fn test_session_search_access_covers_the_whole_granted_repository() {
    let (_dir, state) = browser_app(Sealer::Configured);
    let user = state.serving.users.create("Alice").unwrap();
    state
        .serving
        .authorization
        .grant(&user.id, Role::RepositoryReader, repository_scope("images"))
        .unwrap();

    assert_eq!(
        ReadAccess::for_request(&state.serving, &session_cookie(&user, FAR_FUTURE))
            .search_access(std::slice::from_ref(state.serving.index_at(0))),
        SearchAccess::new(vec![SearchAccessPattern {
            route: "images".to_owned(),
            glob: "*".to_owned(),
        }])
    );
}

#[test]
fn test_session_search_access_omits_an_ungranted_repository() {
    let (_dir, state) = browser_app(Sealer::Configured);
    let user = state.serving.users.create("Alice").unwrap();

    assert_eq!(
        ReadAccess::for_request(&state.serving, &session_cookie(&user, FAR_FUTURE))
            .search_access(std::slice::from_ref(state.serving.index_at(0))),
        SearchAccess::new(Vec::new())
    );
}

#[test]
fn test_session_read_access_applies_grant_removal_on_the_next_request() {
    let (_dir, state) = browser_app(Sealer::Configured);
    let user = state.serving.users.create("Alice").unwrap();
    let scope = repository_scope("images");
    state
        .serving
        .authorization
        .grant(&user.id, Role::RepositoryReader, scope.clone())
        .unwrap();
    let headers = session_cookie(&user, FAR_FUTURE);
    let granted = ReadAccess::for_request(&state.serving, &headers)
        .for_index(state.serving.index_at(0))
        .authorize_any_resource();

    state
        .serving
        .authorization
        .revoke(&user.id, Role::RepositoryReader, &scope)
        .unwrap();

    assert_eq!(
        (
            granted,
            ReadAccess::for_request(&state.serving, &headers)
                .for_index(state.serving.index_at(0))
                .authorize_any_resource()
        ),
        (Ok(()), Err(Denial::Forbidden))
    );
}

#[test]
fn test_session_read_access_rejects_a_disabled_user() {
    let (_dir, state) = browser_app(Sealer::Configured);
    let user = state.serving.users.create("Alice").unwrap();
    state
        .serving
        .authorization
        .grant(&user.id, Role::RepositoryReader, repository_scope("images"))
        .unwrap();
    state.serving.users.disable(&user.id).unwrap();

    assert!(
        ReadAccess::for_request(&state.serving, &session_cookie(&user, FAR_FUTURE))
            .for_index(state.serving.index_at(0))
            .authorize_any_resource()
            .is_err()
    );
}

#[test]
fn test_session_read_access_rejects_a_user_this_server_never_stored() {
    let elsewhere = tempfile::tempdir().unwrap();
    let stranger = UserService::new(MetaStore::open(elsewhere.path().join("peryx.redb")).unwrap())
        .create("Alice")
        .unwrap();
    let (_dir, state) = browser_app(Sealer::Configured);

    assert!(
        ReadAccess::for_request(&state.serving, &session_cookie(&stranger, FAR_FUTURE))
            .for_index(state.serving.index_at(0))
            .authorize_any_resource()
            .is_err()
    );
}

#[rstest]
#[case::expired_cookie(FAR_PAST)]
#[case::live_cookie(FAR_FUTURE)]
fn test_session_read_access_needs_a_live_cookie(#[case] expires_at: i64) {
    let (_dir, state) = browser_app(Sealer::Configured);
    let user = state.serving.users.create("Alice").unwrap();
    state
        .serving
        .authorization
        .grant(&user.id, Role::RepositoryReader, repository_scope("images"))
        .unwrap();

    assert_eq!(
        ReadAccess::for_request(&state.serving, &session_cookie(&user, expires_at))
            .for_index(state.serving.index_at(0))
            .authorize_any_resource()
            .is_ok(),
        expires_at == FAR_FUTURE
    );
}

#[test]
fn test_session_read_access_needs_a_configured_sealer() {
    let (_dir, state) = browser_app(Sealer::Absent);
    let user = state.serving.users.create("Alice").unwrap();
    state
        .serving
        .authorization
        .grant(&user.id, Role::RepositoryReader, repository_scope("images"))
        .unwrap();

    assert!(
        ReadAccess::for_request(&state.serving, &session_cookie(&user, FAR_FUTURE))
            .for_index(state.serving.index_at(0))
            .authorize_any_resource()
            .is_err()
    );
}

#[rstest]
#[case::unreadable_account("server_user")]
#[case::unreadable_grants("role_grant")]
fn test_session_read_access_denies_when_metadata_cannot_be_read(#[case] table: &str) {
    let (_dir, state, user) = poisoned_browser_app(table);

    assert_eq!(
        ReadAccess::for_request(&state.serving, &session_cookie(&user, FAR_FUTURE))
            .for_index(state.serving.index_at(0))
            .authorize_any_resource(),
        Err(Denial::Unavailable)
    );
}

#[test]
fn test_a_rejected_bearer_credential_does_not_fall_back_to_the_session() {
    let (_dir, mut state) = browser_app(Sealer::Configured);
    state
        .set_token_realm(Signer::new(b"signing-secret", "peryx"), 300)
        .unwrap();
    let user = state.serving.users.create("Alice").unwrap();
    state
        .serving
        .authorization
        .grant(&user.id, Role::RepositoryReader, repository_scope("images"))
        .unwrap();
    let mut headers = session_cookie(&user, FAR_FUTURE);
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer forged"));

    assert!(
        ReadAccess::for_request(&state.serving, &headers)
            .for_index(state.serving.index_at(0))
            .authorize_any_resource()
            .is_err()
    );
}

#[test]
fn test_api_read_access_ignores_the_session_cookie() {
    let (_dir, state) = browser_app(Sealer::Configured);
    let user = state.serving.users.create("Alice").unwrap();
    state
        .serving
        .authorization
        .grant(&user.id, Role::RepositoryReader, repository_scope("images"))
        .unwrap();

    assert!(
        ReadAccess::from_headers(&state.serving, &session_cookie(&user, FAR_FUTURE))
            .for_index(state.serving.index_at(0))
            .authorize_any_resource()
            .is_err()
    );
}

#[test]
fn test_session_read_access_leaves_a_public_index_public() {
    let (_dir, state, _) = app_with_acl(
        "images",
        "images/app",
        IndexAcl {
            anonymous_read: true,
            tokens: Vec::new(),
        },
    );

    assert_eq!(
        ReadAccess::for_request(&state, &HeaderMap::new())
            .for_index(state.index_at(0))
            .authorize_any_resource(),
        Ok(())
    );
}

#[test]
fn test_header_credential_reports_an_absent_header() {
    let (_dir, state, _) = app("images", "images/app");

    assert_eq!(
        HeaderCredential::from_headers(&state, &HeaderMap::new()),
        HeaderCredential::Absent
    );
}

#[test]
fn test_header_credential_parses_a_basic_pair() {
    let (_dir, state, _) = app("images", "images/app");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Basic dXNlcjpzZWNyZXQ="),
    );

    assert_eq!(
        HeaderCredential::from_headers(&state, &headers),
        HeaderCredential::Verified(VerifiedCredential::Basic(BasicCredentials {
            user: "user".to_owned(),
            password: "secret".to_owned(),
        }))
    );
}

#[test]
fn test_header_credential_carries_the_grants_of_a_signed_bearer_token() {
    let (_dir, state, headers) = app("images", "images/app");

    assert!(matches!(
        HeaderCredential::from_headers(&state, &headers),
        HeaderCredential::Verified(VerifiedCredential::Bearer(grants))
            if grants.iter().any(|grant| grant.actions.contains(&Action::Read))
    ));
}

#[rstest]
#[case::forged_bearer("Bearer forged", InvalidCredential::Bearer)]
#[case::unknown_scheme("Digest opaque", InvalidCredential::Unsupported)]
#[case::undecodable_basic("Basic not-base64", InvalidCredential::Unsupported)]
fn test_header_credential_rejects_an_unusable_header(#[case] value: &str, #[case] expected: InvalidCredential) {
    let (_dir, state, _) = app("images", "images/app");
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());

    assert_eq!(
        HeaderCredential::from_headers(&state, &headers),
        HeaderCredential::Invalid(expected)
    );
}

#[test]
fn test_header_credential_rejects_a_bearer_token_without_a_token_realm() {
    let (_dir, state) = browser_app(Sealer::Configured);
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer anything"));

    assert_eq!(
        HeaderCredential::from_headers(&state.serving, &headers),
        HeaderCredential::Invalid(InvalidCredential::Bearer)
    );
}

#[test]
fn test_header_credential_rejects_bytes_that_are_not_text() {
    let (_dir, state, _) = app("images", "images/app");
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, HeaderValue::from_bytes(b"Basic \xff").unwrap());

    assert_eq!(
        HeaderCredential::from_headers(&state, &headers),
        HeaderCredential::Invalid(InvalidCredential::Unsupported)
    );
}

#[rstest]
#[case::only_cookie("peryx_session=sealed", Some("sealed"))]
#[case::after_another("theme=dark; peryx_session=sealed", Some("sealed"))]
#[case::before_another("peryx_session=sealed; theme=dark", Some("sealed"))]
#[case::other_name("theme=dark", None)]
#[case::name_prefix("peryx_session_other=sealed", None)]
#[case::valueless("peryx_session", None)]
fn test_read_cookie_selects_the_named_value(#[case] header: &str, #[case] expected: Option<&str>) {
    let mut headers = HeaderMap::new();
    headers.insert(axum::http::header::COOKIE, HeaderValue::from_str(header).unwrap());

    assert_eq!(
        read_cookie(&headers, SESSION_COOKIE).as_deref(),
        expected.map(str::to_owned).as_deref()
    );
}

#[test]
fn test_read_cookie_needs_a_cookie_header() {
    assert_eq!(read_cookie(&HeaderMap::new(), SESSION_COOKIE), None);
}

#[test]
fn test_read_cookie_ignores_bytes_that_are_not_text() {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        HeaderValue::from_bytes(b"peryx_session=\xff").unwrap(),
    );

    assert_eq!(read_cookie(&headers, SESSION_COOKIE), None);
}

fn repository_scope(name: &str) -> GrantScope {
    GrantScope::Repository { name: name.to_owned() }
}

fn session_cookie(user: &ServerUser, expires_at: i64) -> HeaderMap {
    let sealed = SessionSealer::new(SESSION_KEY).seal_session(user, expires_at);
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::COOKIE,
        HeaderValue::from_str(&format!("theme=dark; {SESSION_COOKIE}={sealed}")).unwrap(),
    );
    headers
}

#[derive(Clone, Copy)]
enum Sealer {
    Configured,
    Absent,
}

fn browser_app(sealer: Sealer) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let mut state = private_index_app(MetaStore::open(dir.path().join("peryx.redb")).unwrap(), dir.path());
    if matches!(sealer, Sealer::Configured) {
        state.set_session_sealer(SessionSealer::new(SESSION_KEY)).unwrap();
    }
    (dir, state)
}

/// Replaces one metadata table with a table of another value type, so reads of it fail instead of
/// reporting an empty store.
fn poisoned_browser_app(table: &str) -> (tempfile::TempDir, AppState, ServerUser) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let meta = MetaStore::open(&path).unwrap();
    let user = UserService::new(meta.clone()).create("Alice").unwrap();
    AuthorizationService::new(meta.clone())
        .grant(&user.id, Role::RepositoryReader, repository_scope("images"))
        .unwrap();
    drop(meta);
    let database = redb::Database::open(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .delete_table(redb::TableDefinition::<&str, &[u8]>::new(table))
        .unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, u64>::new(table))
        .unwrap();
    transaction.commit().unwrap();
    drop(database);
    let mut state = private_index_app(MetaStore::open_existing(&path).unwrap(), dir.path());
    state.set_session_sealer(SessionSealer::new(SESSION_KEY)).unwrap();
    (dir, state, user)
}

fn private_index_app(meta: MetaStore, root: &std::path::Path) -> AppState {
    AppState::new(
        meta,
        peryx_storage::blob::BlobStore::new(root.join("blobs")),
        60,
        vec![Index {
            name: "images".to_owned(),
            route: "images".to_owned(),
            ecosystem: Ecosystem::new("other"),
            kind: IndexKind::Hosted { volatile: true },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl {
                anonymous_read: false,
                tokens: Vec::new(),
            },
        }],
    )
}
