use std::collections::{BTreeSet, HashMap};
use std::fmt::{Display, Formatter};
use std::sync::Arc;

use peryx_core::Ecosystem;

use crate::serving::{
    BlobReferenceDriver, BrowseDriver, CacheDriver, CapabilityRegistrar, EcosystemDriver, FsckDriver, ImportDriver,
    IndexCredentialDriver, IndexSummaryDriver, JobDriver, MetricsDriver, NameDriver, PolicyDriver, PolicyDryRunDriver,
    RetentionDriver, ServiceDriver, TrashDriver,
};

#[derive(Clone, Default)]
pub struct DriverSet {
    drivers: Vec<Arc<dyn EcosystemDriver>>,
    jobs: HashMap<Ecosystem, Arc<dyn JobDriver>>,
    metrics: HashMap<Ecosystem, Arc<dyn MetricsDriver>>,
    names: HashMap<Ecosystem, Arc<dyn NameDriver>>,
    policies: HashMap<Ecosystem, Arc<dyn PolicyDriver>>,
    policy_dry_runs: HashMap<Ecosystem, Arc<dyn PolicyDryRunDriver>>,
    blob_references: HashMap<Ecosystem, Arc<dyn BlobReferenceDriver>>,
    fsck: HashMap<Ecosystem, Arc<dyn FsckDriver>>,
    retention: HashMap<Ecosystem, Arc<dyn RetentionDriver>>,
    cache: HashMap<Ecosystem, Arc<dyn CacheDriver>>,
    index_summaries: HashMap<Ecosystem, Arc<dyn IndexSummaryDriver>>,
    trash: HashMap<Ecosystem, Arc<dyn TrashDriver>>,
    imports: HashMap<Ecosystem, Arc<dyn ImportDriver>>,
    services: HashMap<Ecosystem, Arc<dyn ServiceDriver>>,
    browse: HashMap<Ecosystem, Arc<dyn BrowseDriver>>,
    index_credentials: HashMap<Ecosystem, Arc<dyn IndexCredentialDriver>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobReferenceScan {
    pub ecosystems: Vec<String>,
    pub digests: BTreeSet<String>,
}

#[derive(Debug)]
pub enum BlobReferenceScanError {
    Store(peryx_storage::meta::MetaError),
    MissingDrivers(BTreeSet<String>),
    Driver { ecosystem: Ecosystem, reason: String },
}

impl Display for BlobReferenceScanError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "read repository ecosystems: {error}"),
            Self::MissingDrivers(ecosystems) => write!(
                formatter,
                "metadata contains repositories for ecosystems without blob-reference drivers: {}",
                ecosystems.iter().map(String::as_str).collect::<Vec<_>>().join(", ")
            ),
            Self::Driver { ecosystem, reason } => write!(formatter, "scan {ecosystem} blob references: {reason}"),
        }
    }
}

impl std::error::Error for BlobReferenceScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::MissingDrivers(_) | Self::Driver { .. } => None,
        }
    }
}

impl DriverSet {
    #[must_use]
    pub fn with(mut self, driver: Arc<dyn EcosystemDriver>) -> Self {
        self.insert(driver);
        self
    }

    pub(crate) fn insert(&mut self, driver: Arc<dyn EcosystemDriver>) {
        let ecosystem = driver.ecosystem();
        if let Some(existing) = self
            .drivers
            .iter_mut()
            .find(|existing| existing.ecosystem() == ecosystem)
        {
            *existing = driver;
        } else {
            self.drivers.push(driver);
        }
    }

    #[must_use]
    pub fn get(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn EcosystemDriver>> {
        self.drivers.iter().find(|driver| driver.ecosystem() == *ecosystem)
    }

    pub fn present(&self) -> impl Iterator<Item = &Arc<dyn EcosystemDriver>> {
        self.drivers.iter()
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.drivers.is_empty()
    }

    pub fn jobs(&self) -> impl Iterator<Item = (&Ecosystem, &Arc<dyn JobDriver>)> {
        self.jobs.iter()
    }

    pub fn metrics(&self) -> impl Iterator<Item = (&Ecosystem, &Arc<dyn MetricsDriver>)> {
        self.metrics.iter()
    }

    pub fn services(&self) -> impl Iterator<Item = (&Ecosystem, &Arc<dyn ServiceDriver>)> {
        self.services.iter()
    }

    pub fn index_credentials(&self) -> impl Iterator<Item = &Arc<dyn IndexCredentialDriver>> {
        self.index_credentials.values()
    }

    pub fn blob_reference_drivers(&self) -> impl Iterator<Item = &Arc<dyn BlobReferenceDriver>> {
        self.blob_references.values()
    }

    /// # Errors
    /// Returns every uncovered stored ecosystem or the first metadata or driver scan error.
    pub fn scan_blob_references(
        &self,
        meta: &peryx_storage::meta::MetaStore,
    ) -> Result<BlobReferenceScan, BlobReferenceScanError> {
        let stored = meta.repository_ecosystems().map_err(BlobReferenceScanError::Store)?;
        let ecosystems = self
            .blob_references
            .keys()
            .map(|ecosystem| ecosystem.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        let missing = stored.difference(&ecosystems).cloned().collect::<BTreeSet<_>>();
        if !missing.is_empty() {
            return Err(BlobReferenceScanError::MissingDrivers(missing));
        }
        let mut drivers = self.blob_references.iter().collect::<Vec<_>>();
        drivers.sort_unstable_by_key(|(ecosystem, _)| ecosystem.as_str());
        let mut digests = BTreeSet::new();
        for (ecosystem, driver) in drivers {
            digests.extend(
                driver
                    .referenced_blob_digests(meta)
                    .map_err(|reason| BlobReferenceScanError::Driver {
                        ecosystem: ecosystem.clone(),
                        reason,
                    })?,
            );
        }
        Ok(BlobReferenceScan {
            ecosystems: ecosystems.into_iter().collect(),
            digests,
        })
    }

    pub fn trash_drivers(&self) -> impl Iterator<Item = (&Ecosystem, &Arc<dyn TrashDriver>)> {
        self.trash.iter()
    }

    pub fn cache_drivers(&self) -> impl Iterator<Item = &Arc<dyn CacheDriver>> {
        self.cache.values()
    }

    pub fn fsck_drivers(&self) -> impl Iterator<Item = &Arc<dyn FsckDriver>> {
        self.fsck.values()
    }

    #[must_use]
    pub fn get_job(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn JobDriver>> {
        self.jobs.get(ecosystem)
    }
    #[must_use]
    pub fn get_metrics(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn MetricsDriver>> {
        self.metrics.get(ecosystem)
    }
    #[must_use]
    pub fn get_name(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn NameDriver>> {
        self.names.get(ecosystem)
    }
    #[must_use]
    pub fn get_policy(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn PolicyDriver>> {
        self.policies.get(ecosystem)
    }
    #[must_use]
    pub fn get_policy_dry_run(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn PolicyDryRunDriver>> {
        self.policy_dry_runs.get(ecosystem)
    }
    #[must_use]
    pub fn get_cache(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn CacheDriver>> {
        self.cache.get(ecosystem)
    }
    #[must_use]
    pub fn get_retention(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn RetentionDriver>> {
        self.retention.get(ecosystem)
    }
    #[must_use]
    pub fn get_index_summary(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn IndexSummaryDriver>> {
        self.index_summaries.get(ecosystem)
    }
    #[must_use]
    pub fn get_trash(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn TrashDriver>> {
        self.trash.get(ecosystem)
    }
    #[must_use]
    pub fn get_import(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn ImportDriver>> {
        self.imports.get(ecosystem)
    }
    #[must_use]
    pub fn get_browse(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn BrowseDriver>> {
        self.browse.get(ecosystem)
    }
    #[must_use]
    pub fn get_index_credentials(&self, ecosystem: &Ecosystem) -> Option<&Arc<dyn IndexCredentialDriver>> {
        self.index_credentials.get(ecosystem)
    }
}

impl CapabilityRegistrar for DriverSet {
    fn register_job(&mut self, ecosystem: Ecosystem, driver: Arc<dyn JobDriver>) {
        self.jobs.insert(ecosystem, driver);
    }
    fn register_metrics(&mut self, ecosystem: Ecosystem, driver: Arc<dyn MetricsDriver>) {
        self.metrics.insert(ecosystem, driver);
    }
    fn register_name(&mut self, ecosystem: Ecosystem, driver: Arc<dyn NameDriver>) {
        self.names.insert(ecosystem, driver);
    }
    fn register_policy(&mut self, ecosystem: Ecosystem, driver: Arc<dyn PolicyDriver>) {
        self.policies.insert(ecosystem, driver);
    }
    fn register_policy_dry_run(&mut self, ecosystem: Ecosystem, driver: Arc<dyn PolicyDryRunDriver>) {
        self.policy_dry_runs.insert(ecosystem, driver);
    }
    fn register_blob_references(&mut self, ecosystem: Ecosystem, driver: Arc<dyn BlobReferenceDriver>) {
        self.blob_references.insert(ecosystem, driver);
    }
    fn register_fsck(&mut self, ecosystem: Ecosystem, driver: Arc<dyn FsckDriver>) {
        self.fsck.insert(ecosystem, driver);
    }
    fn register_retention(&mut self, ecosystem: Ecosystem, driver: Arc<dyn RetentionDriver>) {
        self.retention.insert(ecosystem, driver);
    }
    fn register_cache(&mut self, ecosystem: Ecosystem, driver: Arc<dyn CacheDriver>) {
        self.cache.insert(ecosystem, driver);
    }
    fn register_index_summary(&mut self, ecosystem: Ecosystem, driver: Arc<dyn IndexSummaryDriver>) {
        self.index_summaries.insert(ecosystem, driver);
    }
    fn register_trash(&mut self, ecosystem: Ecosystem, driver: Arc<dyn TrashDriver>) {
        self.trash.insert(ecosystem, driver);
    }
    fn register_import(&mut self, ecosystem: Ecosystem, driver: Arc<dyn ImportDriver>) {
        self.imports.insert(ecosystem, driver);
    }
    fn register_service(&mut self, ecosystem: Ecosystem, driver: Arc<dyn ServiceDriver>) {
        self.services.insert(ecosystem, driver);
    }
    fn register_browse(&mut self, ecosystem: Ecosystem, driver: Arc<dyn BrowseDriver>) {
        self.browse.insert(ecosystem, driver);
    }
    fn register_index_credentials(&mut self, ecosystem: Ecosystem, driver: Arc<dyn IndexCredentialDriver>) {
        self.index_credentials.insert(ecosystem, driver);
    }
}
