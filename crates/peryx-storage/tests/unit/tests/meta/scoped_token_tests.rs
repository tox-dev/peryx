use std::collections::BTreeSet;

use peryx_identity::{Action, GrantScope, TokenId, TokenName, TokenSecret, UserId};
use redb::TableDefinition;
use rstest::rstest;

use super::store;
use crate::meta::{
    MetaError, MetaStore, NewScopedToken, RevokeScopedTokenOutcome, ScopedTokenQuery, ScopedTokenQueryError,
    ScopedTokenRecord,
};
use crate::tests::pagination::Page;

const RAW_TOKEN: TableDefinition<&str, &[u8]> = TableDefinition::new("scoped_token");
const RAW_REACH: TableDefinition<&str, &str> = TableDefinition::new("scoped_token_reach");
const RAW_VERIFIER: TableDefinition<&str, &str> = TableDefinition::new("scoped_token_verifier");

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

fn actions(actions: impl IntoIterator<Item = Action>) -> BTreeSet<Action> {
    actions.into_iter().collect()
}

fn new_token(name: &str, reach: GrantScope, verifier_of: &TokenSecret, expires_at: Option<i64>) -> NewScopedToken {
    NewScopedToken {
        name: TokenName::new(name).unwrap(),
        reach,
        actions: actions([Action::Read, Action::Write]),
        expires_at,
        verifier: verifier_of.verifier(),
        created_by: UserId::random(),
        created_at_unix: 100,
    }
}

fn repo(name: &str) -> GrantScope {
    GrantScope::Repository { name: name.to_owned() }
}

#[test]
fn test_create_persists_metadata_and_reads_back() {
    let (_dir, store) = store();
    let secret = TokenSecret::generate();
    let created = store
        .create_scoped_token(new_token("ci", repo("hosted"), &secret, Some(500)))
        .unwrap();

    assert!(created.id.as_str().starts_with("tok_"));
    assert_eq!(created.name.as_str(), "ci");
    assert_eq!(created.reach, repo("hosted"));
    assert_eq!(created.actions, actions([Action::Read, Action::Write]));
    assert_eq!(created.created_at_unix, 100);
    assert_eq!(created.expires_at, Some(500));
    assert_eq!(created.revoked_at, None);
    assert_eq!(created.revision, 1);
    assert_eq!(store.get_scoped_token(&created.id).unwrap().as_ref(), Some(&created));
    assert_eq!(store.get_scoped_token(&TokenId::new("tok_absent")).unwrap(), None);
}

#[test]
fn test_verify_resolves_a_live_token_and_rejects_others() {
    let (_dir, store) = store();
    let secret = TokenSecret::generate();
    let created = store
        .create_scoped_token(new_token("ci", repo("hosted"), &secret, None))
        .unwrap();

    assert_eq!(
        store.verify_scoped_token(&secret, 200).unwrap().as_ref(),
        Some(&created)
    );
    assert_eq!(store.verify_scoped_token(&TokenSecret::generate(), 200).unwrap(), None);
}

#[test]
fn test_verify_rejects_an_expired_token() {
    let (_dir, store) = store();
    let secret = TokenSecret::generate();
    store
        .create_scoped_token(new_token("ci", repo("hosted"), &secret, Some(150)))
        .unwrap();

    assert!(store.verify_scoped_token(&secret, 149).unwrap().is_some());
    assert_eq!(store.verify_scoped_token(&secret, 150).unwrap(), None);
}

#[test]
fn test_revoke_blocks_the_next_verification_and_is_idempotent() {
    let (_dir, store) = store();
    let secret = TokenSecret::generate();
    let created = store
        .create_scoped_token(new_token("ci", repo("hosted"), &secret, None))
        .unwrap();

    assert!(matches!(
        store.revoke_scoped_token(&created.id, 300).unwrap().unwrap(),
        RevokeScopedTokenOutcome::Revoked(_)
    ));
    let revoked = store.get_scoped_token(&created.id).unwrap().unwrap();
    assert_eq!(revoked.revoked_at, Some(300));
    assert_eq!(revoked.revision, 2);
    assert_eq!(store.verify_scoped_token(&secret, 301).unwrap(), None);
    assert_eq!(
        store.get_scoped_token(&created.id).unwrap().unwrap().revoked_at,
        Some(300)
    );

    let repeat = store.revoke_scoped_token(&created.id, 400).unwrap().unwrap();
    assert_eq!(repeat.record(), &revoked);
    assert_eq!(repeat, RevokeScopedTokenOutcome::Unchanged(revoked));
    assert_eq!(
        store.revoke_scoped_token(&TokenId::new("tok_absent"), 400).unwrap(),
        None
    );
}

#[test]
fn test_rotate_replaces_the_secret_and_leaves_the_id() {
    let (_dir, store) = store();
    let secret = TokenSecret::generate();
    let created = store
        .create_scoped_token(new_token("ci", repo("hosted"), &secret, None))
        .unwrap();
    let next = TokenSecret::generate();

    let rotated = store
        .rotate_scoped_token(&created.id, &next.verifier())
        .unwrap()
        .unwrap();
    assert_eq!(rotated.id, created.id);
    assert_eq!(rotated.revision, 2);
    assert_eq!(
        store
            .verify_scoped_token(&next, 200)
            .unwrap()
            .as_ref()
            .map(|token| &token.id),
        Some(&created.id)
    );
    assert_eq!(store.verify_scoped_token(&secret, 200).unwrap(), None);
    assert_eq!(
        store
            .rotate_scoped_token(&TokenId::new("tok_absent"), &next.verifier())
            .unwrap(),
        None
    );
}

#[test]
fn test_rotate_leaves_sibling_tokens_valid() {
    let (_dir, store) = store();
    let (kept, rotated) = (TokenSecret::generate(), TokenSecret::generate());
    store
        .create_scoped_token(new_token("kept", repo("hosted"), &kept, None))
        .unwrap();
    let target = store
        .create_scoped_token(new_token("rotated", repo("hosted"), &rotated, None))
        .unwrap();

    let next = TokenSecret::generate();
    store.rotate_scoped_token(&target.id, &next.verifier()).unwrap();
    assert!(store.verify_scoped_token(&kept, 200).unwrap().is_some());
}

#[test]
fn test_rotate_refuses_a_revoked_token_and_leaves_it_intact() {
    let (_dir, store) = store();
    let secret = TokenSecret::generate();
    let created = store
        .create_scoped_token(new_token("ci", repo("hosted"), &secret, None))
        .unwrap();
    store.revoke_scoped_token(&created.id, 300).unwrap();
    let next = TokenSecret::generate();

    assert_eq!(store.rotate_scoped_token(&created.id, &next.verifier()).unwrap(), None);
    assert_eq!(store.verify_scoped_token(&next, 400).unwrap(), None);
    assert_eq!(store.get_scoped_token(&created.id).unwrap().unwrap().revision, 2);
}

#[test]
fn test_list_paginates_within_a_reach() {
    let (_dir, store) = store();
    store
        .create_scoped_token(new_token("other", repo("cached"), &TokenSecret::generate(), None))
        .unwrap();
    let mut expected: Vec<_> = ["t0", "t1", "t2"]
        .into_iter()
        .map(|name| seed_token(&store, name, repo("hosted")))
        .collect();
    expected.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let first = token_page(&store, None, 2).unwrap();
    let second = token_page(&store, first.next_cursor.clone(), 2).unwrap();
    assert_eq!(
        (first, second),
        (
            Page {
                keys: vec![expected[0].1.clone(), expected[1].1.clone()],
                next_cursor: Some(expected[1].0.clone()),
            },
            Page {
                keys: vec![expected[2].1.clone()],
                next_cursor: None,
            },
        )
    );
}

#[test]
fn test_list_keeps_nested_repository_reaches_separate() {
    let (_dir, store) = store();
    store
        .create_scoped_token(new_token("root", repo("root"), &TokenSecret::generate(), None))
        .unwrap();
    store
        .create_scoped_token(new_token("nested", repo("root/alpha"), &TokenSecret::generate(), None))
        .unwrap();

    let listed = store
        .list_scoped_tokens(&ScopedTokenQuery {
            reach: repo("root"),
            cursor: None,
            limit: 25,
        })
        .unwrap();
    assert_eq!(listed.tokens.len(), 1);
    assert_eq!(listed.tokens[0].name.as_str(), "root");
}

#[test]
fn test_list_separates_the_server_reach() {
    let (_dir, store) = store();
    store
        .create_scoped_token(new_token("srv", GrantScope::Server, &TokenSecret::generate(), None))
        .unwrap();
    store
        .create_scoped_token(new_token("repo", repo("hosted"), &TokenSecret::generate(), None))
        .unwrap();

    let server = store
        .list_scoped_tokens(&ScopedTokenQuery {
            reach: GrantScope::Server,
            cursor: None,
            limit: 25,
        })
        .unwrap();
    assert_eq!(server.tokens.len(), 1);
    assert_eq!(server.tokens[0].reach, GrantScope::Server);
}

#[rstest]
#[case(0)]
#[case(101)]
fn test_list_rejects_an_out_of_range_limit(#[case] limit: usize) {
    let (_dir, store) = store();
    let error = store
        .list_scoped_tokens(&ScopedTokenQuery {
            reach: GrantScope::Server,
            cursor: None,
            limit,
        })
        .unwrap_err();
    assert!(matches!(error, ScopedTokenQueryError::InvalidLimit));
    assert_eq!(error.to_string(), "limit must be between 1 and 100");
}

fn seed_token(store: &MetaStore, name: &str, reach: GrantScope) -> (String, String) {
    let id = store
        .create_scoped_token(new_token(name, reach, &TokenSecret::generate(), None))
        .unwrap()
        .id
        .as_str()
        .to_owned();
    (id, name.to_owned())
}

fn token_page(store: &MetaStore, cursor: Option<String>, limit: usize) -> Result<Page, ScopedTokenQueryError> {
    let reach = repo("hosted");
    let page = store.list_scoped_tokens(&ScopedTokenQuery {
        reach: reach.clone(),
        cursor: cursor.map(TokenId::new),
        limit,
    })?;
    assert!(page.tokens.iter().all(|token| token.reach == reach));
    Ok(Page {
        keys: page
            .tokens
            .into_iter()
            .map(|token| token.name.as_str().to_owned())
            .collect(),
        next_cursor: page.next_cursor,
    })
}

#[test]
fn test_is_live_reflects_revocation_and_expiry() {
    let record = ScopedTokenRecord {
        id: TokenId::new("tok_1"),
        name: TokenName::new("t").unwrap(),
        reach: GrantScope::Server,
        actions: actions([Action::Read]),
        created_by: UserId::random(),
        created_at_unix: 0,
        expires_at: Some(100),
        revoked_at: None,
        revision: 1,
    };
    assert!(record.is_live(99));
    assert!(!record.is_live(100));
    let revoked = ScopedTokenRecord {
        revoked_at: Some(50),
        ..record
    };
    assert!(!revoked.is_live(10));
}

#[test]
fn test_reads_treat_missing_token_tables_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("older.redb");
    let database = redb::Database::create(&path).unwrap();
    database.begin_write().unwrap().commit().unwrap();
    drop(database);
    let store = MetaStore::open_existing(path).unwrap();

    assert_eq!(store.get_scoped_token(&TokenId::new("tok_1")).unwrap(), None);
    assert_eq!(store.verify_scoped_token(&TokenSecret::generate(), 0).unwrap(), None);
    let page = store
        .list_scoped_tokens(&ScopedTokenQuery {
            reach: GrantScope::Server,
            cursor: None,
            limit: 25,
        })
        .unwrap();
    assert_eq!(page.tokens, Vec::new());
    assert_eq!(page.next_cursor, None);
}

#[test]
fn test_open_adds_token_tables_without_touching_driver_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("older.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(TableDefinition::<&str, &[u8]>::new("driver_kv"))
        .unwrap()
        .insert("repository/config", b"preserved".as_slice())
        .unwrap();
    txn.commit().unwrap();
    drop(database);

    let store = MetaStore::open(&path).unwrap();

    assert_eq!(
        store.get_driver_value("repository/config").unwrap().as_deref(),
        Some(b"preserved".as_slice())
    );
    let secret = TokenSecret::generate();
    let created = store
        .create_scoped_token(new_token("ci", GrantScope::Server, &secret, None))
        .unwrap();
    assert_eq!(
        store.verify_scoped_token(&secret, 200).unwrap().as_ref(),
        Some(&created)
    );
}

#[test]
fn test_incompatible_token_table_surfaces_a_store_error() {
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(TableDefinition::<&str, u64>::new("scoped_token_reach"))
            .unwrap();
    });
    let error = store
        .list_scoped_tokens(&ScopedTokenQuery {
            reach: GrantScope::Server,
            cursor: None,
            limit: 25,
        })
        .unwrap_err();
    assert!(matches!(error, ScopedTokenQueryError::Store(MetaError::Table(_))));
}

#[test]
fn test_list_treats_a_missing_token_table_as_empty() {
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(RAW_REACH).unwrap();
    });
    let page = store
        .list_scoped_tokens(&ScopedTokenQuery {
            reach: GrantScope::Server,
            cursor: None,
            limit: 25,
        })
        .unwrap();
    assert_eq!(page.tokens, Vec::new());
}

#[test]
fn test_get_surfaces_a_store_error() {
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(TableDefinition::<&str, u64>::new("scoped_token"))
            .unwrap();
    });
    assert!(matches!(
        store.get_scoped_token(&TokenId::new("tok_1")),
        Err(MetaError::Table(_))
    ));
}

#[test]
fn test_verify_surfaces_a_verifier_table_store_error() {
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(TableDefinition::<&str, u64>::new("scoped_token_verifier"))
            .unwrap();
    });
    assert!(matches!(
        store.verify_scoped_token(&TokenSecret::generate(), 0),
        Err(MetaError::Table(_))
    ));
}

#[test]
fn test_verify_surfaces_a_token_table_store_error() {
    let secret = TokenSecret::generate();
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(RAW_VERIFIER)
            .unwrap()
            .insert(secret.verifier().as_str(), "tok_1")
            .unwrap();
        txn.open_table(TableDefinition::<&str, u64>::new("scoped_token"))
            .unwrap();
    });
    assert!(matches!(
        store.verify_scoped_token(&secret, 0),
        Err(MetaError::Table(_))
    ));
}

#[test]
fn test_list_surfaces_a_token_table_store_error() {
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(RAW_REACH).unwrap();
        txn.open_table(TableDefinition::<&str, u64>::new("scoped_token"))
            .unwrap();
    });
    let error = store
        .list_scoped_tokens(&ScopedTokenQuery {
            reach: GrantScope::Server,
            cursor: None,
            limit: 25,
        })
        .unwrap_err();
    assert!(matches!(error, ScopedTokenQueryError::Store(MetaError::Table(_))));
}

#[test]
fn test_verify_returns_none_when_the_token_table_is_absent() {
    let secret = TokenSecret::generate();
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(RAW_VERIFIER)
            .unwrap()
            .insert(secret.verifier().as_str(), "tok_dangling")
            .unwrap();
    });
    assert_eq!(store.verify_scoped_token(&secret, 0).unwrap(), None);
}

#[test]
fn test_verify_and_list_skip_a_dangling_index_entry() {
    let secret = TokenSecret::generate();
    let (_dir, store) = raw_store(|txn| {
        txn.open_table(RAW_TOKEN).unwrap();
        txn.open_table(RAW_VERIFIER)
            .unwrap()
            .insert(secret.verifier().as_str(), "tok_dangling")
            .unwrap();
        txn.open_table(RAW_REACH)
            .unwrap()
            .insert("server\0tok_dangling", "tok_dangling")
            .unwrap();
    });
    assert_eq!(store.verify_scoped_token(&secret, 0).unwrap(), None);
    let page = store
        .list_scoped_tokens(&ScopedTokenQuery {
            reach: GrantScope::Server,
            cursor: None,
            limit: 25,
        })
        .unwrap();
    assert_eq!(page.tokens, Vec::new());
}
