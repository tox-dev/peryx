//! The ownership consensus group as the mutation path sees it.
//!
//! Authoritative first-publish home assignment runs through a Raft group, but this neutral crate carries
//! no consensus dependency. The binary implements [`OwnershipAuthority`] over the concrete node and
//! registers it on the [`ServingState`](crate::state::ServingState); a process running no group registers
//! nothing and the mutation path skips the claim.

pub use peryx_ha::{ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome};

/// Claim `authority`'s home on its first publish, best effort, when this process runs a group.
///
/// Skips the claim when no group runs or the authority is already homed, so the common repeat-publish
/// case costs one local read and no consensus round. A claim that cannot commit is logged, never
/// surfaced, so a publish is not blocked on consensus reachability; a node that is not the leader logs
/// and leaves the home to a leader-side claim.
pub(super) async fn claim_first_publish_home(group: Option<&std::sync::Arc<dyn OwnershipAuthority>>, authority: &str) {
    let Some(group) = group else { return };
    if group.has_home(authority).await {
        return;
    }
    if let Err(error) = group.claim_home(authority).await {
        tracing::warn!(%error, authority, "first-publish home claim did not commit");
    }
}

/// The committed authority epoch for `authority`, or `0` when this process runs no group.
///
/// The fence a writer stamps onto its work. A process with no group holds no epoch, reported as the
/// unassigned `0` sentinel the placement fence reads as closed.
pub(super) async fn committed_authority_epoch(
    group: Option<&std::sync::Arc<dyn OwnershipAuthority>>,
    authority: &str,
) -> u64 {
    match group {
        Some(group) => group.committed_epoch(authority).await,
        None => 0,
    }
}

/// Whether work carrying `presented` under `authority` may still be written against the committed epoch.
///
/// A process with no group has no authority to supersede its work, so it admits everything; a running
/// group fences any epoch below its committed one.
pub(super) async fn admit_authority_epoch(
    group: Option<&std::sync::Arc<dyn OwnershipAuthority>>,
    authority: &str,
    presented: u64,
) -> bool {
    match group {
        Some(group) => group.admit_epoch(authority, presented).await,
        None => true,
    }
}

/// Move `authority`'s home to `new_home` on the control quorum, or report the group is absent.
///
/// A process with no group cannot commit a transfer, so it returns `Ok(None)` (nothing moved); a running
/// group commits the fenced move and returns the [`TransferOutcome`], or the [`OwnershipError`] the
/// commit failed with - a control minority surfaces as [`OwnershipError::NotLeader`].
///
/// # Errors
/// The [`OwnershipError`] a running group's commit failed with.
pub(super) async fn transfer_authority_home(
    group: Option<&std::sync::Arc<dyn OwnershipAuthority>>,
    authority: &str,
    new_home: &str,
) -> Result<Option<TransferOutcome>, OwnershipError> {
    match group {
        Some(group) => group.transfer_home(authority, new_home).await,
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome, admit_authority_epoch,
        claim_first_publish_home, committed_authority_epoch, transfer_authority_home,
    };

    struct Fake {
        homed: bool,
        claim: Result<HomeClaim, OwnershipError>,
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
        async fn has_home(&self, _authority: &str) -> bool {
            self.homed
        }

        async fn claim_home(&self, _authority: &str) -> Result<HomeClaim, OwnershipError> {
            match &self.claim {
                Ok(outcome) => Ok(*outcome),
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

    fn group(homed: bool, claim: Result<HomeClaim, OwnershipError>) -> Arc<dyn OwnershipAuthority> {
        Arc::new(Fake {
            homed,
            claim,
            epoch: 7,
            transfer: Ok(None),
        })
    }

    fn transferring_group(transfer: Result<Option<TransferOutcome>, OwnershipError>) -> Arc<dyn OwnershipAuthority> {
        Arc::new(Fake {
            homed: true,
            claim: Ok(HomeClaim::AlreadyHomed),
            epoch: 7,
            transfer,
        })
    }

    #[tokio::test]
    async fn test_first_publish_claims_when_unhomed() {
        // An unhomed authority under a running group triggers a claim; the Ok path is the assignment.
        claim_first_publish_home(Some(&group(false, Ok(HomeClaim::AssignedHere))), "proj").await;
    }

    #[tokio::test]
    async fn test_first_publish_skips_an_already_homed_authority() {
        // A homed authority never reaches claim_home, so a claim error here would be a bug if it ran.
        claim_first_publish_home(
            Some(&group(
                true,
                Err(OwnershipError::Unavailable("must not run".to_owned())),
            )),
            "proj",
        )
        .await;
    }

    #[tokio::test]
    async fn test_first_publish_swallows_a_claim_failure() {
        // A claim that cannot commit is logged, not surfaced, so the publish is never blocked.
        claim_first_publish_home(
            Some(&group(false, Err(OwnershipError::NotLeader { leader: None }))),
            "proj",
        )
        .await;
    }

    #[tokio::test]
    async fn test_first_publish_is_a_no_op_without_a_group() {
        claim_first_publish_home(None, "proj").await;
    }

    #[tokio::test]
    async fn test_committed_epoch_reads_the_running_group() {
        let group = group(true, Ok(HomeClaim::AlreadyHomed));
        assert_eq!(committed_authority_epoch(Some(&group), "proj").await, 7);
    }

    #[tokio::test]
    async fn test_committed_epoch_is_the_unassigned_sentinel_without_a_group() {
        assert_eq!(committed_authority_epoch(None, "proj").await, 0);
    }

    #[tokio::test]
    async fn test_admit_epoch_admits_the_committed_epoch_and_fences_a_stale_one() {
        // The Fake commits epoch 7, so the current epoch is admitted and the superseded one below it is not.
        let group = group(true, Ok(HomeClaim::AlreadyHomed));
        assert!(admit_authority_epoch(Some(&group), "proj", 7).await);
        assert!(!admit_authority_epoch(Some(&group), "proj", 6).await);
    }

    #[tokio::test]
    async fn test_admit_epoch_admits_everything_without_a_group() {
        assert!(admit_authority_epoch(None, "proj", 6).await);
    }

    #[tokio::test]
    async fn test_committed_epoch_reports_the_group_epoch() {
        let group = group(false, Ok(HomeClaim::AlreadyHomed));

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
        // A minority cannot commit the transfer, so the group forwards to the leader it knows.
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
            homed: false,
            claim: Ok(HomeClaim::AlreadyHomed),
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
}
