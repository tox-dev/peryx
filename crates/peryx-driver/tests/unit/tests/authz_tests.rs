use peryx_identity::{GrantScope, Resource, Role, RoleGrant, Scope, UserId};
use peryx_storage::meta::{DeleteGrantOutcome, MetaStore, RoleGrantFilter, RoleGrantQuery};
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

#[test]
fn test_scoped_decision_keeps_scope_and_outcome() {
    let (_dir, store, service) = service();
    let alice = store.create_user("Alice").unwrap().id;
    service.grant(&alice, Role::Operator, GrantScope::Server).unwrap();

    let decision = service.authorize_scoped(&alice, Scope::OperatorRead, &Resource::Operator);

    assert_eq!(decision.scope(), Scope::OperatorRead);
    assert_eq!(decision.decision(), Decision::Allow);
}

#[test]
fn test_managed_grant_lifecycle_uses_versions() {
    let (_dir, store, service) = service();
    let alice = store.create_user("Alice").unwrap().id;
    let operator = store.create_user("Operator").unwrap().id;
    let grant = RoleGrant::new(alice, Role::RepositoryReader, repository("team/api"));

    let created = service.create_managed_grant(&grant, &operator, 41).unwrap();
    let id = created.record.id();

    assert!(created.created);
    assert_eq!(service.managed_grant(&id).unwrap(), Some(created.record.clone()));
    assert_eq!(
        service
            .list_managed_grants(&RoleGrantQuery {
                filter: RoleGrantFilter::All,
                cursor: None,
                limit: 25,
            })
            .unwrap()
            .grants,
        vec![created.record.clone()]
    );
    assert_eq!(
        service.delete_managed_grant(&id, created.record.version + 1).unwrap(),
        DeleteGrantOutcome::PreconditionFailed {
            current: created.record.version
        }
    );
    assert!(matches!(
        service.delete_managed_grant(&id, created.record.version).unwrap(),
        DeleteGrantOutcome::Removed(_)
    ));
    assert_eq!(service.managed_grant(&id).unwrap(), None);
}
