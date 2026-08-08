use anyhow::Context as _;
use peryx_ha_distributed::ConsensusMember;
pub use peryx_ha_distributed::{ConsensusPlan, OwnershipGroup};

use crate::config::{AvailabilityConfig, Config, DcRole, ReplicationConfig};

const LOG_STORE_SUBPATH: &str = "raft/ownership-log.redb";

pub(super) fn consensus_plan(config: &Config) -> anyhow::Result<Option<ConsensusPlan>> {
    let AvailabilityConfig::Ha(replication) = &config.availability else {
        return Ok(None);
    };
    let Some(membership) = config.dc_membership.as_ref() else {
        return Ok(None);
    };
    let identity = config
        .node_identity
        .as_deref()
        .context("an `ha` consensus roster needs a `node-identity` naming this node's own member entry")?;
    let local = membership
        .members
        .iter()
        .find(|member| member.node == identity)
        .with_context(|| format!("this node's identity {identity:?} is not a member of the roster"))?;
    let (ReplicationConfig::Primary { token, .. } | ReplicationConfig::Replica { token, .. }) = replication;
    let token = token.read().context("read the shared consensus peer token")?;
    let members = membership
        .members
        .iter()
        .map(|member| ConsensusMember {
            datacenter: member.dc.clone(),
            address: member.address.clone(),
        })
        .collect::<Vec<_>>();
    ConsensusPlan::new(
        local.dc.clone(),
        local.role == DcRole::Writer,
        &members,
        config.data_dir.join(LOG_STORE_SUBPATH),
        membership.group.clone(),
        token,
    )
    .map(Some)
}

#[cfg(test)]
#[path = "../../tests/unit/replication/raft/tests.rs"]
mod tests;
