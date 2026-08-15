use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use peryx_driver::state::{ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome};

use super::support::*;

struct RecordingAuthority {
    homed: HashSet<String>,
    fail_claim: bool,
    epoch: u64,
    admit: bool,
    checked: Mutex<Vec<String>>,
    claimed: Mutex<Vec<String>>,
    admitted: Mutex<Vec<u64>>,
}

impl RecordingAuthority {
    fn unhomed() -> Arc<Self> {
        Self::new(HashSet::new(), false, 0, true)
    }

    fn already_homed(authority: &str) -> Arc<Self> {
        Self::new(HashSet::from([authority.to_owned()]), false, 0, true)
    }

    fn failing() -> Arc<Self> {
        Self::new(HashSet::new(), true, 0, true)
    }

    fn homed_at_epoch(authority: &str, epoch: u64, admit: bool) -> Arc<Self> {
        Self::new(HashSet::from([authority.to_owned()]), false, epoch, admit)
    }

    fn new(homed: HashSet<String>, fail_claim: bool, epoch: u64, admit: bool) -> Arc<Self> {
        Arc::new(Self {
            homed,
            fail_claim,
            epoch,
            admit,
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
    async fn has_home(&self, authority: &str) -> bool {
        self.checked.lock().unwrap().push(authority.to_owned());
        self.homed.contains(authority)
    }

    async fn claim_home(&self, authority: &str) -> Result<HomeClaim, OwnershipError> {
        self.claimed.lock().unwrap().push(authority.to_owned());
        if self.fail_claim {
            Err(OwnershipError::Unavailable("ownership group unreachable".to_owned()))
        } else {
            Ok(HomeClaim::AssignedHere)
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
    assert_eq!(group.checked(), ["peryxpkg"], "the path reads the home before claiming");
    assert_eq!(
        group.claimed(),
        ["peryxpkg"],
        "the first stored file claims the normalized project's home",
    );
    // The claimed key is the normalized project key the store uses, so name variants route to one home.
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
async fn test_repeat_publish_of_a_homed_project_makes_no_claim() {
    let h = authority_harness().await;
    let group = RecordingAuthority::already_homed("peryxpkg");
    bind_ownership_authority(&h.state, group.clone());
    let (status, _) = publish(&h.state).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(group.checked(), ["peryxpkg"], "the path reads the existing home");
    assert!(
        group.claimed().is_empty(),
        "an already-homed project costs one local read and no consensus round",
    );
}

#[tokio::test]
async fn test_a_home_claim_that_cannot_commit_does_not_block_the_publish() {
    let h = authority_harness().await;
    let group = RecordingAuthority::failing();
    bind_ownership_authority(&h.state, group.clone());
    let (status, body) = publish(&h.state).await;

    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, "upload accepted"),
        "a claim that cannot commit is logged, never surfaced, and never blocks the publish",
    );
    assert_eq!(group.claimed(), ["peryxpkg"], "the claim was attempted");
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
