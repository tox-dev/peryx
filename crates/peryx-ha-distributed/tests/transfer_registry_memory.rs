//! A coordinator that has resolved a transfer must hold a fixed retention window, not one plan per
//! authority it has ever moved.

use std::alloc::System;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use peryx_ha::{
    CommandOutcome, CommandReceipt, ControlCommand, ControlCommit, ControlError, MembershipControl, OwnershipError,
    PendingTransferAudit, TransferAudit as StoredTransferAudit,
};
use peryx_ha_distributed::{
    AuthorityKey, ControlPlane, DatacenterId, FrontierSource, TransferAuditOutbox, TransferCoordinator,
    TransferRequest, TransferRunError,
};
use peryx_storage::meta::MetaStore;
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};

/// Enough distinct authorities to separate a per-transfer leak from the fixed retention window.
const ABANDONED: usize = 5_000;
/// The abandonment window this coordinator keeps.
const RETAINED: usize = 256;
/// The window and its authority keys, with headroom for the one sealed audit the run also writes.
const MAX_RETAINED_BYTES: isize = 128 << 10;
const BARRIER: u64 = 1;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

struct BelowBarrier;

#[async_trait]
impl FrontierSource for BelowBarrier {
    async fn applied_frontier(&self, _datacenter: &str) -> anyhow::Result<Option<u64>> {
        Ok(Some(0))
    }
}

#[derive(Default)]
struct Committing {
    pending: Mutex<Option<PendingTransferAudit>>,
}

#[async_trait]
impl MembershipControl for Committing {
    async fn submit(&self, key: Option<&str>, command: ControlCommand) -> Result<ControlCommit, ControlError> {
        let ControlCommand::TransferAuthority {
            authority,
            new_home,
            intent: Some(intent),
        } = command
        else {
            return Err(ControlError::Unavailable("expected a transfer command".to_owned()));
        };
        let Some(id) = key else {
            return Err(ControlError::Unavailable("expected a transfer identity".to_owned()));
        };
        let audit = StoredTransferAudit {
            authority,
            source: intent.source,
            target: new_home,
            actor: intent.actor,
            reason: intent.reason,
            barrier: intent.barrier,
            epoch: 3,
            commit_term: 1,
            commit_index: 9,
        };
        *self.pending.lock().unwrap() = Some(PendingTransferAudit {
            id: id.to_owned(),
            audit: audit.clone(),
        });
        Ok(ControlCommit::committed(CommandReceipt {
            term: 1,
            index: 9,
            outcome: CommandOutcome::Committed,
            old_voters: Vec::new(),
            new_voters: Vec::new(),
            transfer_audit: Some(Box::new(audit)),
        }))
    }
}

#[async_trait]
impl TransferAuditOutbox for Committing {
    async fn pending_transfer_audits(&self) -> Result<Vec<PendingTransferAudit>, OwnershipError> {
        Ok(self.pending.lock().unwrap().iter().cloned().collect())
    }

    async fn complete_transfer_audit(&self, id: &str) -> Result<(), OwnershipError> {
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.as_ref().is_some_and(|audit| audit.id == id) {
                *pending = None;
            }
        }
        Ok(())
    }
}

fn request(authority: String, barrier: u64) -> TransferRequest {
    TransferRequest {
        id: format!("transfer-{authority}"),
        authority: AuthorityKey(authority),
        source: DatacenterId("east".to_owned()),
        target: DatacenterId("west".to_owned()),
        actor: "olivia".to_owned(),
        reason: "drain the east datacenter".to_owned(),
        barrier,
    }
}

fn resident() -> isize {
    let stats = ALLOCATOR.stats();
    isize::try_from(stats.bytes_allocated).unwrap() - isize::try_from(stats.bytes_deallocated).unwrap()
}

#[tokio::test]
async fn test_a_coordinator_holds_its_retention_window_not_every_transfer_it_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let consensus = Arc::new(Committing::default());
    let plane = ControlPlane::new(consensus.clone(), Arc::new(|| 0));
    let coordinator = TransferCoordinator::with_schedule(Arc::new(BelowBarrier), Duration::ZERO, 1, RETAINED);
    let baseline = resident();

    for index in 0..ABANDONED {
        let error = coordinator
            .run(
                request(format!("project-{index:06}"), BARRIER),
                &plane,
                consensus.as_ref(),
                &meta,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, TransferRunError::BarrierNotReached));
    }
    let audit = coordinator
        .run(request("committed".to_owned(), 0), &plane, consensus.as_ref(), &meta)
        .await
        .unwrap();

    let retained = resident() - baseline;
    assert_eq!(audit.commit_index, 9);
    assert!(retained < MAX_RETAINED_BYTES);
    assert_eq!(consensus.pending_transfer_audits().await.unwrap(), Vec::new());
}
