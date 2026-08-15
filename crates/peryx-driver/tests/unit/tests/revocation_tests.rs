use std::str::FromStr as _;
use std::sync::Arc;

use peryx_identity::{ArtifactDigest, DigestDecision, RevocationReason, UserId};
use peryx_storage::meta::{DigestRevocationQuery, DigestRevocationState, MetaStore, PutRevocationOutcome};

use crate::revocations::RevocationService;

fn digest() -> ArtifactDigest {
    ArtifactDigest::from_str("sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap()
}

fn other_digest() -> ArtifactDigest {
    ArtifactDigest::from_str("sha256:1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").unwrap()
}

fn service() -> (tempfile::TempDir, MetaStore, RevocationService) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store.clone(), RevocationService::new(store))
}

#[test]
fn test_revocation_service_invalidates_clear_and_revoked_decisions() {
    let (_dir, _store, service) = service();
    let digest = digest();
    let actor = UserId::random();
    let reason = RevocationReason::new("incident").unwrap();

    assert!(!service.has_active().unwrap());
    assert_eq!(service.decision(&digest).unwrap(), DigestDecision::Clear);
    assert!(matches!(
        service.put(&digest, &reason, &actor, 10).unwrap(),
        PutRevocationOutcome::Created(_)
    ));
    assert!(service.has_active().unwrap());
    assert_eq!(service.decision(&digest).unwrap(), DigestDecision::Revoked);
    assert_eq!(service.decision(&digest).unwrap(), DigestDecision::Revoked);
    service.lift(&digest, &actor, 11).unwrap();
    assert!(!service.has_active().unwrap());
    assert_eq!(service.decision(&digest).unwrap(), DigestDecision::Clear);
}

#[test]
fn test_revocation_service_exposes_management_records() {
    let (_dir, _store, service) = service();
    let digest = digest();
    let actor = UserId::random();
    service
        .put(&digest, &RevocationReason::new("incident").unwrap(), &actor, 10)
        .unwrap();

    assert_eq!(
        service.inspect(&digest).unwrap().unwrap().state,
        DigestRevocationState::Active
    );
    assert_eq!(
        service.list(&DigestRevocationQuery::default()).unwrap().revocations[0].digest,
        digest
    );
}

#[test]
fn test_revocation_service_reads_a_lifted_digest_while_another_is_active() {
    let (_dir, _store, service) = service();
    let digest = digest();
    let other_digest = other_digest();
    let actor = UserId::random();
    let reason = RevocationReason::new("incident").unwrap();
    service.put(&digest, &reason, &actor, 10).unwrap();
    service.put(&other_digest, &reason, &actor, 11).unwrap();
    service.lift(&digest, &actor, 12).unwrap();

    assert!(service.has_active().unwrap());
    assert_eq!(service.decision(&digest).unwrap(), DigestDecision::Clear);
    assert_eq!(service.decision(&other_digest).unwrap(), DigestDecision::Revoked);
}

#[test]
fn test_revocation_service_serializes_misses_against_mutation() {
    let (_dir, _store, service) = service();
    let service = Arc::new(service);
    let digest = digest();
    let mut readers = Vec::new();
    for _ in 0..8 {
        let service = Arc::clone(&service);
        let digest = digest.clone();
        readers.push(std::thread::spawn(move || {
            for _ in 0..100 {
                let _ = service.decision(&digest).unwrap();
            }
        }));
    }
    service
        .put(
            &digest,
            &RevocationReason::new("incident").unwrap(),
            &UserId::random(),
            10,
        )
        .unwrap();
    for reader in readers {
        reader.join().unwrap();
    }

    assert_eq!(service.decision(&digest).unwrap(), DigestDecision::Revoked);
}

#[test]
fn test_revocation_service_fails_closed_on_a_store_type_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("digest_revocation"))
        .unwrap();
    txn.commit().unwrap();
    drop(database);
    let service = RevocationService::new(MetaStore::open_existing(path).unwrap());

    assert!(service.has_active().is_err());
    assert!(service.decision(&digest()).is_err());
}
