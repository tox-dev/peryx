use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::{
    TestOwnership, app_with_distributed, auth, bind_ownership, body_has_code, hosted_writable_distributed,
    hosted_writable_distributed_with_clock, oci_index, send, send_body, send_with, writer_acl,
};
use axum::http::{HeaderMap, Method, StatusCode};
use bytes::Bytes;
use peryx_driver::state::{ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome};
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use tokio::sync::Notify;

const TOKEN: &str = "s3cret";
// An index with no children is the cheapest manifest a push accepts: these tests fence the write
// path, not the image document, so the fixture names no blob to upload first.
const MANIFEST_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const MANIFEST: &[u8] = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;
const OTHER_MANIFEST: &[u8] = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[],"annotations":{"v":"2"}}"#;

struct RecordingAuthority {
    fail_claim: bool,
    home: &'static str,
    homed: Mutex<HashSet<String>>,
    checked: Mutex<Vec<String>>,
    claimed: Mutex<Vec<String>>,
}

impl RecordingAuthority {
    fn unhomed() -> Arc<Self> {
        Self::new("local", false)
    }

    fn failing() -> Arc<Self> {
        Self::new("local", true)
    }

    fn remote() -> Arc<Self> {
        Self::new("west", false)
    }

    fn new(home: &'static str, fail_claim: bool) -> Arc<Self> {
        Arc::new(Self {
            fail_claim,
            home,
            homed: Mutex::new(HashSet::new()),
            checked: Mutex::new(Vec::new()),
            claimed: Mutex::new(Vec::new()),
        })
    }

    fn checked(&self) -> Vec<String> {
        self.checked.lock().unwrap().clone()
    }

    fn claimed(&self) -> Vec<String> {
        self.claimed.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl OwnershipAuthority for RecordingAuthority {
    async fn claim_home(&self, authority: &str) -> Result<HomeClaim, OwnershipError> {
        self.checked.lock().unwrap().push(authority.to_owned());
        let assigned = !self.homed.lock().unwrap().contains(authority);
        if assigned {
            self.claimed.lock().unwrap().push(authority.to_owned());
        }
        if self.fail_claim {
            Err(OwnershipError::Unavailable("ownership group unreachable".to_owned()))
        } else {
            if assigned {
                self.homed.lock().unwrap().insert(authority.to_owned());
            }
            Ok(HomeClaim {
                home: self.home.to_owned(),
                epoch: 1,
            })
        }
    }

    fn cluster_status(&self) -> ClusterStatus {
        ClusterStatus {
            leader: Some("node-a".to_owned()),
            term: 7,
            voters: vec!["node-a".to_owned(), "node-b".to_owned()],
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        0
    }

    async fn admit_epoch(&self, _authority: &str, _presented: u64) -> bool {
        true
    }

    async fn begin_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<peryx_ha::AuthorityWriteLease>, OwnershipError> {
        Ok(Some(peryx_ha::AuthorityWriteLease {
            authority: authority.to_owned(),
            epoch: presented,
            id: "test-write".to_owned(),
            expires_at_unix: i64::MAX,
        }))
    }

    async fn finish_epoch_write(&self, _lease: &peryx_ha::AuthorityWriteLease) -> Result<(), OwnershipError> {
        Ok(())
    }

    async fn transfer_home(&self, authority: &str, new_home: &str) -> Result<Option<TransferOutcome>, OwnershipError> {
        Ok(Some(TransferOutcome {
            from: authority.to_owned(),
            to: new_home.to_owned(),
            epoch: 8,
        }))
    }
}

#[tokio::test]
async fn test_ownership_uses_local_defaults_until_bound() {
    let authority = RecordingAuthority::unhomed();
    let ownership = TestOwnership::default();

    assert_eq!(
        (
            ownership.cluster_status(),
            ownership.committed_epoch("store/app").await,
            ownership.admit_epoch("store/app", 8).await,
            ownership.transfer_home("store/app", "node-b").await.unwrap(),
        ),
        (
            ClusterStatus {
                leader: None,
                term: 0,
                voters: Vec::new(),
            },
            0,
            true,
            None,
        )
    );
    ownership.bind(super::EpochAuthority::settled(7));
    assert_eq!(
        (
            ownership.committed_epoch("store/app").await,
            ownership.admit_epoch("store/app", 8).await,
        ),
        (7, false),
    );
    ownership.bind(authority);
    assert_eq!(
        (
            ownership.cluster_status(),
            ownership.committed_epoch("store/app").await,
            ownership.admit_epoch("store/app", 8).await,
            ownership.transfer_home("store/app", "node-b").await.unwrap(),
        ),
        (
            ClusterStatus {
                leader: Some("node-a".to_owned()),
                term: 7,
                voters: vec!["node-a".to_owned(), "node-b".to_owned()],
            },
            0,
            true,
            Some(TransferOutcome {
                from: "store/app".to_owned(),
                to: "node-b".to_owned(),
                epoch: 8,
            }),
        )
    );
}

#[tokio::test]
async fn shared_distributed_test_services_expose_their_contracts() {
    use peryx_ha::{AnalyticsCompleteness as _, BlobWriteDurability as _};

    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("services.redb")).unwrap();
    assert_eq!(
        super::UnavailableCompleteness.assess(
            &meta,
            &[],
            &peryx_ha::CompletenessQuery {
                from_day: 1,
                to_day: 2,
                today: 2,
                repository: None,
            },
        ),
        Err(peryx_ha::CompletenessError)
    );
    let digest = peryx_storage::blob::Digest::of(b"fixture");
    let durability = super::LocalDurability
        .confirm(peryx_ha::CommittedBlob::new(
            &digest,
            b"fixture".len() as u64,
            "store/app",
            peryx_ha::AuthorityEpoch(1),
            None,
            peryx_core::WriteEvidence::NodeLocal,
            peryx_ha::OperationKind::Publish,
        ))
        .await;
    assert_eq!(
        durability,
        peryx_ha::WriteDurability::Confirmed {
            scope: peryx_core::BlobDurability::Filesystem,
        }
    );
}

#[tokio::test]
async fn upload_epoch_authority_exposes_the_fence_state() {
    let authority = super::EpochAuthority::settled(7);

    assert_eq!(
        authority.claim_home("store/app").await.unwrap(),
        HomeClaim {
            home: "local".to_owned(),
            epoch: 7,
        }
    );
    assert_eq!(authority.cluster_status().term, 7);
    assert_eq!(authority.transfer_home("store/app", "node-b").await.unwrap(), None);
}

async fn push(app: &axum::Router, reference: &str) -> StatusCode {
    push_full(app, reference, MANIFEST).await.0
}

async fn push_full(app: &axum::Router, reference: &str, manifest: &[u8]) -> (StatusCode, HeaderMap, Bytes) {
    send_body(
        app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{reference}"),
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await
}

#[tokio::test]
async fn test_first_manifest_push_claims_the_repositorys_home() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let group = RecordingAuthority::unhomed();
    bind_ownership(&state, group.clone());
    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);

    // The ecosystem prefix isolates authority namespaces.
    assert_eq!(group.checked(), ["oci:app"], "the path resolves the committed home");
    assert_eq!(
        group.claimed(),
        ["oci:app"],
        "the first push claims the repository's home"
    );
}

#[tokio::test]
async fn test_repeat_manifest_push_makes_no_second_claim() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let group = RecordingAuthority::unhomed();
    bind_ownership(&state, group.clone());
    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);
    assert_eq!(push(&app, "v2").await, StatusCode::CREATED);

    assert_eq!(group.checked(), ["oci:app", "oci:app"], "each push reads the home");
    assert_eq!(
        group.claimed(),
        ["oci:app"],
        "only the first push claims; a homed repository costs no second consensus round",
    );
}

#[tokio::test]
async fn test_a_home_claim_that_cannot_commit_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let group = RecordingAuthority::failing();
    bind_ownership(&state, group.clone());
    assert_eq!(push(&app, "v1").await, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(group.claimed(), ["oci:app"], "the claim was attempted");
    assert_eq!(pull_status(&app, "v1").await, StatusCode::NOT_FOUND);
    assert_eq!(state.serving.meta.current_serial().unwrap(), 0);
    assert_eq!(
        state
            .serving
            .meta
            .quota_usage("store")
            .unwrap()
            .accounted_bytes
            .committed,
        0
    );
}

#[tokio::test]
async fn test_a_remote_home_winner_commits_no_manifest_effects() {
    let dir = tempfile::tempdir().unwrap();
    let index = Index {
        acl: writer_acl(TOKEN),
        policy: Policy::compile(
            &PolicyConfig {
                max_accounted_bytes: Some(u64::MAX),
                ..PolicyConfig::default()
            },
            str::to_owned,
        ),
        ..oci_index("store", "store", IndexKind::Hosted { volatile: true })
    };
    let (state, app) = super::app_with_setup(&dir, vec![index], true, |state| {
        super::install_test_distributed(state, None, std::sync::Arc::new(super::LocalDurability));
    });
    bind_ownership(&state, RecordingAuthority::remote());
    let canonical = format!("sha256:{}", peryx_storage::blob::Digest::of(MANIFEST).as_str());

    let (status, _, body) = push_full(&app, "v1", MANIFEST).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_no_topology(&body);
    assert_eq!(
        (
            crate::store::manifest_is_member(&state.serving.meta, "store", "app", &canonical).unwrap(),
            crate::store::get_tag(&state.serving.meta, "store", "app", "v1").unwrap(),
            state.serving.meta.current_serial().unwrap(),
            state.serving.meta.quota_usage("store").unwrap().accounted_bytes,
        ),
        (false, None, 0, peryx_storage::meta::QuotaValue::default())
    );
}

/// The fake fences mutations when its leased and current epochs differ.
struct EpochAuthority {
    committed: AtomicU64,
    current: AtomicU64,
    expires_at_unix: i64,
    finish_available: bool,
    lease_clock_calls: Option<Arc<AtomicU64>>,
}

impl EpochAuthority {
    fn settled(epoch: u64) -> Arc<Self> {
        Arc::new(Self {
            committed: AtomicU64::new(epoch),
            current: AtomicU64::new(epoch),
            expires_at_unix: i64::MAX,
            finish_available: true,
            lease_clock_calls: None,
        })
    }

    fn superseded(leased: u64, current: u64) -> Arc<Self> {
        Arc::new(Self {
            committed: AtomicU64::new(leased),
            current: AtomicU64::new(current),
            expires_at_unix: i64::MAX,
            finish_available: true,
            lease_clock_calls: None,
        })
    }

    fn leased(epoch: u64, expires_at_unix: i64, finish_available: bool) -> Arc<Self> {
        Arc::new(Self {
            committed: AtomicU64::new(epoch),
            current: AtomicU64::new(epoch),
            expires_at_unix,
            finish_available,
            lease_clock_calls: None,
        })
    }

    fn expiring_between_checks(epoch: u64, lease_clock_calls: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self {
            committed: AtomicU64::new(epoch),
            current: AtomicU64::new(epoch),
            expires_at_unix: 1006,
            finish_available: true,
            lease_clock_calls: Some(lease_clock_calls),
        })
    }

    fn transfer(&self) {
        self.current.fetch_add(1, Ordering::SeqCst);
    }

    fn settle(&self) {
        self.committed
            .store(self.current.load(Ordering::SeqCst), Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl OwnershipAuthority for EpochAuthority {
    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.committed.load(Ordering::SeqCst)
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        let current = self.current.load(Ordering::SeqCst);
        current != 0 && presented == current
    }

    async fn begin_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<peryx_ha::AuthorityWriteLease>, OwnershipError> {
        if let Some(calls) = &self.lease_clock_calls {
            calls.store(0, Ordering::SeqCst);
        }
        Ok(self
            .admit_epoch(authority, presented)
            .await
            .then(|| peryx_ha::AuthorityWriteLease {
                authority: authority.to_owned(),
                epoch: presented,
                id: "test-write".to_owned(),
                expires_at_unix: self.expires_at_unix,
            }))
    }

    async fn finish_epoch_write(&self, _lease: &peryx_ha::AuthorityWriteLease) -> Result<(), OwnershipError> {
        if self.finish_available {
            Ok(())
        } else {
            Err(OwnershipError::Unavailable("quorum unavailable".to_owned()))
        }
    }

    async fn claim_home(&self, _authority: &str) -> Result<HomeClaim, OwnershipError> {
        Ok(HomeClaim {
            home: "local".to_owned(),
            epoch: self.committed.load(Ordering::SeqCst),
        })
    }

    fn cluster_status(&self) -> ClusterStatus {
        ClusterStatus {
            leader: None,
            term: self.current.load(Ordering::SeqCst),
            voters: Vec::new(),
        }
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<TransferOutcome>, OwnershipError> {
        Ok(None)
    }
}

/// The reading every clock in the lease-release test returns, far enough inside the lease this
/// authority issues that nothing expires while the test runs.
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

/// A manifest push holds the repository's write lease across its metadata commit and releases it on the
/// way out. The release is observable through the next transfer: a lease the push kept would refuse it
/// until the lease expired, so the same transfer refused mid-push has to succeed once the push returns.
#[tokio::test]
async fn test_a_manifest_push_releases_its_write_lease_to_the_next_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed_with_clock(&dir, TOKEN, Arc::new(|| LEASE_NOW));
    let group = LeaseLedger::homed_at(5);
    bind_ownership(&state, group.clone());
    let during = tokio::spawn({
        let (state, group) = (state.clone(), group.clone());
        async move {
            group.leased.notified().await;
            let refused = state.serving.transfer_authority_home("oci:store/app", "west").await;
            group.observed.notify_one();
            refused
        }
    });

    let pushed = push(&app, "v1").await;
    let during = during.await.unwrap().map_err(|error| error.to_string());
    let after = state
        .serving
        .transfer_authority_home("oci:store/app", "west")
        .await
        .map_err(|error| error.to_string());

    assert_eq!(pushed, StatusCode::CREATED);
    assert_eq!(
        during,
        Err("ownership claim did not commit: authority transfer is blocked by a live write lease".to_owned()),
        "the push must hold its lease across the metadata commit",
    );
    assert_eq!(
        after,
        Ok(Some(TransferOutcome {
            from: "local".to_owned(),
            to: "west".to_owned(),
            epoch: 6,
        })),
        "the push must release its lease, or the next transfer waits out the lease expiry",
    );
}

/// The ledger fences nothing and names no topology, so a refused transfer in the test above can only
/// be the lease the push still held.
#[tokio::test]
async fn test_the_lease_ledger_reports_its_epoch_and_names_no_topology() {
    let ledger = LeaseLedger::homed_at(5);

    assert_eq!(
        (
            ledger.cluster_status(),
            ledger.committed_epoch("oci:store/app").await,
            ledger.admit_epoch("oci:store/app", 5).await,
        ),
        (
            ClusterStatus {
                leader: None,
                term: 0,
                voters: Vec::new(),
            },
            5,
            true
        )
    );
}

fn metered_app(dir: &tempfile::TempDir, max_bytes: u64) -> (Arc<peryx_driver::AppState>, axum::Router) {
    let index = Index {
        acl: writer_acl(TOKEN),
        policy: Policy::compile(
            &PolicyConfig {
                max_accounted_bytes: Some(max_bytes),
                ..PolicyConfig::default()
            },
            str::to_owned,
        ),
        ..oci_index("store", "store", IndexKind::Hosted { volatile: true })
    };
    app_with_distributed(dir, index)
}

async fn delete(app: &axum::Router, reference: &str) -> StatusCode {
    send_with(
        app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{reference}"),
        &[("authorization", &auth(TOKEN))],
    )
    .await
    .0
}

async fn pull_status(app: &axum::Router, reference: &str) -> StatusCode {
    send(app, Method::GET, &format!("/v2/store/app/manifests/{reference}"))
        .await
        .0
}

/// Stale-epoch errors must not expose control-plane topology.
fn assert_no_topology(body: &Bytes) {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    for leaked in ["leader", "voter", "datacenter", "://", ".internal"] {
        assert!(!text.contains(leaked), "stale-epoch response leaked {leaked:?}: {text}");
    }
}

#[tokio::test]
async fn test_manifest_push_at_the_current_epoch_publishes() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    bind_ownership(&state, EpochAuthority::settled(5));
    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);
    assert_eq!(pull_status(&app, "v1").await, StatusCode::OK);
}

#[tokio::test]
async fn test_manifest_push_with_an_expired_lease_never_mutates_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed_with_clock(&dir, TOKEN, Arc::new(|| 1000));
    bind_ownership(&state, EpochAuthority::leased(5, 1005, true));

    assert_eq!(push(&app, "v1").await, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(pull_status(&app, "v1").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_manifest_push_fences_a_lease_that_expires_before_commit() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let clock_calls = calls.clone();
    let clock = Arc::new(move || {
        if clock_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            1000
        } else {
            1001
        }
    });
    let (state, app) = hosted_writable_distributed_with_clock(&dir, TOKEN, clock);
    bind_ownership(&state, EpochAuthority::expiring_between_checks(5, calls));

    assert_eq!(push(&app, "v1").await, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(pull_status(&app, "v1").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_manifest_push_release_failure_does_not_revoke_the_commit() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    bind_ownership(&state, EpochAuthority::leased(5, i64::MAX, false));

    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);
    assert_eq!(pull_status(&app, "v1").await, StatusCode::OK);
}

#[tokio::test]
async fn test_manifest_push_under_a_superseded_epoch_is_unavailable_then_retries() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let group = EpochAuthority::superseded(5, 6);
    bind_ownership(&state, group.clone());
    let (status, _, body) = push_full(&app, "v1", MANIFEST).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_has_code(&body, "UNAVAILABLE"), "{body:?}");
    assert_no_topology(&body);

    assert_eq!(pull_status(&app, "v1").await, StatusCode::NOT_FOUND);

    group.settle();
    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);
    assert_eq!(pull_status(&app, "v1").await, StatusCode::OK);
}

#[tokio::test]
async fn test_manifest_push_without_a_quorum_lease_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    bind_ownership(&state, super::EpochAuthority::unavailable(5));

    let (status, _, body) = push_full(&app, "v1", MANIFEST).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_has_code(&body, "UNAVAILABLE"), "{body:?}");
    assert_eq!(pull_status(&app, "v1").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_unassigned_distributed_authority_cannot_delete_local_state() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);
    bind_ownership(&state, EpochAuthority::settled(0));

    assert_eq!(delete(&app, "v1").await, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(pull_status(&app, "v1").await, StatusCode::OK);
}

#[tokio::test]
async fn test_tag_replacement_under_a_superseded_epoch_keeps_the_old_target() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let group = EpochAuthority::settled(5);
    bind_ownership(&state, group.clone());
    let (status, headers, _) = push_full(&app, "release", MANIFEST).await;
    assert_eq!(status, StatusCode::CREATED);
    let first = headers["docker-content-digest"].to_str().unwrap().to_owned();

    group.transfer();
    let (status, _, body) = push_full(&app, "release", OTHER_MANIFEST).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_has_code(&body, "UNAVAILABLE"), "{body:?}");

    let (status, headers, _) = send(&app, Method::GET, "/v2/store/app/manifests/release").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], first);
}

#[tokio::test]
async fn test_manifest_delete_is_fenced_by_the_repository_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let group = EpochAuthority::settled(9);
    bind_ownership(&state, group.clone());
    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);

    group.transfer();
    let (status, _, body) = send_with(
        &app,
        Method::DELETE,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN))],
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_has_code(&body, "UNAVAILABLE"), "{body:?}");
    assert_eq!(pull_status(&app, "v1").await, StatusCode::OK);

    group.settle();
    assert_eq!(delete(&app, "v1").await, StatusCode::ACCEPTED);
    assert_eq!(pull_status(&app, "v1").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_manifest_restore_is_fenced_by_the_repository_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let group = EpochAuthority::settled(3);
    bind_ownership(&state, group.clone());
    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);
    assert_eq!(delete(&app, "v1").await, StatusCode::ACCEPTED);

    group.transfer();
    let (status, _, body) = send_with(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1/restore",
        &[("authorization", &auth(TOKEN))],
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_has_code(&body, "UNAVAILABLE"), "{body:?}");
    assert_eq!(pull_status(&app, "v1").await, StatusCode::NOT_FOUND);

    group.settle();
    let (status, ..) = send_with(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1/restore",
        &[("authorization", &auth(TOKEN))],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(pull_status(&app, "v1").await, StatusCode::OK);
}

#[tokio::test]
async fn test_a_fenced_push_releases_its_quota_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = metered_app(&dir, 4 << 20);
    bind_ownership(&state, EpochAuthority::superseded(5, 6));
    let (status, ..) = push_full(&app, "v1", MANIFEST).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    assert_eq!(
        state
            .serving
            .meta
            .quota_usage("store")
            .unwrap()
            .accounted_bytes
            .reserved,
        0
    );
}

#[tokio::test]
async fn test_epoch_authority_double_reports_no_group_topology() {
    // Fence tests need epochs but no consensus runtime.
    let group = EpochAuthority::settled(4);
    let status = group.cluster_status();
    assert_eq!((status.leader, status.term, status.voters), (None, 4, Vec::new()));
    assert_eq!(group.transfer_home("app", "west").await.unwrap(), None);
}

const LAYER: &[u8] = b"a-layer-under-authority";

async fn push_blob(app: &axum::Router) -> StatusCode {
    send_body(
        app,
        Method::POST,
        &format!("/v2/store/app/blobs/uploads/?digest={}", super::oci_digest(LAYER)),
        &[("authorization", &auth(TOKEN))],
        LAYER.to_vec(),
    )
    .await
    .0
}

async fn delete_blob(app: &axum::Router) -> (StatusCode, Bytes) {
    let (status, _, body) = send_with(
        app,
        Method::DELETE,
        &format!("/v2/store/app/blobs/{}", super::oci_digest(LAYER)),
        &[("authorization", &auth(TOKEN))],
    )
    .await;
    (status, body)
}

async fn pull_blob_status(app: &axum::Router) -> StatusCode {
    send(
        app,
        Method::GET,
        &format!("/v2/store/app/blobs/{}", super::oci_digest(LAYER)),
    )
    .await
    .0
}

#[tokio::test]
async fn test_blob_delete_is_fenced_by_the_repository_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let group = EpochAuthority::settled(9);
    bind_ownership(&state, group.clone());
    assert_eq!(push_blob(&app).await, StatusCode::CREATED);

    group.transfer();
    let (status, body) = delete_blob(&app).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_has_code(&body, "UNAVAILABLE"), "{body:?}");
    assert_no_topology(&body);
    assert_eq!(pull_blob_status(&app).await, StatusCode::OK);
}

#[tokio::test]
async fn test_blob_delete_at_the_settled_epoch_unlinks_the_digest() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable_distributed(&dir, TOKEN);
    let group = EpochAuthority::settled(9);
    bind_ownership(&state, group.clone());
    assert_eq!(push_blob(&app).await, StatusCode::CREATED);
    group.transfer();
    assert_eq!(delete_blob(&app).await.0, StatusCode::SERVICE_UNAVAILABLE);

    group.settle();

    assert_eq!(delete_blob(&app).await.0, StatusCode::ACCEPTED);
    assert_eq!(pull_blob_status(&app).await, StatusCode::NOT_FOUND);
}
