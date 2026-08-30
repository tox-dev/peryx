use std::collections::BTreeSet;
use std::error::Error as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier as ThreadBarrier};

use argon2::password_hash::{PasswordHasher as _, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use peryx_identity::{
    Action, Glob, Grant, IndexAcl, NamedToken, PasswordCheck, PasswordError, PasswordPolicy, PasswordVerifier,
    Principal, UserId, UserLifecycleChange, UserState,
};
use peryx_storage::meta::{MetaStore, StoredPasswordVerifier};
use tracing_subscriber::layer::SubscriberExt as _;

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
    assert!(matches!(
        error,
        BootstrapError::Derivation(crate::users::PasswordDerivationError::Hash(PasswordError::Params))
    ));
}

#[test]
fn test_enrollment_and_bootstrap_report_password_overload() {
    let enrollment = EnrollError::from(crate::users::PasswordDerivationError::Overloaded);
    let bootstrap = BootstrapError::from(crate::users::PasswordDerivationError::Overloaded);

    assert_eq!(
        (enrollment.to_string(), bootstrap.to_string()),
        (
            "password derivation capacity exhausted".to_owned(),
            "password derivation capacity exhausted".to_owned(),
        )
    );
    assert!(matches!(
        enrollment,
        EnrollError::Derivation(crate::users::PasswordDerivationError::Overloaded)
    ));
    assert!(matches!(
        bootstrap,
        BootstrapError::Derivation(crate::users::PasswordDerivationError::Overloaded)
    ));
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
        stored.verifier().check("correct horse", &tighter),
        PasswordCheck::Accepted { stale: true }
    );

    assert_eq!(
        strong.authenticate("Alice", "correct horse").await.unwrap(),
        Some(user.id.clone())
    );

    let upgraded = store.get_user_password(&user.id).unwrap().unwrap();
    assert_eq!(
        upgraded.verifier().check("correct horse", &tighter),
        PasswordCheck::Accepted { stale: false }
    );
}

#[rstest::rstest]
#[case::reset(ConcurrentPasswordChange::Reset)]
#[case::clear(ConcurrentPasswordChange::Clear)]
#[case::enrollment(ConcurrentPasswordChange::Enrollment)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_authenticate_rejects_a_stale_verifier_after_a_password_change(#[case] change: ConcurrentPasswordChange) {
    let (_dir, store, weak) = cheap_service();
    let user = weak.create("Alice").unwrap();
    weak.set_password(&user.id, "old password").await.unwrap();
    let policy = PasswordPolicy::new(16, 2, 1).unwrap();
    let service = UserService::with_password_settings(store.clone(), policy, 2);
    let verifier_read = Arc::new(ThreadBarrier::new(2));
    let release_login = Arc::new(ThreadBarrier::new(2));
    let verifier_reads = Arc::new(AtomicUsize::new(0));
    let login = {
        let service = service.clone();
        let verifier_read = Arc::clone(&verifier_read);
        let release_login = Arc::clone(&release_login);
        let verifier_reads = Arc::clone(&verifier_reads);
        std::thread::spawn(move || {
            let subscriber = tracing_subscriber::registry().with(VerifierReadBarrier {
                verifier_read,
                release_login,
                verifier_reads,
            });
            tracing::subscriber::with_default(subscriber, || {
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(service.authenticate("Alice", "old password"))
            })
        })
    };
    verifier_read.wait();

    match change {
        ConcurrentPasswordChange::Reset => service.set_password(&user.id, "new password").await.unwrap(),
        ConcurrentPasswordChange::Clear => service.clear_password(&user.id).unwrap(),
        ConcurrentPasswordChange::Enrollment => {
            service.clear_password(&user.id).unwrap();
            service.set_password(&user.id, "new password").await.unwrap();
        }
    }
    let password_after_change = store
        .get_user_password(&user.id)
        .unwrap()
        .map(|stored| stored.verifier().clone());
    release_login.wait();

    assert_eq!(login.join().unwrap().unwrap(), None);
    assert_eq!(verifier_reads.load(Ordering::SeqCst), 1);
    let stored = store.get_user_password(&user.id).unwrap();
    assert_eq!(
        stored.as_ref().map(StoredPasswordVerifier::verifier),
        password_after_change.as_ref()
    );
    match change {
        ConcurrentPasswordChange::Clear => assert!(stored.is_none()),
        ConcurrentPasswordChange::Reset | ConcurrentPasswordChange::Enrollment => {
            let stored = stored.unwrap();
            assert_eq!(
                (
                    stored.verifier().check("new password", &policy),
                    stored.verifier().check("old password", &policy),
                ),
                (PasswordCheck::Accepted { stale: false }, PasswordCheck::Rejected)
            );
        }
    }
}

#[derive(Clone, Copy)]
enum ConcurrentPasswordChange {
    Reset,
    Clear,
    Enrollment,
}

struct VerifierReadBarrier {
    verifier_read: Arc<ThreadBarrier>,
    release_login: Arc<ThreadBarrier>,
    verifier_reads: Arc<AtomicUsize>,
}

impl<Subscriber> tracing_subscriber::Layer<Subscriber> for VerifierReadBarrier
where
    Subscriber: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _context: tracing_subscriber::layer::Context<'_, Subscriber>) {
        if event.metadata().target() == "peryx_driver::users::password_verifier_read"
            && self.verifier_reads.fetch_add(1, Ordering::SeqCst) == 0
        {
            self.verifier_read.wait();
            self.release_login.wait();
        }
    }
}

#[tokio::test]
async fn test_authenticate_upgrades_a_legacy_profile() {
    let (_dir, store, service) = cheap_service();
    let user = service.create("Alice").unwrap();
    let salt = SaltString::encode_b64(&[0; 16]).unwrap();
    let params = Params::new(8, 1, 1, Some(16)).unwrap();
    let encoded = Argon2::new(Algorithm::Argon2i, Version::V0x10, params)
        .hash_password(b"correct horse", &salt)
        .unwrap()
        .to_string();
    let verifier: PasswordVerifier = serde_json::from_value(serde_json::Value::String(encoded)).unwrap();
    store.set_user_password(&user.id, &verifier).unwrap();

    assert_eq!(
        service.authenticate("Alice", "correct horse").await.unwrap(),
        Some(user.id.clone())
    );
    assert_eq!(
        store
            .get_user_password(&user.id)
            .unwrap()
            .unwrap()
            .verifier()
            .check("correct horse", &cheap_policy()),
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
    assert!(matches!(
        EnrollError::from(PasswordError::Params),
        EnrollError::Derivation(crate::users::PasswordDerivationError::Hash(_))
    ));
}
