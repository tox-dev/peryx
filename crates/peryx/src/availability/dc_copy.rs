use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;

use anyhow::Context as _;
use peryx_driver::state::ServingState;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{BackendId, DataCenterId};

use crate::config::{Config, DcMembership, DcRole, ReplicationConfig, SecretSource};

pub struct CrossDcBlobCopier(peryx_ha_distributed::CrossDcBlobCopier);

impl CrossDcBlobCopier {
    /// # Errors
    /// Returns an error when the local datacenter or replication credential is invalid.
    pub fn from_config(config: &Config, store: BlobStore, backend: BackendId) -> anyhow::Result<Option<Self>> {
        let (Some(membership), Some(identity), Some(replication)) = (
            config.dc_membership.as_ref(),
            config.node_identity.as_deref().or(config.writer_identity.as_deref()),
            config.availability.replication(),
        ) else {
            return Ok(None);
        };
        let Some(local) = membership.members.iter().find(|member| member.node == identity) else {
            return Ok(None);
        };
        let local_dc =
            DataCenterId::new(&local.dc).context("the local datacenter is not a valid placement component")?;
        let token = replication_token(replication)
            .read()
            .context("read the replication token for cross-datacenter copy")?;
        Ok(peryx_ha_distributed::CrossDcBlobCopier::http(
            local_dc,
            source_roster(membership, &local.dc),
            token,
            store,
            backend,
        )
        .map(Self))
    }

    pub fn bind(self, state: Arc<ServingState>) -> Arc<dyn peryx_ha::CrossDcCopier> {
        self.0.bind(state)
    }
}

const fn replication_token(replication: &ReplicationConfig) -> &SecretSource {
    match replication {
        ReplicationConfig::Primary { token, .. } | ReplicationConfig::Replica { token, .. } => token,
    }
}

fn source_roster(membership: &DcMembership, local_dc: &str) -> HashMap<String, String> {
    let mut roster = HashMap::new();
    for member in &membership.members {
        if member.dc == local_dc {
            continue;
        }
        match roster.entry(member.dc.clone()) {
            Entry::Vacant(slot) => {
                slot.insert(member.address.clone());
            }
            Entry::Occupied(mut slot) if member.role == DcRole::Writer => {
                slot.insert(member.address.clone());
            }
            Entry::Occupied(_) => {}
        }
    }
    roster
}

#[cfg(test)]
#[path = "../../tests/unit/availability/dc_copy/tests.rs"]
mod tests;
