use std::collections::{BTreeSet, VecDeque};
use std::sync::Mutex;

use peryx_identity::{Action, GrantScope, TokenId, TokenName, UserId};
use peryx_storage::meta::{MetaStore, RevokeScopedTokenOutcome, ScopedTokenQuery};

use crate::tokens::{CreateScopedToken, TokenService, TokenServiceError};

#[test]
fn test_create_returns_a_verifiable_secret_and_records_metadata() {
    let (_dir, service) = service();
    let (record, secret) = service.create(request(), 100).unwrap();

    assert!(secret.expose().starts_with("peryx_"));
    assert_eq!(record.revision, 1);
    assert_eq!(service.inspect(&record.id).unwrap().as_ref(), Some(&record));
    assert_eq!(service.verify(&secret, 200).unwrap().as_ref(), Some(&record));
}

#[test]
fn test_create_regenerates_a_colliding_secret() {
    let collision = peryx_identity::TokenSecret::presented("collision");
    let replacement = peryx_identity::TokenSecret::presented("replacement");
    let (_dir, service) = service_with_secrets([collision.clone(), collision.clone(), replacement.clone()]);
    let (first, _) = service.create(request(), 100).unwrap();

    let (second, secret) = service.create(request(), 101).unwrap();

    assert_eq!(secret, replacement);
    assert_eq!(service.verify(&collision, 200).unwrap(), Some(first));
    assert_eq!(service.verify(&secret, 200).unwrap(), Some(second));
}

#[test]
fn test_create_fails_after_two_colliding_secrets() {
    let collision = peryx_identity::TokenSecret::presented("collision");
    let (_dir, service) = service_with_secrets([collision.clone(), collision.clone(), collision.clone()]);
    let (first, _) = service.create(request(), 100).unwrap();

    let error = service.create(request(), 101).unwrap_err();

    assert!(matches!(error, TokenServiceError::SecretGenerationExhausted));
    assert_eq!(service.verify(&collision, 200).unwrap(), Some(first.clone()));
    assert_eq!(
        service
            .list(&ScopedTokenQuery {
                reach: GrantScope::Server,
                cursor: None,
                limit: 25,
            })
            .unwrap()
            .tokens,
        vec![first]
    );
}

#[test]
fn test_create_propagates_a_store_error_without_regenerating_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    drop(MetaStore::open(&path).unwrap());
    let secrets = Mutex::new(VecDeque::from([peryx_identity::TokenSecret::presented("only")]));
    let service = TokenService::with_secret_source(MetaStore::open_existing_read_only(path).unwrap(), move || {
        secrets
            .lock()
            .unwrap()
            .pop_front()
            .expect("store errors are not retried")
    });

    let error = service.create(request(), 100).unwrap_err();

    assert!(matches!(error, TokenServiceError::Store(_)));
}

#[test]
fn test_list_returns_created_tokens() {
    let (_dir, service) = service();
    let (record, _) = service.create(request(), 100).unwrap();

    let page = service
        .list(&ScopedTokenQuery {
            reach: GrantScope::Server,
            cursor: None,
            limit: 25,
        })
        .unwrap();
    assert_eq!(page.tokens, vec![record]);
}

#[test]
fn test_rotate_issues_a_new_secret_and_invalidates_the_old() {
    let (_dir, service) = service();
    let (record, first) = service.create(request(), 100).unwrap();
    let actor = record.created_by.clone();

    let (rotated, second) = service.rotate(&record.id, &actor).unwrap().unwrap();
    assert_eq!(rotated.revision, 2);
    assert_eq!(
        service.verify(&second, 200).unwrap().as_ref().map(|token| &token.id),
        Some(&record.id)
    );
    assert_eq!(service.verify(&first, 200).unwrap(), None);
    assert_eq!(service.rotate(&TokenId::new("tok_absent"), &actor).unwrap(), None);
}

#[test]
fn test_rotate_regenerates_a_colliding_secret() {
    let owner_secret = peryx_identity::TokenSecret::presented("owner");
    let target_secret = peryx_identity::TokenSecret::presented("target");
    let replacement = peryx_identity::TokenSecret::presented("replacement");
    let (_dir, service) = service_with_secrets([
        owner_secret.clone(),
        target_secret,
        owner_secret.clone(),
        replacement.clone(),
    ]);
    let (owner, _) = service.create(request(), 100).unwrap();
    let (target, _) = service.create(request(), 101).unwrap();

    let (rotated, secret) = service.rotate(&target.id, &target.created_by).unwrap().unwrap();

    assert_eq!(secret, replacement);
    assert_eq!(rotated.revision, 2);
    assert_eq!(service.verify(&owner_secret, 200).unwrap(), Some(owner));
    assert_eq!(service.verify(&secret, 200).unwrap(), Some(rotated));
}

#[test]
fn test_rotate_fails_after_two_colliding_secrets() {
    let owner_secret = peryx_identity::TokenSecret::presented("owner");
    let target_secret = peryx_identity::TokenSecret::presented("target");
    let (_dir, service) = service_with_secrets([
        owner_secret.clone(),
        target_secret.clone(),
        owner_secret.clone(),
        owner_secret.clone(),
    ]);
    let (owner, _) = service.create(request(), 100).unwrap();
    let (target, _) = service.create(request(), 101).unwrap();

    let error = service.rotate(&target.id, &target.created_by).unwrap_err();

    assert!(matches!(error, TokenServiceError::SecretGenerationExhausted));
    assert_eq!(service.verify(&owner_secret, 200).unwrap(), Some(owner));
    assert_eq!(service.verify(&target_secret, 200).unwrap(), Some(target));
}

#[test]
fn test_revoke_is_idempotent_and_reports_absence() {
    let (_dir, service) = service();
    let (record, secret) = service.create(request(), 100).unwrap();
    let actor = record.created_by.clone();

    let revoked = service.revoke(&record.id, &actor, 300).unwrap().unwrap();
    assert!(matches!(revoked, RevokeScopedTokenOutcome::Revoked(_)));
    assert_eq!(service.verify(&secret, 400).unwrap(), None);

    let repeat = service.revoke(&record.id, &actor, 400).unwrap().unwrap();
    assert!(matches!(repeat, RevokeScopedTokenOutcome::Unchanged(_)));
    assert_eq!(service.revoke(&TokenId::new("tok_absent"), &actor, 400).unwrap(), None);
}

#[test]
fn test_debug_names_the_service_and_store() {
    let (_dir, service) = service();

    assert!(format!("{service:?}").starts_with("TokenService { store: MetaStore"));
}

fn service() -> (tempfile::TempDir, TokenService) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, TokenService::new(meta))
}

fn service_with_secrets(
    secrets: impl IntoIterator<Item = peryx_identity::TokenSecret>,
) -> (tempfile::TempDir, TokenService) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let secrets = Mutex::new(secrets.into_iter().collect::<VecDeque<_>>());
    (
        dir,
        TokenService::with_secret_source(meta, move || {
            secrets
                .lock()
                .unwrap()
                .pop_front()
                .expect("test supplied enough secrets")
        }),
    )
}

fn request() -> CreateScopedToken {
    CreateScopedToken {
        name: TokenName::new("ci").unwrap(),
        reach: GrantScope::Server,
        actions: BTreeSet::from([Action::Read, Action::Write]),
        expires_at: None,
        created_by: UserId::random(),
    }
}
