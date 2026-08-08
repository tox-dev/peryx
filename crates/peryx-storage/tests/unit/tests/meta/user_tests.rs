use std::path::Path;
use std::sync::{Arc, Barrier};

use peryx_identity::{PasswordCheck, PasswordPolicy, ServerUser, UserId, UserLifecycleChange, UserName, UserState};
use redb::TableDefinition;

use super::store;
use crate::meta::{MetaError, MetaStore, UserStoreError};

const RAW_DRIVER: TableDefinition<&str, &[u8]> = TableDefinition::new("driver_kv");
const RAW_USER: TableDefinition<&str, &[u8]> = TableDefinition::new("server_user");
const RAW_USER_NAME: TableDefinition<&str, &str> = TableDefinition::new("server_user_name");

fn raw_store(setup: impl FnOnce(&redb::WriteTransaction)) -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    setup(&txn);
    txn.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();
    (dir, store)
}

fn store_with_incompatible_event_table() -> (tempfile::TempDir, MetaStore, ServerUser) {
    let user = ServerUser {
        id: UserId::random(),
        name: UserName::new("Alice").unwrap(),
        state: UserState::Active,
        revision: 1,
    };
    let bytes = serde_json::to_vec(&user).unwrap();
    let (dir, store) = raw_store(|txn| {
        txn.open_table(RAW_USER)
            .unwrap()
            .insert(user.id.as_str(), bytes.as_slice())
            .unwrap();
        txn.open_table(RAW_USER_NAME)
            .unwrap()
            .insert(user.name.canonical(), user.id.as_str())
            .unwrap();
        txn.open_table(TableDefinition::<&str, u64>::new("server_user_event"))
            .unwrap();
    });
    (dir, store, user)
}

fn older_store(path: &Path, incompatible_user_table: bool) {
    let database = redb::Database::create(path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(RAW_DRIVER)
        .unwrap()
        .insert("repository/config", b"preserved".as_slice())
        .unwrap();
    if incompatible_user_table {
        txn.open_table(TableDefinition::<&str, u64>::new("server_user"))
            .unwrap();
    }
    txn.commit().unwrap();
}

#[test]
fn test_user_create_persists_identity_and_event() {
    let (dir, store) = store();
    let user = store.create_user("  Alice Example  ").unwrap();
    drop(store);

    let reopened = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    assert_eq!(reopened.get_user(&user.id).unwrap().as_ref(), Some(&user));
    assert_eq!(
        reopened.get_user_by_name("ALICE EXAMPLE").unwrap().as_ref(),
        Some(&user)
    );
    assert_eq!(
        reopened.user_events(&user.id).unwrap(),
        vec![peryx_identity::UserLifecycleEvent {
            user_id: user.id,
            sequence: 1,
            change: UserLifecycleChange::Created {
                display_name: "Alice Example".to_owned(),
            },
        }]
    );
}

#[test]
fn test_user_create_rejects_one_of_two_concurrent_canonical_duplicates() {
    let (_dir, store) = store();
    let barrier = Arc::new(Barrier::new(3));
    let results = std::thread::scope(|scope| {
        let first_store = store.clone();
        let first_barrier = Arc::clone(&barrier);
        let first = scope.spawn(move || {
            first_barrier.wait();
            first_store.create_user("Élodie")
        });
        let second_store = store.clone();
        let second_barrier = Arc::clone(&barrier);
        let second = scope.spawn(move || {
            second_barrier.wait();
            second_store.create_user("E\u{301}LODIE")
        });
        barrier.wait();
        [first.join().unwrap(), second.join().unwrap()]
    });

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(UserStoreError::DuplicateName { canonical_name }) if canonical_name == "élodie"))
            .count(),
        1
    );
}

#[test]
fn test_user_rename_preserves_id_and_moves_name_index() {
    let (_dir, store) = store();
    let user = store.create_user("Alice").unwrap();

    let renamed = store.rename_user(&user.id, "Alice Cooper").unwrap();

    assert_eq!(renamed.id, user.id);
    assert_eq!(renamed.name.display(), "Alice Cooper");
    assert_eq!(renamed.revision, 2);
    assert!(store.get_user_by_name("Alice").unwrap().is_none());
    assert_eq!(store.get_user_by_name("ALICE COOPER").unwrap(), Some(renamed));
}

#[test]
fn test_user_rename_rejects_duplicate_without_changing_either_user() {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap();
    let bob = store.create_user("Bob").unwrap();

    let error = store.rename_user(&alice.id, "BOB").unwrap_err();

    assert!(matches!(
        error,
        UserStoreError::DuplicateName { canonical_name } if canonical_name == "bob"
    ));
    assert_eq!(store.get_user(&alice.id).unwrap(), Some(alice));
    assert_eq!(store.get_user(&bob.id).unwrap(), Some(bob));
}

#[test]
fn test_user_state_changes_are_isolated() {
    let (_dir, store) = store();
    let alice = store.create_user("Alice").unwrap();
    let bob = store.create_user("Bob").unwrap();

    let disabled = store.set_user_state(&alice.id, UserState::Disabled).unwrap();

    assert_eq!(disabled.state, UserState::Disabled);
    assert_eq!(store.get_user(&bob.id).unwrap(), Some(bob));
    assert_eq!(store.user_events(&alice.id).unwrap().len(), 2);
}

#[test]
fn test_user_reactivation_records_ordered_lifecycle() {
    let (_dir, store) = store();
    let user = store.create_user("Alice").unwrap();
    store.rename_user(&user.id, "Alice Cooper").unwrap();
    store.set_user_state(&user.id, UserState::Disabled).unwrap();

    let active = store.set_user_state(&user.id, UserState::Active).unwrap();
    let events = store.user_events(&user.id).unwrap();

    assert_eq!(active.state, UserState::Active);
    assert_eq!(
        events.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert!(matches!(events[1].change, UserLifecycleChange::Renamed { .. }));
    assert_eq!(events[2].change, UserLifecycleChange::Disabled);
    assert_eq!(events[3].change, UserLifecycleChange::Reactivated);
}

#[test]
fn test_user_operations_reject_empty_names_and_unknown_ids() {
    let (_dir, store) = store();
    let missing = peryx_identity::UserId::random();

    assert!(matches!(store.create_user("  "), Err(UserStoreError::Name(_))));
    assert!(matches!(store.get_user_by_name("\n"), Err(UserStoreError::Name(_))));
    assert!(matches!(
        store.rename_user(&missing, "Alice"),
        Err(UserStoreError::NotFound { id }) if id == missing
    ));
    assert!(matches!(
        store.set_user_state(&missing, UserState::Disabled),
        Err(UserStoreError::NotFound { id }) if id == missing
    ));
    let verifier = PasswordPolicy::new(8, 1, 1).unwrap().hash("s3cret").unwrap();
    assert!(matches!(
        store.set_user_password(&missing, &verifier),
        Err(UserStoreError::NotFound { id }) if id == missing
    ));
    assert!(matches!(
        store.clear_user_password(&missing),
        Err(UserStoreError::NotFound { id }) if id == missing
    ));
}

#[test]
fn test_user_password_round_trips_and_clears() {
    let (dir, store) = store();
    let policy = PasswordPolicy::new(8, 1, 1).unwrap();
    let user = store.create_user("Alice").unwrap();
    assert_eq!(store.get_user_password(&user.id).unwrap(), None);

    store
        .set_user_password(&user.id, &policy.hash("first").unwrap())
        .unwrap();
    store
        .set_user_password(&user.id, &policy.hash("second").unwrap())
        .unwrap();
    drop(store);

    let reopened = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    let stored = reopened.get_user_password(&user.id).unwrap().unwrap();
    assert_eq!(
        stored.check("second", &policy),
        PasswordCheck::Accepted { stale: false }
    );
    assert_eq!(stored.check("first", &policy), PasswordCheck::Rejected);

    reopened.clear_user_password(&user.id).unwrap();
    assert_eq!(reopened.get_user_password(&user.id).unwrap(), None);
}

#[test]
fn test_get_user_password_reports_an_incompatible_verifier_table() {
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(TableDefinition::<&str, u64>::new("server_user_verifier"))
            .unwrap();
    });

    assert!(matches!(
        store.get_user_password(&UserId::random()),
        Err(MetaError::Table(_))
    ));
}

#[test]
fn test_user_operations_roll_back_when_event_storage_is_incompatible() {
    let (_dir, store, user) = store_with_incompatible_event_table();

    assert!(matches!(
        store.create_user("Bob"),
        Err(UserStoreError::Store(MetaError::Table(_)))
    ));
    assert!(matches!(
        store.rename_user(&user.id, "Alice Cooper"),
        Err(UserStoreError::Store(MetaError::Table(_)))
    ));
    assert!(matches!(
        store.set_user_state(&user.id, UserState::Disabled),
        Err(UserStoreError::Store(MetaError::Table(_)))
    ));
    assert!(matches!(store.user_events(&user.id), Err(MetaError::Table(_))));
    assert_eq!(store.get_user(&user.id).unwrap(), Some(user));
}

#[test]
fn test_user_name_lookup_handles_incomplete_user_tables() {
    let (_dir, incompatible_names) = raw_store(|txn| {
        txn.open_table(TableDefinition::<&str, u64>::new("server_user_name"))
            .unwrap();
    });
    assert!(matches!(
        incompatible_names.get_user_by_name("Alice"),
        Err(UserStoreError::Store(MetaError::Table(_)))
    ));

    let (_dir, missing_users) = raw_store(|txn| {
        txn.open_table(RAW_USER_NAME)
            .unwrap()
            .insert("alice", "usr_missing")
            .unwrap();
    });
    assert_eq!(missing_users.get_user_by_name("Alice").unwrap(), None);

    let (_dir, incompatible_users) = raw_store(|txn| {
        txn.open_table(RAW_USER_NAME)
            .unwrap()
            .insert("alice", "usr_missing")
            .unwrap();
        txn.open_table(TableDefinition::<&str, u64>::new("server_user"))
            .unwrap();
    });
    assert!(matches!(
        incompatible_users.get_user_by_name("Alice"),
        Err(UserStoreError::Store(MetaError::Table(_)))
    ));
}

#[test]
fn test_user_tables_migrate_an_older_store_without_touching_driver_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("older.redb");
    older_store(&path, false);

    let store = MetaStore::open(&path).unwrap();

    assert_eq!(
        store.get_driver_value("repository/config").unwrap().as_deref(),
        Some(b"preserved".as_slice())
    );
    assert_eq!(store.create_user("Alice").unwrap().state, UserState::Active);
}

#[test]
fn test_user_reads_treat_missing_old_tables_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("older.redb");
    let database = redb::Database::create(&path).unwrap();
    database.begin_write().unwrap().commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();
    let id = peryx_identity::UserId::random();

    assert_eq!(store.get_user(&id).unwrap(), None);
    assert_eq!(store.get_user_by_name("Alice").unwrap(), None);
    assert_eq!(store.user_events(&id).unwrap(), Vec::new());
    assert_eq!(store.get_user_password(&id).unwrap(), None);
    assert_eq!(store.create_user("Alice").unwrap().state, UserState::Active);
}

#[test]
fn test_failed_user_table_migration_leaves_prior_data_readable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("incompatible.redb");
    older_store(&path, true);

    assert!(matches!(MetaStore::open(&path), Err(MetaError::Table(_))));
    let prior = MetaStore::open_existing(path).unwrap();
    assert_eq!(
        prior.get_driver_value("repository/config").unwrap().as_deref(),
        Some(b"preserved".as_slice())
    );
    assert!(matches!(
        prior.get_user(&peryx_identity::UserId::random()),
        Err(MetaError::Table(_))
    ));
}
