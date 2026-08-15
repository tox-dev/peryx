use std::collections::BTreeSet;

use peryx_identity::{Action, GrantScope, TokenId, TokenName, UserId};
use peryx_storage::meta::{MetaStore, RevokeScopedTokenOutcome, ScopedTokenQuery};

use crate::tokens::{CreateScopedToken, TokenService};

fn service() -> (tempfile::TempDir, TokenService) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, TokenService::new(meta))
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
