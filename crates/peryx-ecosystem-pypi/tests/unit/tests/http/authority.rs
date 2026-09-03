use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use peryx_driver::state::{ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome};

use super::support::*;
use tokio::sync::Notify;

struct RecordingAuthority {
    homed: HashSet<String>,
    home: &'static str,
    fail_claim: bool,
    epoch: u64,
    admit: bool,
    lease_available: bool,
    lease_expires_at_unix: i64,
    finish_error: Option<&'static str>,
    checked: Mutex<Vec<String>>,
    claimed: Mutex<Vec<String>>,
    admitted: Mutex<Vec<u64>>,
}

impl RecordingAuthority {
    fn unhomed() -> Arc<Self> {
        Self::new(HashSet::new(), "local", false, 0, true, true)
    }

    fn already_homed(authority: &str) -> Arc<Self> {
        Self::new(HashSet::from([authority.to_owned()]), "local", false, 1, true, true)
    }

    fn failing() -> Arc<Self> {
        Self::new(HashSet::new(), "local", true, 0, true, true)
    }

    fn remote() -> Arc<Self> {
        Self::new(HashSet::new(), "west", false, 0, true, true)
    }

    fn homed_at_epoch(authority: &str, epoch: u64, admit: bool) -> Arc<Self> {
        Self::new(
            HashSet::from([authority.to_owned()]),
            "local",
            false,
            epoch,
            admit,
            true,
        )
    }

    fn unavailable(authority: &str, epoch: u64) -> Arc<Self> {
        Self::new(
            HashSet::from([authority.to_owned()]),
            "local",
            false,
            epoch,
            true,
            false,
        )
    }

    fn leased(
        authority: &str,
        epoch: u64,
        lease_expires_at_unix: i64,
        finish_error: Option<&'static str>,
    ) -> Arc<Self> {
        Arc::new(Self {
            homed: HashSet::from([authority.to_owned()]),
            home: "local",
            fail_claim: false,
            epoch,
            admit: true,
            lease_available: true,
            lease_expires_at_unix,
            finish_error,
            checked: Mutex::new(Vec::new()),
            claimed: Mutex::new(Vec::new()),
            admitted: Mutex::new(Vec::new()),
        })
    }

    fn new(
        homed: HashSet<String>,
        home: &'static str,
        fail_claim: bool,
        epoch: u64,
        admit: bool,
        lease_available: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            homed,
            home,
            fail_claim,
            epoch,
            admit,
            lease_available,
            lease_expires_at_unix: i64::MAX,
            finish_error: None,
            checked: Mutex::new(Vec::new()),
            claimed: Mutex::new(Vec::new()),
            admitted: Mutex::new(Vec::new()),
        })
    }

    fn checked(&self) -> Vec<String> {
        self.checked.lock().unwrap().clone()
    }

    fn claimed(&self) -> Vec<String> {
        self.claimed.lock().unwrap().clone()
    }

    fn admitted(&self) -> Vec<u64> {
        self.admitted.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl OwnershipAuthority for RecordingAuthority {
    async fn claim_home(&self, authority: &str) -> Result<HomeClaim, OwnershipError> {
        self.checked.lock().unwrap().push(authority.to_owned());
        if !self.homed.contains(authority) {
            self.claimed.lock().unwrap().push(authority.to_owned());
        }
        if self.fail_claim {
            Err(OwnershipError::Unavailable("ownership group unreachable".to_owned()))
        } else {
            Ok(HomeClaim {
                home: self.home.to_owned(),
                epoch: self.epoch.max(1),
            })
        }
    }

    fn cluster_status(&self) -> ClusterStatus {
        ClusterStatus {
            leader: None,
            term: 0,
            voters: Vec::new(),
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.epoch
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        self.admitted.lock().unwrap().push(presented);
        self.admit
    }

    async fn begin_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<peryx_ha::AuthorityWriteLease>, OwnershipError> {
        let admitted = self.admit_epoch(authority, presented).await;
        if !self.lease_available {
            return Err(OwnershipError::Unavailable("quorum unavailable".to_owned()));
        }
        Ok(admitted.then(|| peryx_ha::AuthorityWriteLease {
            authority: authority.to_owned(),
            epoch: presented,
            id: "test-write".to_owned(),
            expires_at_unix: self.lease_expires_at_unix,
        }))
    }

    async fn finish_epoch_write(&self, _lease: &peryx_ha::AuthorityWriteLease) -> Result<(), OwnershipError> {
        self.finish_error
            .map_or(Ok(()), |error| Err(OwnershipError::Unavailable(error.to_owned())))
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<TransferOutcome>, OwnershipError> {
        Ok(None)
    }
}

#[tokio::test]
async fn test_recording_authority_has_no_consensus_status_or_transfer() {
    let authority = RecordingAuthority::already_homed("peryxpkg");

    assert_eq!(
        authority.cluster_status(),
        ClusterStatus {
            leader: None,
            term: 0,
            voters: Vec::new(),
        }
    );
    assert_eq!(authority.transfer_home("peryxpkg", "west").await.unwrap(), None);
}

async fn publish(state: &std::sync::Arc<peryx_driver::AppState>) -> (StatusCode, String) {
    let (content_type, body) = multipart_body(&[], Some(("peryxpkg-1.0.tar.gz", &fixture_sdist())));
    post_upload_with_headers_response(
        state,
        "/root/pypi/",
        Some(&upload_auth()),
        &content_type,
        &[
            ("host", "peryx.test"),
            ("origin", "http://peryx.test"),
            ("x-peryx-csrf", "http://peryx.test"),
        ],
        body,
    )
    .await
}

#[tokio::test]
async fn test_first_publish_claims_the_projects_home() {
    let h = authority_harness().await;
    let group = RecordingAuthority::unhomed();
    bind_ownership_authority(&h.state, group.clone());
    let (status, body) = publish(&h.state).await;

    assert_eq!((status, body.as_str()), (StatusCode::OK, "upload accepted"));
    assert_eq!(group.checked(), ["peryxpkg"], "the path resolves the committed home");
    assert_eq!(
        group.claimed(),
        ["peryxpkg"],
        "the first stored file claims the normalized project's home",
    );
    assert_eq!(
        h.state
            .serving
            .meta
            .list_upload_entries("hosted", "peryxpkg")
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn test_a_local_winner_publishes_once_across_an_identical_retry() {
    let h = authority_harness().await;
    let group = RecordingAuthority::unhomed();
    bind_ownership_authority(&h.state, group.clone());
    let first = publish(&h.state).await;
    let second = publish(&h.state).await;

    assert_eq!(
        (first, second),
        (
            (StatusCode::OK, "upload accepted".to_owned()),
            (StatusCode::OK, "upload accepted".to_owned())
        )
    );
    assert_eq!(
        (
            group.checked(),
            group.claimed(),
            group.admitted(),
            h.state
                .serving
                .meta
                .list_upload_entries("hosted", "peryxpkg")
                .unwrap()
                .len(),
        ),
        (vec!["peryxpkg".to_owned()], vec!["peryxpkg".to_owned()], vec![1], 1,)
    );
}

#[tokio::test]
async fn test_a_home_claim_that_cannot_commit_keeps_the_publish_pending() {
    let h = authority_harness().await;
    let group = RecordingAuthority::failing();
    bind_ownership_authority(&h.state, group.clone());
    let (status, body) = publish(&h.state).await;

    assert_eq!(
        (status, body.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, "upload storage failed"),
    );
    assert_eq!(group.claimed(), ["peryxpkg"], "the claim was attempted");
    assert!(
        h.state
            .serving
            .meta
            .list_upload_entries("hosted", "peryxpkg")
            .unwrap()
            .is_empty(),
        "the failed claim publishes no file"
    );
}

#[tokio::test]
async fn test_a_remote_home_winner_cannot_expose_staged_bytes() {
    let h = authority_harness().await;
    let group = RecordingAuthority::remote();
    bind_ownership_authority(&h.state, group);
    let bytes = fixture_sdist();
    let digest = peryx_storage::blob::Digest::of(&bytes);

    assert_eq!(publish(&h.state).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert!(h.state.serving.blobs.open(&digest, None).await.is_err());
    assert_eq!(
        get(
            &h.state,
            &local_artifact_url("root/pypi", digest.as_str(), "peryxpkg-1.0.tar.gz"),
            None,
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert!(
        h.state
            .serving
            .meta
            .list_upload_entries("hosted", "peryxpkg")
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn test_a_publish_under_the_current_authority_epoch_stores() {
    let h = authority_harness().await;
    let group = RecordingAuthority::homed_at_epoch("peryxpkg", 7, true);
    bind_ownership_authority(&h.state, group.clone());
    let (status, body) = publish(&h.state).await;

    assert_eq!((status, body.as_str()), (StatusCode::OK, "upload accepted"));
    assert_eq!(
        group.admitted(),
        [7],
        "the store re-admits the epoch it snapshotted before the record write",
    );
    assert_eq!(
        h.state
            .serving
            .meta
            .list_upload_entries("hosted", "peryxpkg")
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn test_an_expired_quorum_lease_publishes_no_file() {
    let h = authority_harness().await;
    bind_ownership_authority(&h.state, RecordingAuthority::leased("peryxpkg", 7, 1005, None));

    let (status, body) = publish(&h.state).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("authority advanced"), "{body:?}");
    assert!(
        h.state
            .serving
            .meta
            .list_upload_entries("hosted", "peryxpkg")
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn test_a_lease_release_failure_does_not_revoke_a_publish() {
    let h = authority_harness().await;
    bind_ownership_authority(
        &h.state,
        RecordingAuthority::leased("peryxpkg", 7, i64::MAX, Some("quorum unavailable")),
    );

    let (status, body) = publish(&h.state).await;

    assert_eq!((status, body.as_str()), (StatusCode::OK, "upload accepted"));
    assert_eq!(
        h.state
            .serving
            .meta
            .list_upload_entries("hosted", "peryxpkg")
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn test_a_stale_home_publish_is_fenced_by_the_authority_epoch() {
    let h = authority_harness().await;
    let group = RecordingAuthority::homed_at_epoch("peryxpkg", 7, false);
    bind_ownership_authority(&h.state, group.clone());
    let (status, body) = publish(&h.state).await;

    assert_eq!(status, StatusCode::CONFLICT, "a superseded epoch fences the publish");
    assert!(
        body.contains("authority advanced"),
        "the response names the fence: {body:?}"
    );
    assert_eq!(group.admitted(), [7], "the snapshot epoch was presented for admission");
    assert!(
        h.state
            .serving
            .meta
            .list_upload_entries("hosted", "peryxpkg")
            .unwrap()
            .is_empty(),
        "the record write never ran, so no file published under the stale home",
    );
}

#[tokio::test]
async fn test_a_publish_without_a_quorum_lease_is_fenced() {
    let h = authority_harness().await;
    bind_ownership_authority(&h.state, RecordingAuthority::unavailable("peryxpkg", 7));

    let (status, body) = publish(&h.state).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("authority advanced"), "{body:?}");
    assert!(
        h.state
            .serving
            .meta
            .list_upload_entries("hosted", "peryxpkg")
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn test_unassigned_distributed_authority_cannot_delete_local_state() {
    let h = authority_harness().await;
    assert_eq!(publish(&h.state).await.0, StatusCode::OK);
    bind_ownership_authority(&h.state, RecordingAuthority::homed_at_epoch("peryxpkg", 0, false));

    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/peryxpkg/", Some(&upload_auth()),).await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.state
            .serving
            .meta
            .list_upload_entries("hosted", "peryxpkg")
            .unwrap()
            .len(),
        1
    );
}

/// The reading the harness clock returns, far enough inside the lease this authority issues that
/// nothing expires while the test runs.
const LEASE_NOW: i64 = 1000;

/// An authority that keeps the write leases it issues, the way the committed ownership state machine
/// keeps them. `OwnershipState::transfer` refuses a home transfer while the authority record still
/// holds a write lease, and the consensus runtime reports that refusal to the caller as
/// `OwnershipError::Unavailable`. A lease a writer never released therefore blocks the next transfer
/// until it expires, which is how a caller observes whether the release happened.
struct LeaseLedger {
    epoch: u64,
    live: Mutex<BTreeMap<String, i64>>,
    issued: AtomicU64,
    /// Signalled once the first lease is recorded, so an observer looks while the writer still holds it.
    leased: Notify,
    /// Awaited by that first lease, so the writer keeps holding it until the observer has looked.
    observed: Notify,
    watched: AtomicBool,
}

impl LeaseLedger {
    fn homed_at(epoch: u64) -> Arc<Self> {
        Arc::new(Self {
            epoch,
            live: Mutex::new(BTreeMap::new()),
            issued: AtomicU64::new(0),
            leased: Notify::new(),
            observed: Notify::new(),
            watched: AtomicBool::new(false),
        })
    }
}

#[async_trait::async_trait]
impl OwnershipAuthority for LeaseLedger {
    async fn claim_home(&self, _authority: &str) -> Result<HomeClaim, OwnershipError> {
        Ok(HomeClaim {
            home: "local".to_owned(),
            epoch: self.epoch,
        })
    }

    fn cluster_status(&self) -> ClusterStatus {
        ClusterStatus {
            leader: None,
            term: 0,
            voters: Vec::new(),
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.epoch
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        presented == self.epoch
    }

    /// Leases whatever epoch the writer presents. Fencing a stale one is the subject of the other
    /// authorities in this file; this one is about what happens to the lease afterwards.
    async fn begin_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<peryx_ha::AuthorityWriteLease>, OwnershipError> {
        let id = format!("write-{}", self.issued.fetch_add(1, Ordering::SeqCst));
        let lease = peryx_ha::AuthorityWriteLease {
            authority: authority.to_owned(),
            epoch: presented,
            id: id.clone(),
            expires_at_unix: LEASE_NOW + peryx_ha::AUTHORITY_WRITE_LEASE_SECS,
        };
        self.live.lock().unwrap().insert(id, lease.expires_at_unix);
        if !self.watched.swap(true, Ordering::SeqCst) {
            self.leased.notify_one();
            self.observed.notified().await;
        }
        Ok(Some(lease))
    }

    async fn finish_epoch_write(&self, lease: &peryx_ha::AuthorityWriteLease) -> Result<(), OwnershipError> {
        self.live.lock().unwrap().remove(&lease.id);
        Ok(())
    }

    async fn transfer_home(&self, _authority: &str, new_home: &str) -> Result<Option<TransferOutcome>, OwnershipError> {
        let mut live = self.live.lock().unwrap();
        live.retain(|_, expires_at_unix| *expires_at_unix > LEASE_NOW);
        if live.is_empty() {
            Ok(Some(TransferOutcome {
                from: "local".to_owned(),
                to: new_home.to_owned(),
                epoch: self.epoch + 1,
            }))
        } else {
            Err(OwnershipError::Unavailable(
                "authority transfer is blocked by a live write lease".to_owned(),
            ))
        }
    }
}

/// A publish holds the project's write lease across its metadata commit and releases it on the way out.
/// The release is observable through the next transfer: a lease the publish kept would refuse it until
/// the lease expired, so the same transfer refused mid-publish has to succeed once the publish returns.
#[tokio::test]
async fn test_a_publish_releases_its_write_lease_to_the_next_transfer() {
    let h = authority_harness().await;
    let group = LeaseLedger::homed_at(1);
    bind_ownership_authority(&h.state, group.clone());
    let during = tokio::spawn({
        let (state, group) = (h.state.clone(), group.clone());
        async move {
            group.leased.notified().await;
            let refused = state.serving.transfer_authority_home("peryxpkg", "west").await;
            group.observed.notify_one();
            refused
        }
    });

    let published = publish(&h.state).await;
    let during = during.await.unwrap().map_err(|error| error.to_string());
    let after = h
        .state
        .serving
        .transfer_authority_home("peryxpkg", "west")
        .await
        .map_err(|error| error.to_string());

    assert_eq!((published.0, published.1.as_str()), (StatusCode::OK, "upload accepted"));
    assert_eq!(
        during,
        Err("ownership claim did not commit: authority transfer is blocked by a live write lease".to_owned()),
        "the publish must hold its lease across the metadata commit",
    );
    assert_eq!(
        after,
        Ok(Some(TransferOutcome {
            from: "local".to_owned(),
            to: "west".to_owned(),
            epoch: 2,
        })),
        "the publish must release its lease, or the next transfer waits out the lease expiry",
    );
}

/// The ledger fences nothing and names no topology, so a refused transfer in the test above can only
/// be the lease the publish still held.
#[tokio::test]
async fn test_the_lease_ledger_reports_its_epoch_and_names_no_topology() {
    let ledger = LeaseLedger::homed_at(1);

    assert_eq!(
        (
            ledger.cluster_status(),
            ledger.committed_epoch("peryxpkg").await,
            ledger.admit_epoch("peryxpkg", 1).await,
        ),
        (
            ClusterStatus {
                leader: None,
                term: 0,
                voters: Vec::new(),
            },
            1,
            true
        )
    );
}
