use std::collections::BTreeSet;
use std::sync::Arc;

use peryx_driver::serving::EcosystemDriver;
use peryx_driver::state::ServingState;
use peryx_ha::{ReclamationFrontiers, ReferenceInventory};
use peryx_storage::meta::MetaStore;

use crate::config::Config;

struct DriverReferences {
    index_names: Vec<String>,
}

impl ReferenceInventory for DriverReferences {
    fn referenced(&self, meta: &MetaStore) -> Result<BTreeSet<String>, String> {
        collect_references(
            crate::app::referenced_blob_digests(meta),
            crate::server::drivers().present(),
            meta,
            &self.index_names,
        )
    }
}

fn collect_references<'a>(
    base: anyhow::Result<BTreeSet<String>>,
    drivers: impl Iterator<Item = &'a Arc<dyn EcosystemDriver>>,
    meta: &MetaStore,
    index_names: &[String],
) -> Result<BTreeSet<String>, String> {
    let mut referenced = base.map_err(|reason| reason.to_string())?;
    for (driver, trash) in drivers.filter_map(|driver| driver.capabilities().trash.map(|trash| (driver, trash))) {
        for record in trash
            .trash_records(meta, index_names)
            .map_err(|reason| format!("scan {} trash: {reason}", driver.ecosystem().as_str()))?
        {
            referenced.extend(
                record
                    .digest
                    .map(|digest| digest.strip_prefix("sha256:").unwrap_or(&digest).to_owned()),
            );
        }
    }
    Ok(referenced)
}

pub struct BlobReclamationSelector(peryx_ha_distributed::BlobReclamationSelector);

impl BlobReclamationSelector {
    pub fn from_config(config: &Config, frontiers: Arc<dyn ReclamationFrontiers>) -> Option<Self> {
        let (Some(membership), Some(identity)) = (config.dc_membership.as_ref(), config.writer_identity.as_deref())
        else {
            return None;
        };
        membership
            .members
            .iter()
            .any(|member| member.node == identity)
            .then(|| {
                Self(peryx_ha_distributed::BlobReclamationSelector::new(
                    Arc::new(DriverReferences {
                        index_names: config.indexes.iter().map(|index| index.name.clone()).collect(),
                    }),
                    frontiers,
                ))
            })
    }

    pub fn bind(self, state: Arc<ServingState>) -> Arc<dyn peryx_ha::BlobReclaimer> {
        self.0.bind(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DcMember, DcMembership, DcRole};
    use peryx_driver::state::AppState;
    use peryx_storage::blob::BlobStorage;
    use peryx_storage::meta::ObservedFrontier;

    struct Frontiers;

    impl ReclamationFrontiers for Frontiers {
        fn observe(&self) -> Option<ObservedFrontier> {
            None
        }
    }

    #[test]
    fn missing_membership_has_no_reclaimer() {
        assert!(BlobReclamationSelector::from_config(&Config::default(), Arc::new(Frontiers)).is_none());
    }

    #[test]
    fn reference_collection_surfaces_the_base_scan_failure() {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

        assert_eq!(
            collect_references(
                Err(anyhow::anyhow!("failed")),
                std::iter::empty::<&Arc<dyn EcosystemDriver>>(),
                &meta,
                &[],
            )
            .unwrap_err(),
            "failed"
        );
    }

    #[tokio::test]
    async fn rostered_node_builds_and_runs_the_reclaimer() {
        let config = Config {
            writer_identity: Some("local".to_owned()),
            dc_membership: Some(DcMembership {
                group: "group".to_owned(),
                members: vec![DcMember {
                    node: "local".to_owned(),
                    dc: "home".to_owned(),
                    address: "http://local/".to_owned(),
                    role: DcRole::Writer,
                }],
            }),
            ..Config::default()
        };
        let meta_dir = tempfile::tempdir().unwrap();
        let blob_dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::new(
            MetaStore::open(meta_dir.path().join("peryx.redb")).unwrap(),
            BlobStorage::filesystem(blob_dir.path().join("blobs")),
            60,
            Vec::new(),
        ));
        let reclaimer = BlobReclamationSelector::from_config(&config, Arc::new(Frontiers)).unwrap();

        assert_eq!(
            reclaimer
                .bind(state.serving.clone())
                .reclaim_pass(&|| false, 1, std::num::NonZeroUsize::MIN)
                .await
                .unwrap(),
            peryx_ha::AvailabilityTaskReport::default()
        );
    }
}
