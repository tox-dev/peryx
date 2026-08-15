use std::error::Error as _;
use std::sync::{Arc, Barrier};

use peryx_identity::{GrantScope, PasswordCheck, PasswordPolicy, Role, UserLifecycleChange};
use redb::{ReadableDatabase as _, ReadableTableMetadata as _, TableDefinition};

use super::store;
use crate::meta::{AdministratorBootstrapError, MetaError, MetaStore};

const RAW_GRANT: TableDefinition<&str, &[u8]> = TableDefinition::new("role_grant");
const RAW_USER: TableDefinition<&str, &[u8]> = TableDefinition::new("server_user");
const RAW_USER_EVENT: TableDefinition<&str, &[u8]> = TableDefinition::new("server_user_event");
const RAW_USER_NAME: TableDefinition<&str, &str> = TableDefinition::new("server_user_name");

#[test]
fn test_bootstrap_commits_an_authenticating_administrator_and_event() {
    let (_dir, store) = store();
    let password = verifier("correct horse battery staple");

    let user = store.bootstrap_administrator(" Alice ", &password).unwrap();

    assert_eq!(store.get_user_by_name("alice").unwrap(), Some(user.clone()));
    assert_eq!(
        store
            .get_user_password(&user.id)
            .unwrap()
            .unwrap()
            .check("correct horse battery staple", &PasswordPolicy::new(8, 1, 1).unwrap()),
        PasswordCheck::Accepted { stale: false }
    );
    assert_eq!(
        store.user_role_grants(&user.id).unwrap(),
        vec![peryx_identity::RoleGrant::new(
            user.id.clone(),
            Role::Administrator,
            GrantScope::Server
        )]
    );
    assert_eq!(
        store.user_events(&user.id).unwrap()[0].change,
        UserLifecycleChange::AdministratorBootstrapped {
            display_name: "Alice".to_owned()
        }
    );
}

#[test]
fn test_concurrent_bootstrap_commits_one_administrator() {
    let (_dir, store) = store();
    let barrier = Arc::new(Barrier::new(3));
    let results = std::thread::scope(|scope| {
        let first_store = store.clone();
        let first_barrier = Arc::clone(&barrier);
        let first = scope.spawn(move || {
            first_barrier.wait();
            first_store.bootstrap_administrator("Alice", &verifier("first administrator password"))
        });
        let second_store = store.clone();
        let second_barrier = Arc::clone(&barrier);
        let second = scope.spawn(move || {
            second_barrier.wait();
            second_store.bootstrap_administrator("Bob", &verifier("second administrator password"))
        });
        barrier.wait();
        [first.join().unwrap(), second.join().unwrap()]
    });

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(AdministratorBootstrapError::AdministratorExists)))
            .count(),
        1
    );
    let winner = results.into_iter().find_map(Result::ok).unwrap();
    assert_eq!(store.user_role_grants(&winner.id).unwrap().len(), 1);
}

#[test]
fn test_bootstrap_refusal_preserves_existing_state() {
    let (_dir, store) = store();
    let administrator = store.create_user("Existing").unwrap();
    store
        .grant_role(
            &administrator.id,
            Role::Administrator,
            GrantScope::Repository {
                name: "team/api".to_owned(),
            },
        )
        .unwrap();

    let error = store
        .bootstrap_administrator("Alice", &verifier("another administrator password"))
        .unwrap_err();

    assert!(error.source().is_none());
    assert!(matches!(error, AdministratorBootstrapError::AdministratorExists));
    assert_eq!(store.get_user_by_name("Alice").unwrap(), None);
    assert_eq!(store.get_user(&administrator.id).unwrap(), Some(administrator.clone()));
    assert_eq!(store.user_events(&administrator.id).unwrap().len(), 1);
    assert_eq!(store.user_role_grants(&administrator.id).unwrap().len(), 1);
}

#[test]
fn test_bootstrap_rejects_an_invalid_name() {
    let (_dir, store) = store();
    let password = verifier("administrator password");

    let error = store.bootstrap_administrator(" ", &password).unwrap_err();

    assert_eq!(error.to_string(), "user display name cannot be empty");
    assert!(error.source().is_none());
    assert!(matches!(error, AdministratorBootstrapError::Name(_)));
}

#[test]
fn test_bootstrap_rejects_a_duplicate_name() {
    let (_dir, store) = store();
    store.create_user("Alice").unwrap();
    let password = verifier("administrator password");

    let error = store.bootstrap_administrator("ALICE", &password).unwrap_err();

    assert_eq!(error.to_string(), "user identity \"alice\" already exists");
    assert!(error.source().is_none());
    assert!(matches!(
        error,
        AdministratorBootstrapError::DuplicateName { canonical_name } if canonical_name == "alice"
    ));
}

#[test]
fn test_bootstrap_rolls_back_earlier_writes_when_a_later_table_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(RAW_GRANT).unwrap();
    txn.open_table(RAW_USER).unwrap();
    txn.open_table(RAW_USER_EVENT).unwrap();
    txn.open_table(RAW_USER_NAME).unwrap();
    txn.open_table(TableDefinition::<&str, u64>::new("server_user_verifier"))
        .unwrap();
    txn.commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(&path).unwrap();

    let error = store
        .bootstrap_administrator("Alice", &verifier("administrator password"))
        .unwrap_err();
    assert!(error.source().is_none());
    assert!(matches!(error, AdministratorBootstrapError::Store(MetaError::Table(_))));
    drop(store);

    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_read().unwrap();
    assert_eq!(txn.open_table(RAW_USER).unwrap().len().unwrap(), 0);
    assert_eq!(txn.open_table(RAW_USER_NAME).unwrap().len().unwrap(), 0);
    assert_eq!(txn.open_table(RAW_USER_EVENT).unwrap().len().unwrap(), 0);
    assert_eq!(txn.open_table(RAW_GRANT).unwrap().len().unwrap(), 0);
}

fn verifier(password: &str) -> peryx_identity::PasswordVerifier {
    PasswordPolicy::new(8, 1, 1).unwrap().hash(password).unwrap()
}
