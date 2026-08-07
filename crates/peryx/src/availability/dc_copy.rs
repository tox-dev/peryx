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
mod tests {
    use super::*;
    use crate::config::{AvailabilityConfig, DcMember};
    use peryx_driver::state::AppState;
    use peryx_storage::blob::BlobStorage;
    use peryx_storage::meta::MetaStore;

    fn member(node: &str, dc: &str, role: DcRole) -> DcMember {
        DcMember {
            node: node.to_owned(),
            dc: dc.to_owned(),
            address: format!("http://{node}/"),
            role,
        }
    }

    fn distributed_config(token: SecretSource) -> Config {
        Config {
            writer_identity: Some("local".to_owned()),
            availability: AvailabilityConfig::Ha(ReplicationConfig::Primary {
                source: "peer".to_owned(),
                token,
            }),
            dc_membership: Some(DcMembership {
                group: "group".to_owned(),
                members: vec![
                    member("local", "home", DcRole::Writer),
                    member("peer", "east", DcRole::Replica),
                ],
            }),
            ..Config::default()
        }
    }

    fn state() -> (tempfile::TempDir, tempfile::TempDir, Arc<AppState>) {
        let meta_dir = tempfile::tempdir().unwrap();
        let blob_dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(meta_dir.path().join("peryx.redb")).unwrap();
        let state = Arc::new(AppState::new(
            meta,
            BlobStorage::filesystem(blob_dir.path().join("blobs")),
            60,
            Vec::new(),
        ));
        (meta_dir, blob_dir, state)
    }

    #[test]
    fn source_roster_prefers_remote_writers() {
        let membership = DcMembership {
            group: "group".to_owned(),
            members: vec![
                member("local", "home", DcRole::Writer),
                member("east-replica", "east", DcRole::Replica),
                member("east-writer", "east", DcRole::Writer),
            ],
        };

        assert_eq!(source_roster(&membership, "home")["east"], "http://east-writer/");
    }

    #[test]
    fn source_roster_keeps_the_first_replica() {
        let membership = DcMembership {
            group: "group".to_owned(),
            members: vec![
                member("local", "home", DcRole::Writer),
                member("east-a", "east", DcRole::Replica),
                member("east-b", "east", DcRole::Replica),
            ],
        };

        assert_eq!(source_roster(&membership, "home")["east"], "http://east-a/");
    }

    #[test]
    fn replication_token_accepts_both_roles() {
        let primary = ReplicationConfig::Primary {
            source: "peer".to_owned(),
            token: SecretSource::Literal("primary".to_owned()),
        };
        let replica = ReplicationConfig::Replica {
            upstream: "http://writer/".to_owned(),
            token: SecretSource::Literal("replica".to_owned()),
            poll_interval: std::time::Duration::from_secs(1),
            page_size: std::num::NonZeroUsize::MIN,
        };

        assert_eq!(replication_token(&primary).read().unwrap(), "primary");
        assert_eq!(replication_token(&replica).read().unwrap(), "replica");
    }

    #[test]
    fn disabled_availability_has_no_copier() {
        let config = Config {
            availability: AvailabilityConfig::None,
            ..Config::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let backend = BackendId::new("filesystem").unwrap();

        assert!(
            CrossDcBlobCopier::from_config(&config, store, backend)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn distributed_config_builds_and_binds_a_copier() {
        let dir = tempfile::tempdir().unwrap();
        let storage = BlobStorage::filesystem(dir.path().join("blobs"));
        let copier = CrossDcBlobCopier::from_config(
            &distributed_config(SecretSource::Literal("token".to_owned())),
            storage.filesystem_store().unwrap().clone(),
            storage.backend_id(),
        )
        .unwrap()
        .unwrap();
        let (_meta_dir, _blob_dir, state) = state();

        drop(copier.bind(state.serving.clone()));
    }

    #[test]
    fn unreadable_replication_token_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let storage = BlobStorage::filesystem(dir.path().join("blobs"));

        assert!(
            CrossDcBlobCopier::from_config(
                &distributed_config(SecretSource::File(dir.path().join("missing"))),
                storage.filesystem_store().unwrap().clone(),
                storage.backend_id(),
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_local_datacenter_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let storage = BlobStorage::filesystem(dir.path().join("blobs"));
        let mut config = distributed_config(SecretSource::Literal("token".to_owned()));
        config.dc_membership.as_mut().unwrap().members[0].dc = "d".repeat(600);

        assert!(
            CrossDcBlobCopier::from_config(
                &config,
                storage.filesystem_store().unwrap().clone(),
                storage.backend_id(),
            )
            .is_err()
        );
    }
}
