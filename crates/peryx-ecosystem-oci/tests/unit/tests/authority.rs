//! The manifest push path routes a first publish through the repository's home authority: the first
//! push claims the repository's home, a later push finds the home already set and claims nothing, and a
//! claim that cannot commit never blocks the push.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, Method, StatusCode};
use bytes::Bytes;
use peryx_driver::state::{ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome};
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyConfig};
use rstest::rstest;

use super::{app_with, auth, body_has_code, hosted_writable, oci_index, send, send_body, send_with, writer_acl};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const MANIFEST: &[u8] = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
/// A second manifest whose distinct bytes hash to a different digest, so a tag can be repointed from
/// one to the other.
const OTHER_MANIFEST: &[u8] =
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","annotations":{"v":"2"}}"#;

/// A stand-in ownership group that records which authorities the push path checks and claims. A won
/// claim homes the authority, so a repeat push over the same group finds the home already set. When
/// `fail_claim` is set, a claim is attempted but cannot commit, standing in for an unreachable leader.
struct RecordingAuthority {
    fail_claim: bool,
    homed: Mutex<HashSet<String>>,
    checked: Mutex<Vec<String>>,
    claimed: Mutex<Vec<String>>,
}

impl RecordingAuthority {
    fn unhomed() -> Arc<Self> {
        Self::new(false)
    }

    fn failing() -> Arc<Self> {
        Self::new(true)
    }

    fn new(fail_claim: bool) -> Arc<Self> {
        Arc::new(Self {
            fail_claim,
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
    async fn has_home(&self, authority: &str) -> bool {
        self.checked.lock().unwrap().push(authority.to_owned());
        self.homed.lock().unwrap().contains(authority)
    }

    async fn claim_home(&self, authority: &str) -> Result<HomeClaim, OwnershipError> {
        self.claimed.lock().unwrap().push(authority.to_owned());
        if self.fail_claim {
            Err(OwnershipError::Unavailable("ownership group unreachable".to_owned()))
        } else {
            self.homed.lock().unwrap().insert(authority.to_owned());
            Ok(HomeClaim::AssignedHere)
        }
    }

    fn cluster_status(&self) -> ClusterStatus {
        // A recording double runs no consensus group, so it reports none.
        ClusterStatus {
            leader: None,
            term: 0,
            voters: Vec::new(),
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        0
    }

    async fn admit_epoch(&self, _authority: &str, _presented: u64) -> bool {
        true
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<TransferOutcome>, OwnershipError> {
        Ok(None)
    }
}

/// Push the fixture manifest to `store/app` under `reference` and return the response status.
async fn push(app: &axum::Router, reference: &str) -> StatusCode {
    push_full(app, reference, MANIFEST).await.0
}

/// Push `manifest` to `store/app` under `reference` and return the full response, so a fence test can
/// read the error body and the assigned digest.
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
    let (state, app) = hosted_writable(&dir, TOKEN);
    let group = RecordingAuthority::unhomed();
    state.set_ownership_authority(group.clone());

    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);

    // The repository routes through its canonical, scheme-prefixed authority key, so a repository named
    // like a PyPI project cannot collide with it.
    assert_eq!(group.checked(), ["oci:app"], "the path reads the home before claiming");
    assert_eq!(
        group.claimed(),
        ["oci:app"],
        "the first push claims the repository's home"
    );
}

#[tokio::test]
async fn test_repeat_manifest_push_makes_no_second_claim() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let group = RecordingAuthority::unhomed();
    state.set_ownership_authority(group.clone());

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
async fn test_a_home_claim_that_cannot_commit_does_not_block_the_push() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let group = RecordingAuthority::failing();
    state.set_ownership_authority(group.clone());

    assert_eq!(
        push(&app, "v1").await,
        StatusCode::CREATED,
        "a claim that cannot commit is logged, never surfaced, and never blocks the push",
    );
    assert_eq!(group.claimed(), ["oci:app"], "the claim was attempted");
}

/// A stand-in ownership group for manifest-fence tests. `committed` is the epoch a mutation leases (the
/// local view it reads); `current` is the authoritative epoch the fence admits against. When the two
/// differ, a mutation that leased `committed` is fenced, modelling a repository home that transferred
/// while the request was in flight. `homed` records whether this node holds the home, so a test drives
/// home and non-home ingress; the fence reads only the epoch, so a non-home node at the current epoch
/// finalizes exactly like the home.
struct EpochAuthority {
    committed: AtomicU64,
    current: AtomicU64,
    homed: bool,
}

impl EpochAuthority {
    /// A settled authority whose leased and committed epochs agree, so a mutation finalizes.
    fn settled(epoch: u64, homed: bool) -> Arc<Self> {
        Arc::new(Self {
            committed: AtomicU64::new(epoch),
            current: AtomicU64::new(epoch),
            homed,
        })
    }

    /// An authority already advanced past the epoch a mutation leases, so the mutation is fenced.
    fn superseded(leased: u64, current: u64) -> Arc<Self> {
        Arc::new(Self {
            committed: AtomicU64::new(leased),
            current: AtomicU64::new(current),
            homed: true,
        })
    }

    /// Advance the authoritative epoch past the local view, modelling a home failover after a request
    /// has already read its leased epoch, so its commit is fenced.
    fn transfer(&self) {
        self.current.fetch_add(1, Ordering::SeqCst);
    }

    /// Catch the local view up to the current epoch, so a retry after the transfer settles finalizes.
    fn settle(&self) {
        self.committed
            .store(self.current.load(Ordering::SeqCst), Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl OwnershipAuthority for EpochAuthority {
    async fn has_home(&self, _authority: &str) -> bool {
        self.homed
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.committed.load(Ordering::SeqCst)
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        let current = self.current.load(Ordering::SeqCst);
        current != 0 && presented == current
    }

    async fn claim_home(&self, _authority: &str) -> Result<HomeClaim, OwnershipError> {
        Ok(HomeClaim::AlreadyHomed)
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
        // Manifest-fence tests never move a home; the fence reads only the committed and current epochs.
        Ok(None)
    }
}

/// A hosted store at route `store` under a byte quota, so a fenced push has a reservation to release.
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
    app_with(dir, index)
}

/// Delete `store/app`'s manifest reference and return the response status.
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

/// Pull `store/app`'s manifest by reference and return the response status.
async fn pull_status(app: &axum::Router, reference: &str) -> StatusCode {
    send(app, Method::GET, &format!("/v2/store/app/manifests/{reference}"))
        .await
        .0
}

/// A stale-epoch response is protocol-safe: it names no leader, datacenter, or membership, so it leaks
/// no internal control topology.
fn assert_no_topology(body: &Bytes) {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    for leaked in ["leader", "voter", "datacenter", "://", ".internal"] {
        assert!(!text.contains(leaked), "stale-epoch response leaked {leaked:?}: {text}");
    }
}

#[rstest]
#[case::home_ingress(true)]
#[case::non_home_ingress(false)]
#[tokio::test]
async fn test_manifest_push_at_the_current_epoch_publishes(#[case] homed: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    // A repository at a settled epoch admits the push, and a non-home node at that epoch admits it too.
    state.set_ownership_authority(EpochAuthority::settled(5, homed));

    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);
    assert_eq!(pull_status(&app, "v1").await, StatusCode::OK);
}

#[tokio::test]
async fn test_manifest_push_under_a_superseded_epoch_is_unavailable_then_retries() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    // The push leases epoch 5, but the repository home advanced to 6 while the manifest was validated.
    let group = EpochAuthority::superseded(5, 6);
    state.set_ownership_authority(group.clone());

    let (status, _, body) = push_full(&app, "v1", MANIFEST).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_has_code(&body, "UNAVAILABLE"), "{body:?}");
    assert_no_topology(&body);

    // The fenced push exposed nothing: neither the tag nor the digest resolves.
    assert_eq!(pull_status(&app, "v1").await, StatusCode::NOT_FOUND);

    // Once the node catches up to the new epoch, the same push finalizes without re-uploading.
    group.settle();
    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);
    assert_eq!(pull_status(&app, "v1").await, StatusCode::OK);
}

#[tokio::test]
async fn test_tag_replacement_under_a_superseded_epoch_keeps_the_old_target() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let group = EpochAuthority::settled(5, true);
    state.set_ownership_authority(group.clone());

    // Publish the tag against the first manifest at the settled epoch.
    let (status, headers, _) = push_full(&app, "release", MANIFEST).await;
    assert_eq!(status, StatusCode::CREATED);
    let first = headers["docker-content-digest"].to_str().unwrap().to_owned();

    // The home fails over; a push repointing the tag leases the stale epoch and is fenced.
    group.transfer();
    let (status, _, body) = push_full(&app, "release", OTHER_MANIFEST).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_has_code(&body, "UNAVAILABLE"), "{body:?}");

    // The tag still resolves to the first manifest: a stale epoch cannot overwrite a tag.
    let (status, headers, _) = send(&app, Method::GET, "/v2/store/app/manifests/release").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], first);
}

#[tokio::test]
async fn test_manifest_delete_is_fenced_by_the_repository_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let group = EpochAuthority::settled(9, true);
    state.set_ownership_authority(group.clone());
    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);

    // The home fails over; a delete leasing the stale epoch is fenced and the manifest survives.
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

    // Once the node catches up, the delete applies and the tag is gone.
    group.settle();
    assert_eq!(delete(&app, "v1").await, StatusCode::ACCEPTED);
    assert_eq!(pull_status(&app, "v1").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_manifest_restore_is_fenced_by_the_repository_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted_writable(&dir, TOKEN);
    let group = EpochAuthority::settled(3, true);
    state.set_ownership_authority(group.clone());
    assert_eq!(push(&app, "v1").await, StatusCode::CREATED);
    assert_eq!(delete(&app, "v1").await, StatusCode::ACCEPTED);

    // A restore leasing a superseded epoch cannot re-expose the trashed tag.
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

    // After catching up, the restore brings the tag back.
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
    // The metered push reserves bytes, then loses the epoch race and is fenced.
    state.set_ownership_authority(EpochAuthority::superseded(5, 6));

    let (status, ..) = push_full(&app, "v1", MANIFEST).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // The fenced push returned its reservation, so the repository holds no phantom reserved bytes.
    assert_eq!(state.meta.quota_usage("store").unwrap().accounted_bytes.reserved, 0);
}

#[tokio::test]
async fn test_epoch_authority_double_reports_no_group_topology() {
    // The double stands in for a running group without one; the fence never reads its status or moves a
    // home, so these arms exist only to satisfy the trait and report an empty, topology-free group.
    let group = EpochAuthority::settled(4, true);
    let status = group.cluster_status();
    assert_eq!((status.leader, status.term, status.voters), (None, 4, Vec::new()));
    assert_eq!(group.transfer_home("app", "west").await.unwrap(), None);
}
