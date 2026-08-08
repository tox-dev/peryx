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
#[path = "../../tests/unit/availability/reclamation/tests.rs"]
mod tests;
