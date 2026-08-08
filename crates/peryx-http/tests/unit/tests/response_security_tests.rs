use axum::http::{HeaderMap, HeaderValue, header};
use peryx_driver::authz::{AuthorizationService, ScopedDecision};
use peryx_identity::{GrantScope, Resource, Role, Scope, UserId};
use redb::TableDefinition;
use rstest::rstest;

use crate::response_security::{
    ClassifiedField, FieldClassification, ProtectedCachePolicy, ResponseAuthorization, ResponseDenied, filter_fields,
};

#[rstest]
#[case::public(
    None,
    serde_json::json!({"version": "0.0.1"})
)]
#[case::repository_reader(
    Some((Role::RepositoryReader, Scope::RepositoryRead)),
    serde_json::json!({"project": "private-package", "version": "0.0.1"})
)]
#[case::operator(
    Some((Role::Operator, Scope::OperatorRead)),
    serde_json::json!({"queue_depth": 4, "version": "0.0.1"})
)]
#[case::analytics(
    Some((Role::Operator, Scope::AnalyticsRead)),
    serde_json::json!({"queue_depth": 4, "version": "0.0.1"})
)]
#[case::administrator(
    Some((Role::Administrator, Scope::AdministrationRead)),
    serde_json::json!({
        "project": "private-package",
        "queue_depth": 4,
        "signing_key_state": {"configured": true},
        "version": "0.0.1",
    })
)]
#[case::administrator_write(
    Some((Role::Administrator, Scope::AdministrationWrite)),
    serde_json::json!({
        "project": "private-package",
        "queue_depth": 4,
        "signing_key_state": {"configured": true},
        "version": "0.0.1",
    })
)]
fn test_filter_fields_serializes_only_the_callers_classification(
    #[case] role_scope: Option<(Role, Scope)>,
    #[case] expected: serde_json::Value,
) {
    let authorization = role_scope.map_or(ResponseAuthorization::Public, |(role, scope)| {
        ResponseAuthorization::Scoped(authorized(role, scope))
    });
    let filtered = filter_fields(authorization, fixture()).unwrap();

    assert_eq!(serde_json::to_value(filtered).unwrap(), expected);
}

#[test]
fn test_filter_fields_denies_a_role_without_the_checked_scope() {
    assert_response_denied(authorized(Role::RepositoryReader, Scope::OperatorRead));
}

#[test]
fn test_filter_fields_accepts_an_authorized_repository_token() {
    assert_eq!(
        filter_fields(ResponseAuthorization::Repository, fixture()).unwrap(),
        serde_json::from_value(serde_json::json!({"project": "private-package", "version": "0.0.1"})).unwrap()
    );
}

#[test]
fn test_filter_fields_denies_a_server_scope_checked_against_a_repository() {
    assert_response_denied(authorized_on(
        Role::Administrator,
        Scope::AdministrationRead,
        &Resource::Repository("private".to_owned()),
    ));
}

#[test]
fn test_filter_fields_denies_a_storage_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .open_table(TableDefinition::<&str, u64>::new("role_grant"))
        .unwrap();
    transaction.commit().unwrap();
    drop(database);
    let service = AuthorizationService::new(peryx_storage::meta::MetaStore::open_existing(path).unwrap());

    assert_response_denied(service.authorize_scoped(&UserId::random(), Scope::AdministrationRead, &Resource::Operator));
}

#[test]
fn test_response_denial_excludes_request_data() {
    assert_eq!(ResponseDenied.to_string(), "response access denied");
}

#[rstest]
#[case::public(None, Some(FieldClassification::Public))]
#[case::repository_reader(Some((Role::RepositoryReader, Scope::RepositoryRead)), Some(FieldClassification::Repository))]
#[case::operator(Some((Role::Operator, Scope::OperatorRead)), Some(FieldClassification::Operator))]
#[case::administrator(Some((Role::Administrator, Scope::AdministrationRead)), Some(FieldClassification::Administrator))]
#[case::denied(Some((Role::RepositoryReader, Scope::OperatorRead)), None)]
fn test_field_class_reports_the_callers_audience(
    #[case] role_scope: Option<(Role, Scope)>,
    #[case] expected: Option<FieldClassification>,
) {
    let authorization = role_scope.map_or(ResponseAuthorization::Public, |(role, scope)| {
        ResponseAuthorization::Scoped(authorized(role, scope))
    });

    assert_eq!(authorization.field_class(), expected);
}

#[test]
fn test_field_class_of_a_repository_token_is_repository() {
    assert_eq!(
        ResponseAuthorization::Repository.field_class(),
        Some(FieldClassification::Repository)
    );
}

#[rstest]
#[case::private(ProtectedCachePolicy::Private, "private, no-cache")]
#[case::no_store(ProtectedCachePolicy::NoStore, "no-store")]
fn test_protected_cache_policy_sets_cache_control(#[case] policy: ProtectedCachePolicy, #[case] expected: &str) {
    let mut headers = HeaderMap::new();

    policy.apply(&mut headers);

    assert_eq!(headers[header::CACHE_CONTROL], expected);
}

#[test]
fn test_protected_cache_policy_preserves_other_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", HeaderValue::from_static("request-42"));

    ProtectedCachePolicy::NoStore.apply(&mut headers);

    assert_eq!(headers["x-request-id"], "request-42");
}

fn assert_response_denied(authorization: ScopedDecision) {
    assert_eq!(
        filter_fields(ResponseAuthorization::Scoped(authorization), fixture()),
        Err(ResponseDenied)
    );
}

fn authorized(role: Role, scope: Scope) -> ScopedDecision {
    let resource = match scope {
        Scope::RepositoryRead | Scope::RepositoryWrite | Scope::RepositoryDelete => {
            Resource::Repository("private".to_owned())
        }
        Scope::OperatorRead | Scope::AnalyticsRead | Scope::AdministrationRead | Scope::AdministrationWrite => {
            Resource::Operator
        }
    };
    authorized_on(role, scope, &resource)
}

fn authorized_on(role: Role, scope: Scope, resource: &Resource) -> ScopedDecision {
    let dir = tempfile::tempdir().unwrap();
    let store = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let user = store.create_user("Alice").unwrap().id;
    let service = AuthorizationService::new(store);
    service.grant(&user, role, GrantScope::Server).unwrap();
    service.authorize_scoped(&user, scope, resource)
}

fn fixture() -> Vec<ClassifiedField> {
    vec![
        ClassifiedField::new("version", FieldClassification::Public, serde_json::json!("0.0.1")),
        ClassifiedField::new(
            "project",
            FieldClassification::Repository,
            serde_json::json!("private-package"),
        ),
        ClassifiedField::new("queue_depth", FieldClassification::Operator, serde_json::json!(4)),
        ClassifiedField::new(
            "signing_key_state",
            FieldClassification::Administrator,
            serde_json::json!({"configured": true}),
        ),
    ]
}
