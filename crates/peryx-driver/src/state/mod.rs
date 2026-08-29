mod app;
mod build;
mod caches;
mod derived_views;
mod describe;
mod operation;
mod ownership;
mod registry;
mod traces;

pub use app::{AppState, Clock, ServingState};
pub use build::{DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS, DEFAULT_TOKEN_TTL_SECS, RuntimeOptions};
pub use derived_views::{REQUIRED_VIEWS, ReadableFrontier, SEARCH_VIEW, ViewBlock, readable_frontier};
pub use describe::{
    HostedDescription, IndexDescription, MemberDescription, SecretDescription, UpstreamDescription,
    UpstreamSourceDescription, describe_index, describe_indexes,
};
pub use ownership::{
    AuthorityWriteLease, ClusterStatus, HomeClaim, OwnershipAuthority, OwnershipError, TransferOutcome,
};
pub use peryx_core::PrometheusSource;
pub use peryx_ha::{CommandOutcome, CommandReceipt, ControlCommand, ControlError, MembershipControl, OperationKind};
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
        self.serving.bump_search_epoch();
        let mut blocked = None;
        for driver in self.replicated_apply_drivers() {
            if let Err(block) = driver.apply_replicated_changes(&self.serving, changed_keys) {
                blocked = Some(block);
            }
        }
        if let Some(block) = blocked {
            tracing::warn!(view = %block.view, serial = page.serial, "replica view rebuild blocked the frontier");
        } else if let Err(error) = self.serving.meta.set_view_frontier(SEARCH_VIEW, page.serial) {
            tracing::error!(%error, serial = page.serial, "recording the replica view frontier failed");
        }
    }

    fn readable_frontier(&self) -> u64 {
        self.serving.readable_frontier().map_or(0, |frontier| frontier.serial)
    }

    fn publish_applied_frontier(&self, serial: u64) {
        self.serving.availability.publish_applied_frontier(serial);
    }
}

#[cfg(test)]
#[path = "../../tests/unit/state/tests.rs"]
mod tests;
