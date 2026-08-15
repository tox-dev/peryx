use std::collections::BTreeSet;
use std::error::Error as _;
use std::sync::Arc;

use peryx_identity::{
    Action, Glob, Grant, IndexAcl, NamedToken, PasswordCheck, PasswordError, PasswordPolicy, Principal, UserId,
    UserLifecycleChange, UserState,
};
use peryx_storage::meta::MetaStore;
use tokio::sync::Barrier;

use crate::users::{BootstrapError, EnrollError, UserService};

fn writer_acl(secret: impl Into<String>) -> IndexAcl {
    IndexAcl {
        anonymous_read: true,
        tokens: vec![NamedToken {
            name: "uploader".to_owned(),
            secret: secret.into(),
            grants: vec![Grant {
                resources: vec![Glob::new("*")],
                actions: BTreeSet::from([Action::Write, Action::Delete]),
            }],
            expires_at: None,
        }],
    }
}

fn service() -> (tempfile::TempDir, UserService) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, UserService::new(store))
}

fn cheap_policy() -> PasswordPolicy {
    PasswordPolicy::new(8, 1, 1).unwrap()
}

fn cheap_service() -> (tempfile::TempDir, MetaStore, UserService) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let service = UserService::with_password_settings(store.clone(), cheap_policy(), 2);
    (dir, store, service)
}

#[test]
fn test_user_service_runs_the_account_lifecycle() {
    let (_dir, service) = service();
    let user = service.create("Alice").unwrap();

    assert_eq!(service.rename(&user.id, "Alice").unwrap(), user);
    service.rename(&user.id, "ALICE").unwrap();
    let renamed = service.rename(&user.id, "Alice Cooper").unwrap();
    let disabled = service.disable(&user.id).unwrap();
    assert_eq!(service.disable(&user.id).unwrap(), disabled);

    assert_eq!(renamed.id, user.id);
    assert_eq!(disabled.state, UserState::Disabled);
    assert_eq!(service.inspect(&user.id).unwrap(), Some(disabled));
    assert_eq!(service.identify("ALICE COOPER").unwrap(), None);

    let active = service.reactivate(&user.id).unwrap();
    assert_eq!(service.identify("alice cooper").unwrap(), Some(active));
    assert_eq!(
        service.events(&user.id).unwrap()[4].change,
        UserLifecycleChange::Reactivated
    );
}

#[test]
fn test_user_disable_does_not_change_token_identity() {
    let (_dir, service) = service();
    let user = service.create("Alice").unwrap();
    let acl = writer_acl("s3cret");
    let before = acl.identify(Some("Basic YWxpY2U6czNjcmV0"), 0).principal;

    service.disable(&user.id).unwrap();

    assert_eq!(
        before,
        Principal::Named {
            subject: "uploader".to_owned(),
        }
    );
    assert_eq!(acl.identify(Some("Basic YWxpY2U6czNjcmV0"), 0).principal, before);
}

#[tokio::test]
async fn test_authenticate_accepts_the_password_and_rejects_a_wrong_one() {
    let (_dir, _store, service) = cheap_service();
    let user = service.create("Alice").unwrap();
    service.set_password(&user.id, "correct horse").await.unwrap();

    assert_eq!(
        service.authenticate("Alice", "correct horse").await.unwrap(),
        Some(user.id)
    );
    assert_eq!(service.authenticate("alice", "battery staple").await.unwrap(), None);
}

#[tokio::test]
async fn test_user_service_bootstrap_uses_the_password_worker() {
    let (_dir, store, service) = cheap_service();

    let user = service
        .bootstrap_administrator("Alice", "correct horse battery staple")
        .await
        .unwrap();

    assert_eq!(
        service
            .authenticate("Alice", "correct horse battery staple")
            .await
            .unwrap(),
        Some(user.id.clone())
    );
    assert_eq!(
        store.user_role_grants(&user.id).unwrap()[0].role,
        peryx_identity::Role::Administrator
    );
}

#[tokio::test]
async fn test_user_service_bootstrap_forwards_store_refusal() {
    let (_dir, _store, service) = cheap_service();
    service
        .bootstrap_administrator("Alice", "correct horse battery staple")
        .await
        .unwrap();

    let error = service
        .bootstrap_administrator("Bob", "another administrator password")
        .await
        .unwrap_err();

    assert!(error.source().is_none());
    assert!(matches!(error, BootstrapError::Store(_)));
}

#[test]
fn test_bootstrap_error_preserves_a_hash_failure() {
    let error = BootstrapError::from(PasswordError::Params);
    assert_eq!(error.to_string(), "argon2 cost parameters are out of range");
    assert!(error.source().is_none());
    assert!(matches!(error, BootstrapError::Hash(PasswordError::Params)));
}

#[tokio::test]
async fn test_authenticate_fails_the_same_way_for_unknown_disabled_and_passwordless() {
    let (_dir, _store, service) = cheap_service();
    service.create("Passwordless").unwrap();
    let disabled = service.create("Disabled").unwrap();
    service.set_password(&disabled.id, "correct horse").await.unwrap();
    service.disable(&disabled.id).unwrap();

    assert_eq!(service.authenticate("Unknown", "correct horse").await.unwrap(), None);
    assert_eq!(
        service.authenticate("Passwordless", "correct horse").await.unwrap(),
        None
    );
    assert_eq!(service.authenticate("Disabled", "correct horse").await.unwrap(), None);
}

#[tokio::test]
async fn test_authenticate_rejects_an_empty_display_name() {
    let (_dir, _store, service) = cheap_service();

    assert_eq!(service.authenticate("   ", "correct horse").await.unwrap(), None);
}

#[tokio::test]
async fn test_authenticate_upgrades_a_stale_verifier_under_the_same_id() {
    let (_dir, store, weak) = cheap_service();
    let user = weak.create("Alice").unwrap();
    weak.set_password(&user.id, "correct horse").await.unwrap();
    let tighter = PasswordPolicy::new(16, 2, 1).unwrap();
    let strong = UserService::with_password_settings(store.clone(), tighter, 2);
    let stored = store.get_user_password(&user.id).unwrap().unwrap();
    assert_eq!(
        stored.check("correct horse", &tighter),
        PasswordCheck::Accepted { stale: true }
    );

    assert_eq!(
        strong.authenticate("Alice", "correct horse").await.unwrap(),
        Some(user.id.clone())
    );

    let upgraded = store.get_user_password(&user.id).unwrap().unwrap();
    assert_eq!(
        upgraded.check("correct horse", &tighter),
        PasswordCheck::Accepted { stale: false }
    );
}

#[tokio::test]
async fn test_authenticate_denies_the_login_when_the_identity_lookup_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("server_user_name"))
        .unwrap();
    txn.commit().unwrap();
    drop(database);
    let service = UserService::with_password_settings(MetaStore::open_existing(path).unwrap(), cheap_policy(), 2);

    assert!(service.authenticate("Alice", "correct horse").await.is_err());
}

#[tokio::test]
async fn test_clear_password_stops_password_authentication() {
    let (_dir, _store, service) = cheap_service();
    let user = service.create("Alice").unwrap();
    service.set_password(&user.id, "correct horse").await.unwrap();

    service.clear_password(&user.id).unwrap();

    assert_eq!(service.authenticate("Alice", "correct horse").await.unwrap(), None);
}

#[tokio::test]
async fn test_set_password_reports_an_unknown_user() {
    let (_dir, _store, service) = cheap_service();
    let missing = UserId::random();

    let error = service.set_password(&missing, "correct horse").await.unwrap_err();

    assert!(matches!(error, EnrollError::Store(_)));
    assert!(matches!(EnrollError::from(PasswordError::Params), EnrollError::Hash(_)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_password_checks_do_not_exhaust_request_workers() {
    let (_dir, _store, service) = cheap_service();
    let user = service.create("Alice").unwrap();
    service.set_password(&user.id, "correct horse").await.unwrap();

    let logins = (0..16)
        .map(|_| {
            let service = service.clone();
            let id = user.id.clone();
            tokio::spawn(async move {
                assert_eq!(service.authenticate("Alice", "correct horse").await.unwrap(), Some(id));
            })
        })
        .collect::<Vec<_>>();
    let ready = Arc::new(Barrier::new(17));
    let requests = (0..16)
        .map(|seq| {
            let ready = ready.clone();
            tokio::spawn(async move {
                ready.wait().await;
                seq
            })
        })
        .collect::<Vec<_>>();
    ready.wait().await;
    let (logins, served) = tokio::join!(
        futures_util::future::join_all(logins),
        futures_util::future::join_all(requests),
    );

    assert!(logins.into_iter().all(|login| login.is_ok()));
    assert_eq!(
        served.into_iter().map(Result::unwrap).sum::<usize>(),
        (0..16).sum::<usize>()
    );
}
