use std::sync::{Arc, Barrier};

use peryx_identity::{
    ExternalGroup, ExternalGroupGrant, ExternalIdentity, ExternalIdentityLinker, ExternalLinkRequest, ExternalLogin,
    ExternalSubject, GrantScope, ManagedRoleGrant, ProviderId, Role, RoleGrant, UserId, UserName, UserState,
};
use redb::TableDefinition;
use sha2::{Digest as _, Sha256};

use super::store;
use crate::meta::{
    DeleteGrantOutcome, ExternalIdentityStoreError, MetaError, MetaStore, RoleGrantFilter, RoleGrantOrigin,
    RoleGrantQuery,
};

const RAW_EXTERNAL_IDENTITY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("external_identity");
const RAW_EXTERNAL_ROLE_GRANT: TableDefinition<&str, &[u8]> = TableDefinition::new("external_role_grant");

fn identity(provider: &str, subject: &str) -> ExternalIdentity {
    ExternalIdentity::new(
        ProviderId::new(provider).unwrap(),
        ExternalSubject::new(subject).unwrap(),
    )
}

fn request(provider: &str, subject: &str, display_name: &str, grants: Vec<ManagedRoleGrant>) -> ExternalLinkRequest {
    ExternalLinkRequest {
        identity: identity(provider, subject),
        display_name: UserName::new(display_name).unwrap(),
        grants,
    }
}

fn grant(role: Role, scope: GrantScope) -> ManagedRoleGrant {
    ManagedRoleGrant { role, scope }
}

fn repository(name: &str) -> GrantScope {
    GrantScope::Repository { name: name.to_owned() }
}

fn raw_store(setup: impl FnOnce(&redb::WriteTransaction)) -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    setup(&txn);
    txn.commit().unwrap();
    drop(database);
    (dir, MetaStore::open_existing(path).unwrap())
}

fn identity_key(identity: &ExternalIdentity) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"peryx.external-identity.v1\0");
    digest.update(u64::try_from(identity.provider.as_str().len()).unwrap().to_be_bytes());
    digest.update(identity.provider.as_str());
    digest.update(u64::try_from(identity.subject.as_str().len()).unwrap().to_be_bytes());
    digest.update(identity.subject.as_str());
    digest.finalize().into()
}

#[test]
fn test_first_login_persists_one_stable_link_user_and_grants() {
    let (dir, store) = store();
    let external = identity("corporate", "employee-42");
    let first = store
        .link_external_identity(request(
            "corporate",
            "employee-42",
            "Alice",
            vec![grant(Role::RepositoryPublisher, repository("team/api"))],
        ))
        .unwrap();

    assert!(first.link_created);
    assert!(first.grants_changed);
    drop(store);

    let reopened = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    assert_eq!(
        reopened.external_identity_user(&external).unwrap(),
        Some(first.user.id.clone())
    );
    assert_eq!(
        reopened.user_role_grants(&first.user.id).unwrap(),
        vec![peryx_identity::RoleGrant::new(
            first.user.id,
            Role::RepositoryPublisher,
            repository("team/api")
        )]
    );
}

#[test]
fn test_link_owned_grants_use_every_managed_listing_index() {
    let (_dir, store) = store();
    let result = store
        .link_external_identity(request(
            "corporate",
            "employee-42",
            "Alice",
            vec![grant(Role::RepositoryPublisher, repository("team/api"))],
        ))
        .unwrap();
    let scope = repository("team/api");
    let pages = [
        RoleGrantFilter::All,
        RoleGrantFilter::User(result.user.id.clone()),
        RoleGrantFilter::Resource(scope),
    ]
    .map(|filter| {
        store
            .list_managed_grants(&RoleGrantQuery {
                filter,
                cursor: None,
                limit: 10,
            })
            .unwrap()
    });

    assert!(pages.iter().all(|page| page.grants == pages[0].grants));
    assert!(matches!(
        pages[0].grants.as_slice(),
        [stored] if stored.grant.user == result.user.id
            && matches!(stored.origin, RoleGrantOrigin::ExternalIdentity { .. })
    ));
}

#[test]
fn test_link_owned_grants_are_inspectable_but_not_directly_deletable() {
    let (_dir, store) = store();
    store
        .link_external_identity(request(
            "corporate",
            "employee-42",
            "Alice",
            vec![grant(Role::RepositoryPublisher, repository("team/api"))],
        ))
        .unwrap();
    let stored = store
        .list_managed_grants(&RoleGrantQuery {
            filter: RoleGrantFilter::All,
            cursor: None,
            limit: 10,
        })
        .unwrap()
        .grants
        .pop()
        .unwrap();
    assert_eq!(store.managed_grant(&stored.id()).unwrap(), Some(stored.clone()));
    assert!(matches!(
        store.delete_managed_grant(&stored.id(), stored.version).unwrap(),
        DeleteGrantOutcome::ExternallyManaged { link_id }
            if stored.origin == RoleGrantOrigin::ExternalIdentity { link_id: link_id.clone() }
    ));
}

#[test]
fn test_provider_neutral_linker_maps_groups_into_the_atomic_store_operation() {
    let (_dir, store) = store();
    let linker = ExternalIdentityLinker::new(store.clone());
    let login = ExternalLogin::new(
        identity("corporate", "employee-42"),
        UserName::new("Alice").unwrap(),
        vec![ExternalGroup::new("engineering").unwrap()],
    )
    .unwrap();

    let result = linker
        .link_or_resolve(
            &login,
            &[ExternalGroupGrant {
                group: ExternalGroup::new("engineering").unwrap(),
                role: Role::RepositoryPublisher,
                scope: repository("team/api"),
            }],
        )
        .unwrap();

    assert_eq!(store.user_role_grants(&result.user.id).unwrap().len(), 1);
}

#[test]
fn test_repeated_login_resolves_the_same_user_without_writes() {
    let (_dir, store) = store();
    let request = request(
        "corporate",
        "employee-42",
        "Alice",
        vec![grant(Role::Operator, GrantScope::Server)],
    );
    let first = store.link_external_identity(request.clone()).unwrap();

    let second = store.link_external_identity(request).unwrap();

    assert_eq!(second.user.id, first.user.id);
    assert!(!second.link_created);
    assert!(!second.grants_changed);
    assert_eq!(store.user_events(&first.user.id).unwrap().len(), 1);
}

#[test]
fn test_equal_subjects_from_two_providers_create_distinct_users() {
    let (_dir, store) = store();

    let first = store
        .link_external_identity(request("first", "same", "Alice", Vec::new()))
        .unwrap();
    let second = store
        .link_external_identity(request("second", "same", "Alice", Vec::new()))
        .unwrap();

    assert_ne!(first.user.id, second.user.id);
    assert_eq!(first.user.name.display(), "Alice");
    assert_eq!(second.user.name.display(), format!("Alice ({})", second.user.id));
    assert_eq!(second.user.name.canonical(), format!("alice ({})", second.user.id));
}

#[test]
fn test_concurrent_first_login_creates_one_link_and_user() {
    let (_dir, store) = store();
    let barrier = Arc::new(Barrier::new(3));
    let results = std::thread::scope(|scope| {
        let first_store = store.clone();
        let first_barrier = Arc::clone(&barrier);
        let first = scope.spawn(move || {
            first_barrier.wait();
            first_store.link_external_identity(request("corporate", "same", "Alice", Vec::new()))
        });
        let second_store = store.clone();
        let second_barrier = Arc::clone(&barrier);
        let second = scope.spawn(move || {
            second_barrier.wait();
            second_store.link_external_identity(request("corporate", "same", "Alice", Vec::new()))
        });
        barrier.wait();
        [first.join().unwrap().unwrap(), second.join().unwrap().unwrap()]
    });

    assert_eq!(results[0].user.id, results[1].user.id);
    assert_eq!(results.iter().filter(|result| result.link_created).count(), 1);
    assert_eq!(store.user_events(&results[0].user.id).unwrap().len(), 1);
}

#[test]
fn test_group_refresh_replaces_only_managed_grants() {
    let (_dir, store) = store();
    let first = store
        .link_external_identity(request(
            "corporate",
            "employee-42",
            "Alice",
            vec![
                grant(Role::RepositoryReader, repository("team/api")),
                grant(Role::Operator, GrantScope::Server),
            ],
        ))
        .unwrap();
    store
        .grant_role(&first.user.id, Role::RepositoryReader, repository("team/api"))
        .unwrap();
    store
        .grant_role(&first.user.id, Role::RepositoryPublisher, repository("manual"))
        .unwrap();

    let refreshed = store
        .link_external_identity(request(
            "corporate",
            "employee-42",
            "Ignored New Name",
            vec![grant(Role::RepositoryReader, repository("team/api"))],
        ))
        .unwrap();

    assert!(refreshed.grants_changed);
    assert_eq!(refreshed.user.name.display(), "Alice");
    assert_eq!(
        store.user_role_grants(&first.user.id).unwrap(),
        vec![
            peryx_identity::RoleGrant::new(first.user.id.clone(), Role::RepositoryPublisher, repository("manual")),
            peryx_identity::RoleGrant::new(first.user.id, Role::RepositoryReader, repository("team/api")),
        ]
    );
}

#[test]
fn test_empty_group_refresh_removes_managed_authority() {
    let (_dir, store) = store();
    let first = store
        .link_external_identity(request(
            "corporate",
            "employee-42",
            "Alice",
            vec![grant(Role::Operator, GrantScope::Server)],
        ))
        .unwrap();

    let refreshed = store
        .link_external_identity(request("corporate", "employee-42", "Alice", Vec::new()))
        .unwrap();

    assert!(refreshed.grants_changed);
    assert!(store.user_role_grants(&first.user.id).unwrap().is_empty());
    assert!(
        store
            .list_managed_grants(&RoleGrantQuery {
                filter: RoleGrantFilter::User(first.user.id),
                cursor: None,
                limit: 10,
            })
            .unwrap()
            .grants
            .is_empty()
    );
}

#[test]
fn test_open_backfills_existing_link_owned_grants_into_listing_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    let grant = RoleGrant::new(UserId::random(), Role::Operator, GrantScope::Server);
    let bytes = serde_json::to_vec(&grant).unwrap();
    txn.open_table(RAW_EXTERNAL_ROLE_GRANT)
        .unwrap()
        .insert(
            format!("{}/ext_existing/operator/server", grant.user).as_str(),
            bytes.as_slice(),
        )
        .unwrap();
    txn.commit().unwrap();
    drop(database);

    let store = MetaStore::open(&path).unwrap();
    let page = store
        .list_managed_grants(&RoleGrantQuery {
            filter: RoleGrantFilter::All,
            cursor: None,
            limit: 10,
        })
        .unwrap();

    assert!(matches!(
        page.grants.as_slice(),
        [stored] if stored.grant == grant
            && stored.origin == RoleGrantOrigin::ExternalIdentity { link_id: "ext_existing".to_owned() }
    ));
    drop(store);
    assert_eq!(
        MetaStore::open(path)
            .unwrap()
            .list_managed_grants(&RoleGrantQuery {
                filter: RoleGrantFilter::All,
                cursor: None,
                limit: 10,
            })
            .unwrap()
            .grants
            .len(),
        1
    );
}

#[test]
fn test_open_rejects_a_malformed_existing_link_owned_grant_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    let grant = RoleGrant::new(UserId::random(), Role::Operator, GrantScope::Server);
    let bytes = serde_json::to_vec(&grant).unwrap();
    txn.open_table(RAW_EXTERNAL_ROLE_GRANT)
        .unwrap()
        .insert("malformed", bytes.as_slice())
        .unwrap();
    txn.commit().unwrap();
    drop(database);

    assert!(matches!(
        MetaStore::open(path),
        Err(MetaError::MalformedExternalGrantKey { key }) if key == "malformed"
    ));
}

#[test]
fn test_disabled_linked_user_fails_without_changing_grants() {
    let (_dir, store) = store();
    let first = store
        .link_external_identity(request(
            "corporate",
            "employee-42",
            "Alice",
            vec![grant(Role::Operator, GrantScope::Server)],
        ))
        .unwrap();
    store.set_user_state(&first.user.id, UserState::Disabled).unwrap();

    let error = store
        .link_external_identity(request("corporate", "employee-42", "Alice", Vec::new()))
        .unwrap_err();

    assert!(matches!(error, ExternalIdentityStoreError::DisabledUser { id } if id == first.user.id));
    assert_eq!(store.user_role_grants(&first.user.id).unwrap().len(), 1);
}

#[test]
fn test_link_read_treats_missing_old_table_as_empty() {
    let (_dir, store) = raw_store(|_| {});

    assert_eq!(
        store
            .external_identity_user(&identity("corporate", "employee-42"))
            .unwrap(),
        None
    );
}

#[test]
fn test_link_read_returns_none_when_the_index_has_no_matching_key() {
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(RAW_EXTERNAL_IDENTITY).unwrap();
    });

    assert_eq!(
        store
            .external_identity_user(&identity("corporate", "employee-42"))
            .unwrap(),
        None
    );
}

#[test]
fn test_link_operations_surface_incompatible_tables_without_partial_state() {
    let (_dir, incompatible_link) = raw_store(|txn| {
        txn.open_table(TableDefinition::<&str, u64>::new("external_identity"))
            .unwrap();
    });
    assert!(matches!(
        incompatible_link.external_identity_user(&identity("corporate", "employee-42")),
        Err(ExternalIdentityStoreError::Store(MetaError::Table(_)))
    ));

    let (_dir, incompatible_grants) = raw_store(|txn| {
        txn.open_table(TableDefinition::<&str, u64>::new("external_role_grant"))
            .unwrap();
    });
    assert!(matches!(
        incompatible_grants.link_external_identity(request("corporate", "employee-42", "Alice", Vec::new())),
        Err(ExternalIdentityStoreError::Store(MetaError::Table(_)))
    ));
    assert_eq!(incompatible_grants.get_user_by_name("Alice").unwrap(), None);
    assert_eq!(
        incompatible_grants
            .external_identity_user(&identity("corporate", "employee-42"))
            .unwrap(),
        None
    );
}

#[test]
fn test_link_integrity_failures_do_not_expose_or_reassign_subjects() {
    let requested = identity("corporate", "sensitive-subject");
    let conflicting = identity("other", "other-subject");
    let key = identity_key(&requested);
    let bytes = serde_json::to_vec(&serde_json::json!({
        "id": "ext_conflict",
        "identity": conflicting,
        "user_id": peryx_identity::UserId::random(),
    }))
    .unwrap();
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(RAW_EXTERNAL_IDENTITY)
            .unwrap()
            .insert(key.as_slice(), bytes.as_slice())
            .unwrap();
    });

    let read_error = store.external_identity_user(&requested).unwrap_err();
    let write_error = store
        .link_external_identity(ExternalLinkRequest {
            identity: requested,
            display_name: UserName::new("Alice").unwrap(),
            grants: Vec::new(),
        })
        .unwrap_err();

    assert!(matches!(read_error, ExternalIdentityStoreError::KeyCollision));
    assert!(matches!(write_error, ExternalIdentityStoreError::KeyCollision));
    assert!(!format!("{write_error:?}").contains("sensitive-subject"));
}

#[test]
fn test_corrupt_link_record_fails_without_creating_a_user() {
    let requested = identity("corporate", "employee-42");
    let key = identity_key(&requested);
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(RAW_EXTERNAL_IDENTITY)
            .unwrap()
            .insert(key.as_slice(), b"not-json".as_slice())
            .unwrap();
    });

    assert!(matches!(
        store.external_identity_user(&requested),
        Err(ExternalIdentityStoreError::Store(MetaError::Decode(_)))
    ));
    assert!(matches!(
        store.link_external_identity(ExternalLinkRequest {
            identity: requested,
            display_name: UserName::new("Alice").unwrap(),
            grants: Vec::new(),
        }),
        Err(ExternalIdentityStoreError::Store(MetaError::Decode(_)))
    ));
    assert_eq!(store.get_user_by_name("Alice").unwrap(), None);
}

#[test]
fn test_dangling_link_fails_closed() {
    let requested = identity("corporate", "employee-42");
    let missing = peryx_identity::UserId::random();
    let key = identity_key(&requested);
    let bytes = serde_json::to_vec(&serde_json::json!({
        "id": "ext_missing",
        "identity": requested,
        "user_id": missing,
    }))
    .unwrap();
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(RAW_EXTERNAL_IDENTITY)
            .unwrap()
            .insert(key.as_slice(), bytes.as_slice())
            .unwrap();
    });

    assert!(matches!(
        store.link_external_identity(request("corporate", "employee-42", "Alice", Vec::new())),
        Err(ExternalIdentityStoreError::MissingUser { id }) if id == missing
    ));
}
