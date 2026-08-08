use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Context as _;
use peryx_driver::state::ServingState;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::DataCenterId;

use crate::config::Config;

pub struct FilesystemPlacementReconciler(peryx_ha_distributed::FilesystemPlacementReconciler);

impl FilesystemPlacementReconciler {
    /// # Errors
    /// Returns an error when a configured datacenter is invalid.
    pub fn from_config(config: &Config, store: BlobStore) -> anyhow::Result<Option<Self>> {
        let (Some(membership), Some(identity)) = (
            config.dc_membership.as_ref(),
            config.node_identity.as_deref().or(config.writer_identity.as_deref()),
        ) else {
            return Ok(None);
        };
        let Some(local) = membership.members.iter().find(|member| member.node == identity) else {
            return Ok(None);
        };
        let local_dc =
            DataCenterId::new(&local.dc).context("the local datacenter is not a valid placement component")?;
        let target_dcs = membership
            .members
            .iter()
            .map(|member| {
                DataCenterId::new(&member.dc)
                    .with_context(|| format!("datacenter {:?} is not a valid placement component", member.dc))
            })
            .collect::<anyhow::Result<BTreeSet<_>>>()?;
        Ok(peryx_ha_distributed::FilesystemPlacementReconciler::new(local_dc, store, target_dcs).map(Self))
    }

    pub fn bind(self, state: Arc<ServingState>) -> Arc<dyn peryx_ha::PlacementReconciler> {
        self.0.bind(state)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/availability/placement_reconcile/tests.rs"]
mod tests;
