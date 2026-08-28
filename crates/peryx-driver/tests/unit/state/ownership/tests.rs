use std::sync::{Arc, Mutex};

use super::{
    ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome, admit_authority_epoch,
    claim_first_publish_home, committed_authority_epoch, transfer_authority_home,
};

struct Fake {
    claim: Result<HomeClaim, OwnershipError>,
    claims: Arc<Mutex<Vec<String>>>,
    epoch: u64,
    transfer: Result<Option<TransferOutcome>, OwnershipError>,
}

fn clone_ownership_error(error: &OwnershipError) -> OwnershipError {
    match error {
        OwnershipError::NotLeader { leader } => OwnershipError::NotLeader { leader: leader.clone() },
        OwnershipError::Unavailable(reason) => OwnershipError::Unavailable(reason.clone()),
    }
}

#[async_trait::async_trait]
impl OwnershipAuthority for Fake {
    async fn claim_home(&self, authority: &str) -> Result<HomeClaim, OwnershipError> {
        self.claims.lock().unwrap().push(authority.to_owned());
        match &self.claim {
            Ok(outcome) => Ok(outcome.clone()),
            Err(OwnershipError::NotLeader { leader }) => Err(OwnershipError::NotLeader { leader: leader.clone() }),
            Err(OwnershipError::Unavailable(reason)) => Err(OwnershipError::Unavailable(reason.clone())),
        }
    }

    fn cluster_status(&self) -> ClusterStatus {
        ClusterStatus {
            leader: Some("east".to_owned()),
            term: 3,
            voters: vec!["east".to_owned()],
        }
    }

    async fn committed_epoch(&self, _authority: &str) -> u64 {
        self.epoch
    }

    async fn admit_epoch(&self, _authority: &str, presented: u64) -> bool {
        self.epoch != 0 && presented == self.epoch
    }

    async fn transfer_home(
        &self,
        _authority: &str,
        _new_home: &str,
    ) -> Result<Option<TransferOutcome>, OwnershipError> {
        match &self.transfer {
            Ok(outcome) => Ok(outcome.clone()),
            Err(error) => Err(clone_ownership_error(error)),
        }
    }
}

fn group(claim: Result<HomeClaim, OwnershipError>) -> Arc<dyn OwnershipAuthority> {
    Arc::new(Fake {
        claim,
        claims: Arc::default(),
        epoch: 7,
        transfer: Ok(None),
    })
}

fn home_claim() -> HomeClaim {
    HomeClaim {
        home: "east".to_owned(),
        epoch: 7,
    }
}

fn transferring_group(transfer: Result<Option<TransferOutcome>, OwnershipError>) -> Arc<dyn OwnershipAuthority> {
    Arc::new(Fake {
        claim: Ok(home_claim()),
        claims: Arc::default(),
        epoch: 7,
        transfer,
    })
}

#[tokio::test]
async fn test_first_publish_resolves_the_committed_snapshot() {
    let claims = Arc::new(Mutex::new(Vec::new()));
    let authority: Arc<dyn OwnershipAuthority> = Arc::new(Fake {
        claim: Ok(home_claim()),
        claims: claims.clone(),
        epoch: 7,
        transfer: Ok(None),
    });

    assert_eq!(
        claim_first_publish_home(Some(&authority), "resource-a").await.unwrap(),
        Some(home_claim())
    );
    assert_eq!(*claims.lock().unwrap(), ["resource-a"]);
}

#[tokio::test]
async fn test_first_publish_surfaces_a_claim_failure() {
    let claims = Arc::new(Mutex::new(Vec::new()));
    let authority: Arc<dyn OwnershipAuthority> = Arc::new(Fake {
        claim: Err(OwnershipError::NotLeader { leader: None }),
        claims: claims.clone(),
        epoch: 7,
        transfer: Ok(None),
    });

    assert!(matches!(
        claim_first_publish_home(Some(&authority), "resource-a").await,
        Err(OwnershipError::NotLeader { leader: None })
    ));
    assert_eq!(*claims.lock().unwrap(), ["resource-a"]);
}

#[tokio::test]
async fn test_first_publish_is_a_no_op_without_a_group() {
    assert_eq!(claim_first_publish_home(None, "resource-a").await.unwrap(), None);
}

#[tokio::test]
async fn test_committed_epoch_reads_the_running_group() {
    let group = group(Ok(home_claim()));
    assert_eq!(committed_authority_epoch(Some(&group), "proj").await, 7);
}

#[tokio::test]
async fn test_committed_epoch_is_the_unassigned_sentinel_without_a_group() {
    assert_eq!(committed_authority_epoch(None, "proj").await, 0);
}

#[tokio::test]
async fn test_admit_epoch_admits_the_committed_epoch_and_fences_a_stale_one() {
    let group = group(Ok(home_claim()));
    assert!(admit_authority_epoch(Some(&group), "proj", 7).await);
    assert!(!admit_authority_epoch(Some(&group), "proj", 6).await);
}

#[tokio::test]
async fn test_admit_epoch_admits_everything_without_a_group() {
    assert!(admit_authority_epoch(None, "proj", 6).await);
}

#[tokio::test]
async fn test_committed_epoch_reports_the_group_epoch() {
    let group = group(Ok(home_claim()));

    assert_eq!(group.committed_epoch("proj").await, 7);
}

#[tokio::test]
async fn test_transfer_commits_the_move_through_the_group() {
    let outcome = TransferOutcome {
        from: "east".to_owned(),
        to: "west".to_owned(),
        epoch: 2,
    };
    let group = transferring_group(Ok(Some(outcome.clone())));

    let moved = transfer_authority_home(Some(&group), "proj", "west")
        .await
        .expect("commits");
    assert_eq!(moved, Some(outcome));
}

#[tokio::test]
async fn test_transfer_by_a_control_minority_is_not_the_leader() {
    let group = transferring_group(Err(OwnershipError::NotLeader {
        leader: Some("east.internal:4460".to_owned()),
    }));

    let error = transfer_authority_home(Some(&group), "proj", "west").await.unwrap_err();
    assert!(matches!(error, OwnershipError::NotLeader { leader: Some(_) }));
}

#[tokio::test]
async fn test_transfer_that_cannot_commit_surfaces_unavailable() {
    let group = transferring_group(Err(OwnershipError::Unavailable("log store gone".to_owned())));

    let error = transfer_authority_home(Some(&group), "proj", "west").await.unwrap_err();
    assert!(matches!(error, OwnershipError::Unavailable(reason) if reason == "log store gone"));
}

#[tokio::test]
async fn test_transfer_without_a_group_moves_nothing() {
    let moved = transfer_authority_home(None, "proj", "west")
        .await
        .expect("no group moves nothing");
    assert_eq!(moved, None);
}

#[test]
fn test_cluster_status_snapshots_the_group() {
    let status = Fake {
        claim: Ok(home_claim()),
        claims: Arc::default(),
        epoch: 0,
        transfer: Ok(None),
    }
    .cluster_status();

    assert_eq!(status.leader, Some("east".to_owned()));
    assert_eq!(status.term, 3);
    assert_eq!(status.voters, vec!["east".to_owned()]);
}

#[test]
fn test_not_leader_names_the_known_leader() {
    let error = OwnershipError::NotLeader {
        leader: Some("east.internal:4460".to_owned()),
    };

    assert_eq!(
        error.to_string(),
        "not the ownership leader; leader at east.internal:4460"
    );
}

#[test]
fn test_not_leader_omits_an_unknown_leader() {
    let error = OwnershipError::NotLeader { leader: None };

    assert_eq!(error.to_string(), "not the ownership leader");
}

#[test]
fn test_unavailable_carries_its_reason() {
    let error = OwnershipError::Unavailable("log store gone".to_owned());

    assert_eq!(error.to_string(), "ownership claim did not commit: log store gone");
}
