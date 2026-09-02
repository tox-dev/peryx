use std::collections::BTreeSet;

mod app;
mod build;
mod caches;
mod derived_views;
mod describe;
mod operation;
mod ownership;
mod registry;
mod revocation;

pub use app::{AppState, Clock, ServingState};
pub use build::{DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS, DEFAULT_TOKEN_TTL_SECS, RuntimeOptions};
pub use derived_views::{REQUIRED_VIEWS, ReadableFrontier, SEARCH_VIEW, ViewBlock, readable_frontier};
pub use describe::{
    HostedDescription, IndexDescription, MemberDescription, SecretDescription, UpstreamDescription,
    UpstreamSourceDescription, describe_index, describe_indexes,
};
pub use ownership::{
    AuthorityWriteLease, ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, SINGLETON_LEASE_SECS,
    SINGLETON_RENEW_SECS, SingletonAcquisition, SingletonLease, SingletonRelease, SingletonRenewal, TransferOutcome,
    singleton_grant_admits,
};
pub use peryx_core::PrometheusSource;
pub use peryx_ha::{
    CommandOutcome, CommandReceipt, ControlCommand, ControlCommit, ControlError, MembershipControl, OperationKind,
};
pub use peryx_index::{Index, IndexKind};

impl peryx_ha::ReplicaViewApplier for AppState {
    fn apply(&self, page: peryx_ha::ReplicaPage, changed_keys: &[String]) {
        if page.changes == 0 {
            return;
        }
        tracing::info!(
            changes = page.changes,
            serial = page.serial,
            primary_serial = page.primary_serial,
            "replica page applied"
        );
        self.serving.revocations.invalidate_replicated(&page.revocations);
        // A revocation names a digest and no index maps one back to the projects that publish it, so
        // it retires the whole view the way an operator's own revocation does.
        let blocked = self.retire_views(changed_keys, !page.revocations.is_empty());
        if let Some(block) = blocked {
            tracing::warn!(view = %block.view, serial = page.serial, "replica view rebuild blocked the frontier");
        } else if let Err(error) = self.serving.meta.set_view_frontier(SEARCH_VIEW, page.serial) {
            tracing::error!(%error, serial = page.serial, "recording the replica view frontier failed");
        }
    }

    /// A replica applies a page before it holds the bytes that page names, so a document derived while
    /// the blob was still absent reports it that way. Retiring the same keys once the bytes land is what
    /// brings local availability up to date, and it costs the resources the page named rather than the
    /// store.
    fn apply_blob_commit(&self, committed: &[peryx_ha::BlobCommit]) {
        if committed.is_empty() {
            return;
        }
        let unnamed = committed.iter().any(|commit| commit.keys.is_empty());
        let keys = committed
            .iter()
            .flat_map(|commit| commit.keys.iter().cloned())
            .collect::<Vec<_>>();
        let (blobs, named) = (committed.len(), keys.len());
        tracing::info!(blobs, named, unnamed, "replica blob commit applied");
        if let Some(block) = self.retire_views(&keys, unnamed) {
            tracing::warn!(view = %block.view, "replica view rebuild blocked after a blob commit");
        }
    }

    fn readable_frontier(&self) -> u64 {
        self.serving.readable_frontier().map_or(0, |frontier| frontier.serial)
    }

    fn publish_applied_frontier(&self, serial: u64) {
        self.serving.availability.publish_applied_frontier(serial);
    }
}

impl AppState {
    /// Hands `changed_keys` to the ecosystem drivers and retires the search epoch for whatever they
    /// left behind, reporting the view a driver could not rebuild.
    ///
    /// The drivers run first and the epoch retires only what they did not report. Retiring it up front
    /// discarded the documents they went on to rebuild and put every later search back on a full
    /// re-derivation, so the narrowed refresh a driver sets up could never be taken. `force_full`
    /// retires it regardless, for a change this node cannot name a resource for.
    fn retire_views(&self, changed_keys: &[String], force_full: bool) -> Option<ViewBlock> {
        let mut blocked = None;
        let mut current = BTreeSet::new();
        for driver in self.replicated_apply_drivers() {
            match driver.apply_replicated_changes(&self.serving, changed_keys) {
                Ok(keys) => current.extend(keys),
                Err(block) => blocked = Some(block),
            }
        }
        if force_full || changed_keys.iter().any(|key| !current.contains(key.as_str())) {
            self.serving.bump_search_epoch();
        }
        blocked
    }
}

#[cfg(test)]
#[path = "../../tests/unit/state/tests.rs"]
mod tests;
