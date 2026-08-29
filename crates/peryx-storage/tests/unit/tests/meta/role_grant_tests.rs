use peryx_identity::{GrantScope, Role, RoleGrant, ServerUser, UserId, UserName, UserState};
use redb::TableDefinition;
use rstest::rstest;

use super::store;
use crate::meta::{
    DeleteGrantOutcome, MetaError, MetaStore, RoleGrantFilter, RoleGrantOrigin, RoleGrantQuery, RoleGrantQueryError,
    RoleGrantStoreError, StoredRoleGrant,
};

fn sample_id() -> String {
    StoredRoleGrant {
        grant: RoleGrant::new(UserId::random(), Role::Operator, GrantScope::Server),
        version: 0,
        granted_by: None,
        granted_at_unix: None,
        origin: RoleGrantOrigin::Manual,
    }
    .id()
}

#[test]
fn test_stored_grants_without_an_origin_remain_manual() {
    let grant = RoleGrant::new(UserId::random(), Role::Operator, GrantScope::Server);
    let stored: StoredRoleGrant = serde_json::from_value(serde_json::json!({
        "user": grant.user,
        "role": grant.role,
        "scope": grant.scope,
        "version": 1,
        "granted_by": null,
        "granted_at_unix": null,
    }))
    .unwrap();

    assert_eq!(stored.origin, RoleGrantOrigin::Manual);
}

const RAW_USER: TableDefinition<&str, &[u8]> = TableDefinition::new("server_user");
const RAW_GRANT: TableDefinition<&str, &[u8]> = TableDefinition::new("role_grant");

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

fn persist_user(txn: &redb::WriteTransaction, id: &UserId) {
    let user = ServerUser {
        id: id.clone(),
        name: UserName::new("Alice").unwrap(),
        state: UserState::Active,
        revision: 1,
    };
    let bytes = serde_json::to_vec(&user).unwrap();
    txn.open_table(RAW_USER)
        .unwrap()
        .insert(id.as_str(), bytes.as_slice())
        .unwrap();
}

#[test]
fn test_grants_persist_and_read_back_for_the_user() {
    let (dir, store) = store();
    let alice = store.create_user("Alice").unwrap().id;
    let bob = store.create_user("Bob").unwrap().id;
    let publisher = store
        .grant_role(&alice, Role::RepositoryPublisher, repository("team/api"))
        .unwrap();
    let operator = store.grant_role(&alice, Role::Operator, GrantScope::Server).unwrap();
    store
        .grant_role(&bob, Role::RepositoryReader, repository("team/web"))
        .unwrap();
    drop(store);

    let reopened = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    let alice_grants = reopened.user_role_grants(&alice).unwrap();
    assert_eq!(alice_grants.len(), 2);
    assert!(alice_grants.contains(&publisher));
    assert!(alice_grants.contains(&operator));
    assert_eq!(
        reopened.user_role_grants(&bob).unwrap(),
        vec![RoleGrant::new(bob, Role::RepositoryReader, repository("team/web"))]
    );
}

#[test]
fn test_regranting_the_same_role_and_reach_is_idempotent() {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap().id;

    store
        .grant_role(&alice, Role::RepositoryReader, repository("team/api"))
        .unwrap();
    store
        .grant_role(&alice, Role::RepositoryReader, repository("team/api"))
        .unwrap();

    assert_eq!(store.user_role_grants(&alice).unwrap().len(), 1);
}

#[test]
fn test_reprovisioning_keeps_the_binding_at_version_one() {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap().id;
    let grant = RoleGrant::new(alice.clone(), Role::Operator, GrantScope::Server);

    store.grant_role(&alice, Role::Operator, GrantScope::Server).unwrap();
    store.grant_role(&alice, Role::Operator, GrantScope::Server).unwrap();

    let id = StoredRoleGrant {
        grant,
        version: 0,
        granted_by: None,
        granted_at_unix: None,
        origin: RoleGrantOrigin::Manual,
    }
    .id();
    let stored = store.managed_grant(&id).unwrap().unwrap();
    assert_eq!(stored.version, 1);
    assert_eq!(stored.granted_by, None);
    assert_eq!(stored.granted_at_unix, None);
}

#[test]
fn test_provisioning_preserves_an_already_updated_grant() {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap().id;
    let bob = store.create_user("Bob").unwrap().id;
    let grant = RoleGrant::new(alice.clone(), Role::Operator, GrantScope::Server);

    store.create_managed_grant(&grant, &bob, 1_000).unwrap();
    let updated = store.create_managed_grant(&grant, &bob, 2_000).unwrap().record;
    assert_eq!(updated.version, 2);

    store.grant_role(&alice, Role::Operator, GrantScope::Server).unwrap();

    assert_eq!(store.managed_grant(&updated.id()).unwrap(), Some(updated));
}

#[test]
fn test_revoke_removes_only_the_named_binding_and_reports_presence() {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap().id;
    store
        .grant_role(&alice, Role::RepositoryReader, repository("team/api"))
        .unwrap();
    store.grant_role(&alice, Role::Operator, GrantScope::Server).unwrap();

    assert!(
        store
            .revoke_role(&alice, Role::RepositoryReader, &repository("team/api"))
            .unwrap()
    );
    assert!(
        !store
            .revoke_role(&alice, Role::RepositoryReader, &repository("team/api"))
            .unwrap()
    );

    assert_eq!(
        store.user_role_grants(&alice).unwrap(),
        vec![RoleGrant::new(alice, Role::Operator, GrantScope::Server)]
    );
}

#[test]
fn test_grant_rejects_an_unknown_user() {
    let (_dir, store) = store();
    let missing = UserId::random();

    assert!(matches!(
        store.grant_role(&missing, Role::Administrator, GrantScope::Server),
        Err(RoleGrantStoreError::UnknownUser { id }) if id == missing
    ));
}

#[test]
fn test_grants_read_empty_before_the_table_exists() {
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(RAW_USER).unwrap();
    });

    assert_eq!(store.user_role_grants(&UserId::random()).unwrap(), Vec::new());
}

#[test]
fn test_grant_operations_surface_an_incompatible_table() {
    let alice = UserId::random();
    let (_dir, store) = raw_store(|txn| {
        persist_user(txn, &alice);
        txn.open_table(TableDefinition::<&str, u64>::new("role_grant"))
            .unwrap()
            .insert(alice.as_str(), 1)
            .unwrap();
    });

    assert!(matches!(
        store.grant_role(&alice, Role::Operator, GrantScope::Server),
        Err(RoleGrantStoreError::Store(MetaError::Table(_)))
    ));
    assert!(matches!(store.user_role_grants(&alice), Err(MetaError::Table(_))));
    assert!(matches!(
        store.revoke_role(&alice, Role::Operator, &GrantScope::Server),
        Err(MetaError::Table(_))
    ));
}

#[test]
fn test_grant_read_surfaces_an_incompatible_managed_table() {
    let alice = UserId::random();
    let (_dir, store) = raw_store(|txn| {
        persist_user(txn, &alice);
        txn.open_table(RAW_GRANT).unwrap();
        txn.open_table(TableDefinition::<&str, u64>::new("external_role_grant"))
            .unwrap();
    });

    assert!(matches!(store.user_role_grants(&alice), Err(MetaError::Table(_))));
}

fn all(limit: usize) -> RoleGrantQuery {
    RoleGrantQuery {
        filter: RoleGrantFilter::All,
        cursor: None,
        limit,
    }
}

#[test]
fn test_managed_grant_records_metadata_and_bumps_version_on_reassertion() {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap().id;
    let bob = store.create_user("Bob").unwrap().id;
    let grant = RoleGrant::new(alice.clone(), Role::RepositoryReader, repository("team/api"));

    let first = store.create_managed_grant(&grant, &bob, 1_000).unwrap();
    assert!(first.created);
    assert_eq!(first.record.version, 1);
    assert_eq!(first.record.granted_by, Some(bob));
    assert_eq!(first.record.granted_at_unix, Some(1_000));

    let second = store.create_managed_grant(&grant, &alice, 2_000).unwrap();
    assert!(!second.created);
    assert_eq!(second.record.version, 2);
    assert_eq!(second.record.granted_by, Some(alice));
    assert_eq!(second.record.granted_at_unix, Some(2_000));
    assert_eq!(store.user_role_grants(&grant.user).unwrap(), vec![grant]);
}

#[test]
fn test_managed_grant_rejects_an_unknown_user() {
    let (_dir, store) = store();
    let missing = UserId::random();
    let grant = RoleGrant::new(missing.clone(), Role::Operator, GrantScope::Server);

    assert!(matches!(
        store.create_managed_grant(&grant, &missing, 0),
        Err(RoleGrantStoreError::UnknownUser { id }) if id == missing
    ));
}

#[test]
fn test_managed_grant_rejects_a_disabled_user() {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap().id;
    store.set_user_state(&alice, UserState::Disabled).unwrap();
    let grant = RoleGrant::new(alice.clone(), Role::Operator, GrantScope::Server);

    assert!(matches!(
        store.create_managed_grant(&grant, &alice, 0),
        Err(RoleGrantStoreError::DisabledUser { id }) if id == alice
    ));
}

#[test]
fn test_a_grant_reads_back_by_its_stable_id() {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap().id;
    let grant = RoleGrant::new(alice.clone(), Role::RepositoryPublisher, repository("team/api"));
    let stored = store.create_managed_grant(&grant, &alice, 7).unwrap().record;

    assert_eq!(store.managed_grant(&stored.id()).unwrap(), Some(stored));
    assert_eq!(store.managed_grant("rg_00").unwrap(), None);
}

#[rstest]
#[case::no_prefix("deadbeef")]
#[case::odd_length("rg_0")]
#[case::not_hex("rg_zz")]
#[case::not_utf8("rg_ff")]
fn test_a_malformed_id_reads_and_deletes_as_absent(#[case] id: &str) {
    let (_dir, store) = store();
    assert_eq!(store.managed_grant(id).unwrap(), None);
    assert_eq!(store.delete_managed_grant(id, 0).unwrap(), DeleteGrantOutcome::NotFound);
}

fn id_for(key: &str) -> String {
    use std::fmt::Write as _;
    key.bytes().fold(String::from("rg_"), |mut id, byte| {
        write!(id, "{byte:02x}").unwrap();
        id
    })
}

#[rstest]
#[case::server("usr_x/operator/server", Some(GrantScope::Server))]
#[case::repository("usr_x/repository_reader/repository/team/api", Some(GrantScope::Repository { name: "team/api".to_owned() }))]
#[case::unknown_reach("usr_x/operator/elsewhere", None)]
#[case::empty_repository_name("usr_x/operator/repository/", None)]
#[case::missing_reach("usr_x/operator", None)]
#[case::only_user("usr_x", None)]
fn test_an_id_recovers_the_reach_it_addresses(#[case] key: &str, #[case] expected: Option<GrantScope>) {
    assert_eq!(crate::meta::role_grant_reach(&id_for(key)), expected);
}

#[test]
fn test_a_stored_grant_id_round_trips_to_its_reach() {
    let stored = StoredRoleGrant {
        grant: RoleGrant::new(UserId::random(), Role::RepositoryReader, repository("team/api")),
        version: 0,
        granted_by: None,
        granted_at_unix: None,
        origin: RoleGrantOrigin::Manual,
    };
    assert_eq!(
        crate::meta::role_grant_reach(&stored.id()),
        Some(repository("team/api"))
    );
    assert_eq!(crate::meta::role_grant_reach("not-an-id"), None);
}

#[test]
fn test_revocation_honors_the_version_precondition_and_frees_the_next_read() {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap().id;
    let grant = RoleGrant::new(alice.clone(), Role::RepositoryReader, repository("team/api"));
    let stored = store.create_managed_grant(&grant, &alice, 0).unwrap().record;
    let id = stored.id();

    assert_eq!(
        store.delete_managed_grant(&id, stored.version + 1).unwrap(),
        DeleteGrantOutcome::PreconditionFailed {
            current: stored.version
        }
    );
    assert_eq!(store.user_role_grants(&alice).unwrap(), vec![grant]);

    assert_eq!(
        store.delete_managed_grant(&id, stored.version).unwrap(),
        DeleteGrantOutcome::Removed(stored)
    );
    assert_eq!(store.user_role_grants(&alice).unwrap(), Vec::new());
    assert_eq!(
        store.delete_managed_grant(&id, 0).unwrap(),
        DeleteGrantOutcome::NotFound
    );
}

#[test]
fn test_listing_paginates_every_managed_grant_in_key_order() {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap().id;
    for name in ["team/a", "team/b", "team/c"] {
        store
            .create_managed_grant(
                &RoleGrant::new(alice.clone(), Role::RepositoryReader, repository(name)),
                &alice,
                0,
            )
            .unwrap();
    }

    let first = store.list_managed_grants(&all(2)).unwrap();
    assert_eq!(first.grants.len(), 2);
    let cursor = first.next_cursor.clone();
    assert!(cursor.is_some());

    let second = store
        .list_managed_grants(&RoleGrantQuery {
            filter: RoleGrantFilter::All,
            cursor,
            limit: 2,
        })
        .unwrap();
    assert_eq!(second.grants.len(), 1);
    assert_eq!(second.next_cursor, None);
    assert_eq!(
        first.grants[0].grant.scope,
        GrantScope::Repository {
            name: "team/a".to_owned()
        }
    );
    assert_eq!(
        second.grants[0].grant.scope,
        GrantScope::Repository {
            name: "team/c".to_owned()
        }
    );
}

#[test]
fn test_a_user_filter_scans_only_that_users_grants() {
    let before = UserId::from_stored("usr_00000000000000000000000000000000");
    let selected = UserId::from_stored("usr_50000000000000000000000000000000");
    let after = UserId::from_stored("usr_99999999999999999999999999999999");
    let (_dir, store) = raw_store(|txn| {
        for id in [&before, &selected, &after] {
            persist_user(txn, id);
        }
    });
    for id in [&before, &after] {
        store
            .create_managed_grant(
                &RoleGrant::new(id.clone(), Role::RepositoryReader, repository("unrelated")),
                &selected,
                0,
            )
            .unwrap();
    }
    let expected = ["team/a", "team/b", "team/c"].map(|name| {
        store
            .create_managed_grant(
                &RoleGrant::new(selected.clone(), Role::RepositoryReader, repository(name)),
                &selected,
                0,
            )
            .unwrap()
            .record
    });

    let first = store
        .list_managed_grants(&RoleGrantQuery {
            filter: RoleGrantFilter::User(selected.clone()),
            cursor: None,
            limit: 2,
        })
        .unwrap();
    assert_eq!(first.grants, expected[..2]);
    assert!(first.next_cursor.is_some());

    let second = store
        .list_managed_grants(&RoleGrantQuery {
            filter: RoleGrantFilter::User(selected),
            cursor: first.next_cursor,
            limit: 2,
        })
        .unwrap();
    assert_eq!(second.grants, expected[2..]);
    assert_eq!(second.next_cursor, None);
}

#[rstest]
#[case::before("", 1)]
#[case::after("\u{10ffff}", 0)]
fn test_a_user_filter_clamps_a_cursor_outside_its_range(#[case] cursor: &str, #[case] expected: usize) {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap().id;
    store
        .create_managed_grant(
            &RoleGrant::new(alice.clone(), Role::Operator, GrantScope::Server),
            &alice,
            0,
        )
        .unwrap();

    let page = store
        .list_managed_grants(&RoleGrantQuery {
            filter: RoleGrantFilter::User(alice),
            cursor: Some(cursor.to_owned()),
            limit: 10,
        })
        .unwrap();

    assert_eq!(page.grants.len(), expected);
    assert_eq!(page.next_cursor, None);
}

#[test]
fn test_a_resource_filter_uses_the_scope_index_without_leaking_a_prefix_sibling() {
    let users = [
        UserId::from_stored("usr_10000000000000000000000000000000"),
        UserId::from_stored("usr_50000000000000000000000000000000"),
        UserId::from_stored("usr_90000000000000000000000000000000"),
    ];
    let (_dir, store) = raw_store(|txn| {
        for id in &users {
            persist_user(txn, id);
        }
    });
    for name in ["alpha", "zulu"] {
        store
            .create_managed_grant(
                &RoleGrant::new(users[0].clone(), Role::RepositoryReader, repository(name)),
                &users[0],
                0,
            )
            .unwrap();
    }
    let expected = users.clone().map(|user| {
        store
            .create_managed_grant(
                &RoleGrant::new(user, Role::RepositoryReader, repository("team")),
                &users[0],
                0,
            )
            .unwrap()
            .record
    });

    let first = store
        .list_managed_grants(&RoleGrantQuery {
            filter: RoleGrantFilter::Resource(repository("team")),
            cursor: None,
            limit: 2,
        })
        .unwrap();
    assert_eq!(first.grants, expected[..2]);
    assert!(first.next_cursor.is_some());

    let second = store
        .list_managed_grants(&RoleGrantQuery {
            filter: RoleGrantFilter::Resource(repository("team")),
            cursor: first.next_cursor,
            limit: 2,
        })
        .unwrap();
    assert_eq!(second.grants, expected[2..]);
    assert_eq!(second.next_cursor, None);
}

#[test]
fn test_provisioned_and_revoked_grants_track_the_scope_index() {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap().id;
    store
        .grant_role(&alice, Role::RepositoryPublisher, repository("team/api"))
        .unwrap();

    let query = RoleGrantQuery {
        filter: RoleGrantFilter::Resource(repository("team/api")),
        cursor: None,
        limit: 10,
    };
    assert_eq!(store.list_managed_grants(&query).unwrap().grants.len(), 1);

    store
        .revoke_role(&alice, Role::RepositoryPublisher, &repository("team/api"))
        .unwrap();
    assert_eq!(store.list_managed_grants(&query).unwrap().grants, Vec::new());
}

#[test]
fn test_the_bootstrap_administrator_appears_in_the_server_scope_index() {
    let (_dir, store) = store();
    let verifier = peryx_identity::PasswordPolicy::new(8, 1, 1)
        .unwrap()
        .hash("bootstrap password")
        .unwrap();
    let admin = store.bootstrap_administrator("Root", &verifier).unwrap();

    let page = store
        .list_managed_grants(&RoleGrantQuery {
            filter: RoleGrantFilter::Resource(GrantScope::Server),
            cursor: None,
            limit: 10,
        })
        .unwrap();
    assert_eq!(page.grants.len(), 1);
    assert_eq!(page.grants[0].grant.user, admin.id);
    assert_eq!(page.grants[0].grant.role, Role::Administrator);
}

#[rstest]
#[case::zero(0)]
#[case::above_cap(101)]
fn test_listing_rejects_an_out_of_range_limit(#[case] limit: usize) {
    let (_dir, store) = store();
    assert!(matches!(
        store.list_managed_grants(&all(limit)),
        Err(RoleGrantQueryError::InvalidLimit)
    ));
}

#[test]
fn test_listing_reads_empty_before_any_grant_exists() {
    let (_dir, store) = store();
    let page = store.list_managed_grants(&all(100)).unwrap();
    assert_eq!(page.grants, Vec::new());
    assert_eq!(page.next_cursor, None);
}

#[test]
fn test_reads_are_empty_before_the_grant_tables_exist() {
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(RAW_USER).unwrap();
    });

    assert_eq!(store.managed_grant(&sample_id()).unwrap(), None);
    assert_eq!(store.list_managed_grants(&all(10)).unwrap().grants, Vec::new());
    assert_eq!(
        store
            .list_managed_grants(&RoleGrantQuery {
                filter: RoleGrantFilter::Resource(GrantScope::Server),
                cursor: None,
                limit: 10,
            })
            .unwrap()
            .grants,
        Vec::new()
    );
}

#[test]
fn test_reads_surface_an_incompatible_grant_table() {
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(RAW_USER).unwrap();
        txn.open_table(TableDefinition::<&str, u64>::new("role_grant"))
            .unwrap()
            .insert("x", 1)
            .unwrap();
    });

    assert!(matches!(store.managed_grant(&sample_id()), Err(MetaError::Table(_))));
    assert!(matches!(
        store.list_managed_grants(&all(10)),
        Err(RoleGrantQueryError::Store(MetaError::Table(_)))
    ));
}
