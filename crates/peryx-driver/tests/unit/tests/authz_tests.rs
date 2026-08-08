use peryx_identity::{GrantScope, Resource, Role, Scope, UserId};
use peryx_storage::meta::MetaStore;
use redb::TableDefinition;

use crate::authz::{AuthorizationService, Decision, DenyReason};

fn service() -> (tempfile::TempDir, MetaStore, AuthorizationService) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let service = AuthorizationService::new(store.clone());
    (dir, store, service)
}

fn repository(name: &str) -> GrantScope {
    GrantScope::Repository { name: name.to_owned() }
}

#[test]
fn test_a_covering_grant_allows_and_a_missing_one_denies() {
    let (_dir, store, service) = service();
    let alice = store.create_user("Alice").unwrap().id;
    service
        .grant(&alice, Role::RepositoryPublisher, repository("team/api"))
        .unwrap();

    assert_eq!(
        service.authorize(
            &alice,
            Scope::RepositoryWrite,
            &Resource::Repository("team/api".to_owned())
        ),
        Decision::Allow
    );
    assert!(
        service
            .authorize(
                &alice,
                Scope::RepositoryWrite,
                &Resource::Repository("team/api".to_owned())
            )
            .is_allowed()
    );
    assert_eq!(
        service.authorize(
            &alice,
            Scope::RepositoryWrite,
            &Resource::Repository("team/web".to_owned())
        ),
        Decision::Deny(DenyReason::NoGrant)
    );
    assert_eq!(
        service.authorize(&alice, Scope::OperatorRead, &Resource::Operator),
        Decision::Deny(DenyReason::NoGrant)
    );
}

#[test]
fn test_an_unknown_user_holds_no_grant() {
    let (_dir, _store, service) = service();

    assert_eq!(
        service.authorize(
            &UserId::random(),
            Scope::RepositoryRead,
            &Resource::Repository("team/api".to_owned())
        ),
        Decision::Deny(DenyReason::NoGrant)
    );
}

#[test]
fn test_revoking_a_grant_changes_the_next_decision() {
    let (_dir, store, service) = service();
    let alice = store.create_user("Alice").unwrap().id;
    service.grant(&alice, Role::Operator, GrantScope::Server).unwrap();
    assert!(
        service
            .authorize(&alice, Scope::OperatorRead, &Resource::Operator)
            .is_allowed()
    );

    assert!(service.revoke(&alice, Role::Operator, &GrantScope::Server).unwrap());

    assert_eq!(
        service.authorize(&alice, Scope::OperatorRead, &Resource::Operator),
        Decision::Deny(DenyReason::NoGrant)
    );
    assert!(service.grants(&alice).unwrap().is_empty());
}

#[test]
fn test_a_storage_fault_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(TableDefinition::<&str, u64>::new("role_grant")).unwrap();
    txn.commit().unwrap();
    drop(database);
    let service = AuthorizationService::new(MetaStore::open_existing(path).unwrap());

    let decision = service.authorize(&UserId::random(), Scope::OperatorRead, &Resource::Operator);

    assert_eq!(decision, Decision::Deny(DenyReason::StorageUnavailable));
    assert!(!decision.is_allowed());
}
