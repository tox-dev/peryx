use std::collections::BTreeSet;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_core::Ecosystem;
use peryx_identity::{Action, Denial, Glob, Grant, IndexAcl, NamedToken};
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use super::AppState;
use crate::serving::IndexCredentialDriver;

const NOW: i64 = 1_000;
const SECRET: &str = "live-secret";

struct TokenCredentials;

impl IndexCredentialDriver for TokenCredentials {
    fn recognizes(&self, authorization: &str) -> bool {
        peryx_identity::parse_basic(authorization).is_some_and(|credentials| credentials.user == "__token__")
    }
}

fn basic(user: &str, password: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{password}")))
}

fn state(register: bool) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta, blobs, 60, Vec::new());
    Arc::get_mut(&mut state.serving).unwrap().clock = Arc::new(|| NOW);
    if register {
        state.register_capabilities(|registrar| {
            registrar.register_index_credentials(Ecosystem::new("example"), Arc::new(TokenCredentials));
        });
    }
    (dir, state)
}

fn index(anonymous_read: bool, tokens: Vec<NamedToken>) -> Index {
    Index {
        name: "catalog".to_owned(),
        route: "catalog".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl { anonymous_read, tokens },
    }
}

fn token(resources: &[&str], actions: &[Action], expires_at: Option<i64>) -> NamedToken {
    NamedToken {
        name: "reader".to_owned(),
        secret: SECRET.to_owned(),
        grants: vec![Grant {
            resources: resources.iter().map(|resource| Glob::new(*resource)).collect(),
            actions: actions.iter().copied().collect::<BTreeSet<_>>(),
        }],
        expires_at,
    }
}

fn catalog_reader() -> NamedToken {
    token(&["*"], &[Action::Read], None)
}

#[rstest]
#[case::open_index(true)]
#[case::closed_index(false)]
fn test_resolved_token_authorizes_regardless_of_anonymous_read(#[case] anonymous_read: bool) {
    let (_dir, state) = state(true);

    assert_eq!(
        state.authorize_index_credential(
            &index(anonymous_read, vec![catalog_reader()]),
            Some(&basic("__token__", SECRET)),
            Action::Read,
        ),
        Ok(())
    );
}

/// The password is the secret under RFC 7617; an index that serves artifacts anonymously must not
/// promote a credential whose secret resolves to nothing.
#[rstest]
#[case::unknown_secret(true, vec![catalog_reader()], basic("__token__", "garbage"))]
#[case::unknown_secret_closed_index(false, vec![catalog_reader()], basic("__token__", "garbage"))]
#[case::expired_token(true, vec![token(&["*"], &[Action::Read], Some(NOW))], basic("__token__", SECRET))]
#[case::index_without_tokens(true, Vec::new(), basic("__token__", SECRET))]
fn test_unresolved_token_is_unauthenticated(
    #[case] anonymous_read: bool,
    #[case] tokens: Vec<NamedToken>,
    #[case] authorization: String,
) {
    let (_dir, state) = state(true);

    assert_eq!(
        state.authorize_index_credential(&index(anonymous_read, tokens), Some(&authorization), Action::Read),
        Err(Denial::Unauthenticated)
    );
}

#[rstest]
#[case::absent(None)]
#[case::foreign_user(Some(basic("publisher", SECRET)))]
#[case::not_basic(Some(format!("Bearer {SECRET}")))]
fn test_credential_the_ecosystem_does_not_recognize_is_unauthenticated(#[case] authorization: Option<String>) {
    let (_dir, state) = state(true);

    assert_eq!(
        state.authorize_index_credential(
            &index(true, vec![catalog_reader()]),
            authorization.as_deref(),
            Action::Read,
        ),
        Err(Denial::Unauthenticated)
    );
}

#[test]
fn test_ecosystem_without_credential_driver_is_unauthenticated() {
    let (_dir, state) = state(false);

    assert_eq!(
        state.authorize_index_credential(
            &index(true, vec![catalog_reader()]),
            Some(&basic("__token__", SECRET)),
            Action::Read,
        ),
        Err(Denial::Unauthenticated)
    );
}

/// Operational surfaces classify their response by repository, so a token earns them only with a
/// grant over the whole catalog - `anonymous_read` never stands in for one.
#[rstest]
#[case::action_not_granted(&["*"], &[Action::Write])]
#[case::grant_below_the_catalog(&["team/*"], &[Action::Read])]
fn test_resolved_token_without_a_catalog_grant_is_forbidden(#[case] resources: &[&str], #[case] actions: &[Action]) {
    let (_dir, state) = state(true);

    assert_eq!(
        state.authorize_index_credential(
            &index(true, vec![token(resources, actions, None)]),
            Some(&basic("__token__", SECRET)),
            Action::Read,
        ),
        Err(Denial::Forbidden)
    );
}
