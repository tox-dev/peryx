use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use peryx_driver::{
    AppState, ServingStateAvailabilityAuthorizer, ServingStateControlAuthorizer, ServingStateMetadataFrontierProvider,
};
use peryx_ha_distributed::{
    DistributedMode, DistributedRuntime, DistributedRuntimeContext, RuntimeConfig, RuntimeMember, RuntimeMemberRole,
    RuntimeMembership, RuntimeRole,
};

use crate::config::{AvailabilityConfig, Config, DcRole, ReplicationConfig};

pub struct ReplicationRuntime {
    runtime: DistributedRuntime,
    config: RuntimeConfig,
}

impl ReplicationRuntime {
    /// # Errors
    /// Returns an error when availability is disabled or distributed startup preparation fails.
    pub fn new(config: &Config, state: &Arc<AppState>) -> anyhow::Result<Self> {
        let config = runtime_config(config)?.context("distributed runtime requested while availability is disabled")?;
        let serving = state.serving.clone();
        let runtime = DistributedRuntime::new(
            &config,
            &DistributedRuntimeContext {
                meta: serving.meta.clone(),
                blobs: serving.blobs.clone(),
                clock: serving.clock.clone(),
                replica_views: state.clone(),
                analytics: Arc::new(serving.metrics.clone()),
                frontier: Arc::new(ServingStateMetadataFrontierProvider::new(serving.clone())),
            },
            Arc::new(ServingStateAvailabilityAuthorizer::new(serving)),
        );
        let runtime = runtime?;
        Ok(Self { runtime, config })
    }

    pub fn routes(&self) -> Router {
        peryx_ha::AvailabilityRuntime::routes(&self.runtime)
    }

    /// # Errors
    /// Returns an error when distributed preparation fails.
    pub async fn prepare(
        self,
        state: &Arc<AppState>,
        references: Arc<dyn peryx_ha::ReferenceInventory>,
        listener: Option<Box<dyn peryx_ha_distributed::PreparedAvailabilityListener>>,
    ) -> anyhow::Result<peryx_ha::PreparedAvailability<Router, peryx_ha_distributed::DistributedHandle>> {
        peryx_ha::AvailabilityRuntime::prepare(
            self.runtime,
            peryx_ha_distributed::DistributedPrepareContext {
                config: self.config,
                state: state.clone(),
                control_authorizer: Arc::new(ServingStateControlAuthorizer::new(state.serving.clone())),
                references,
                listener,
            },
        )
        .await
    }
}

pub(crate) fn runtime_config(config: &Config) -> anyhow::Result<Option<RuntimeConfig>> {
    let (mode, role) = match &config.availability {
        AvailabilityConfig::None => return Ok(None),
        AvailabilityConfig::Dc(replication) => (DistributedMode::Dc, project_role(replication)?),
        AvailabilityConfig::Ha(replication) => (DistributedMode::Ha, project_role(replication)?),
    };
    Ok(Some(RuntimeConfig {
        mode,
        role,
        write_ack_policy: config.write_ack.policy,
        membership: config.dc_membership.as_ref().map(|membership| RuntimeMembership {
            group: membership.group.clone(),
            members: membership
                .members
                .iter()
                .map(|member| RuntimeMember {
                    node: member.node.clone(),
                    datacenter: member.dc.clone(),
                    address: member.address.clone(),
                    role: match member.role {
                        DcRole::Writer => RuntimeMemberRole::Writer,
                        DcRole::Replica => RuntimeMemberRole::Replica,
                    },
                })
                .collect(),
        }),
        node_identity: config.node_identity.clone(),
        writer_identity: config.writer_identity.clone(),
        data_dir: config.data_dir.clone(),
        read_through: config.read_through,
    }))
}

fn project_role(replication: &ReplicationConfig) -> anyhow::Result<RuntimeRole> {
    match replication {
        ReplicationConfig::Primary { source, token } => Ok(RuntimeRole::Primary {
            source: source.clone(),
            token: token.read().context("read the primary replication token")?,
        }),
        ReplicationConfig::Replica {
            upstream,
            token,
            poll_interval,
            page_size,
        } => Ok(RuntimeRole::Replica {
            upstream: upstream.clone(),
            token: token.read().context("read the replica replication token")?,
            poll_interval: *poll_interval,
            page_size: *page_size,
        }),
    }
}

#[cfg(test)]
#[path = "../tests/unit/replication/tests.rs"]
mod tests;
