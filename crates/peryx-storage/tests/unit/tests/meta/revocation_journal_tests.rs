use std::str::FromStr as _;

use peryx_identity::{ArtifactDigest, RevocationReason, UserId};
use rstest::rstest;

use crate::meta::{DigestRevocationState, MetaError, MetaStore, PutRevocationError, ServerMutation};

use super::store;

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_str(&format!("sha256:{suffix:064x}")).unwrap()
}

fn reason(value: &str) -> RevocationReason {
    RevocationReason::new(value).unwrap()
}

fn journalled(store: &MetaStore) -> Vec<ServerMutation> {
    store
        .journal_after(0, 16)
        .unwrap()
        .into_iter()
        .filter_map(|record| ServerMutation::decode(&record.payload).unwrap())
        .collect()
}

#[test]
fn test_revoking_a_digest_journals_the_whole_row() {
    let (_dir, store) = store();
    let actor = UserId::random();

    let created = store
        .put_digest_revocation(&digest(1), &reason("incident one"), &actor, 10)
        .unwrap();

    assert_eq!(
        journalled(&store),
        vec![ServerMutation::DigestRevocation {
            record: created.record().clone()
        }]
    );
}

#[test]
fn test_lifting_a_digest_journals_the_lifted_row() {
    let (_dir, store) = store();
    let actor = UserId::random();
    store
        .put_digest_revocation(&digest(1), &reason("incident one"), &actor, 10)
        .unwrap();

    let lifted = store.lift_digest_revocation(&digest(1), &actor, 20).unwrap().unwrap();

    assert_eq!(
        journalled(&store).pop(),
        Some(ServerMutation::DigestRevocation {
            record: lifted.record().clone()
        })
    );
}

#[test]
fn test_reopening_a_lifted_digest_journals_the_reopened_row() {
    let (_dir, store) = store();
    let actor = UserId::random();
    store
        .put_digest_revocation(&digest(1), &reason("incident one"), &actor, 10)
        .unwrap();
    store.lift_digest_revocation(&digest(1), &actor, 20).unwrap();

    let reopened = store
        .put_digest_revocation(&digest(1), &reason("incident two"), &actor, 30)
        .unwrap();

    assert_eq!(
        journalled(&store).pop(),
        Some(ServerMutation::DigestRevocation {
            record: reopened.record().clone()
        })
    );
}

/// A follower advances to a serial it can act on, so a call that leaves the rows alone must not mint
/// one: an entry carrying no change would move every replica's cursor for nothing.
#[test]
fn test_repeating_a_revocation_adds_no_serial() {
    let (_dir, store) = store();
    let actor = UserId::random();
    store
        .put_digest_revocation(&digest(1), &reason("incident one"), &actor, 10)
        .unwrap();

    store
        .put_digest_revocation(&digest(1), &reason("incident one"), &actor, 11)
        .unwrap();

    assert_eq!(store.current_serial().unwrap(), 1);
}

#[test]
fn test_a_conflicting_reason_adds_no_serial() {
    let (_dir, store) = store();
    let actor = UserId::random();
    store
        .put_digest_revocation(&digest(1), &reason("incident one"), &actor, 10)
        .unwrap();

    let outcome = store.put_digest_revocation(&digest(1), &reason("incident two"), &actor, 11);

    assert!(matches!(outcome, Err(PutRevocationError::ReasonConflict)));
    assert_eq!(store.current_serial().unwrap(), 1);
}

#[test]
fn test_lifting_a_digest_that_is_not_revoked_adds_no_serial() {
    let (_dir, store) = store();

    let outcome = store.lift_digest_revocation(&digest(1), &UserId::random(), 20).unwrap();

    assert_eq!((outcome, store.current_serial().unwrap()), (None, 0));
}

#[test]
fn test_applying_a_journalled_revocation_reproduces_the_row() {
    let (_dir, primary) = store();
    let (_replica_dir, replica) = store();
    let actor = UserId::random();
    let created = primary
        .put_digest_revocation(&digest(1), &reason("incident one"), &actor, 10)
        .unwrap();
    let mutation = journalled(&primary).remove(0);

    replica
        .commit_driver_txn::<_, MetaError>(|txn| {
            txn.apply_server_mutation(&mutation)?;
            Ok(((), Vec::new()))
        })
        .unwrap();

    assert_eq!(
        replica.digest_revocation(&digest(1)).unwrap().as_ref(),
        Some(created.record())
    );
}

#[test]
fn test_a_server_mutation_round_trips_through_its_encoding() {
    let (_dir, store) = store();
    let created = store
        .put_digest_revocation(&digest(1), &reason("incident one"), &UserId::random(), 10)
        .unwrap();
    let mutation = ServerMutation::DigestRevocation {
        record: created.record().clone(),
    };

    let decoded = ServerMutation::decode(&mutation.encode()).unwrap();

    assert_eq!(decoded, Some(mutation));
}

#[rstest]
#[case::not_json(b"not json".to_vec())]
#[case::json_without_the_core_tag(br#"{"action":"add-file","project":"alpha"}"#.to_vec())]
#[case::json_scalar(b"42".to_vec())]
fn test_an_ecosystem_payload_is_not_a_core_mutation(#[case] payload: Vec<u8>) {
    assert_eq!(ServerMutation::decode(&payload).unwrap(), None);
}

#[test]
fn test_a_core_payload_that_does_not_describe_its_operation_fails_to_decode() {
    let payload = br#"{"server-op":"digest-revocation"}"#;

    let decoded = ServerMutation::decode(payload);

    assert!(matches!(decoded, Err(MetaError::Decode(_))));
}

#[test]
fn test_a_replayed_revocation_leaves_the_active_count_alone() {
    let (_dir, primary) = store();
    let (_replica_dir, replica) = store();
    let actor = UserId::random();
    primary
        .put_digest_revocation(&digest(1), &reason("incident one"), &actor, 10)
        .unwrap();
    let mutation = journalled(&primary).remove(0);
    let apply = || {
        replica
            .commit_driver_txn::<_, MetaError>(|txn| {
                txn.apply_server_mutation(&mutation)?;
                Ok(((), Vec::new()))
            })
            .unwrap();
    };

    apply();
    apply();
    replica.lift_digest_revocation(&digest(1), &actor, 20).unwrap().unwrap();

    assert_eq!(
        (
            replica.has_active_digest_revocation().unwrap(),
            replica
                .digest_revocation(&digest(1))
                .unwrap()
                .map(|record| record.state),
        ),
        (
            false,
            Some(DigestRevocationState::Lifted {
                lifted_by: actor,
                lifted_at_unix: 20
            })
        )
    );
}
