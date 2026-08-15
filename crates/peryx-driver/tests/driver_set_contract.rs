use std::sync::Arc;

use async_trait::async_trait;
use peryx_core::Ecosystem;
use peryx_driver::DriverSet;
use peryx_driver::serving::{
    BlobReferenceDriver, BrowseDriver, CacheDriver, CapabilityRegistrar, EcosystemDriver, FsckDriver, ImportDriver,
    JobConfig, JobDriver, MetricsDriver, NameDriver, RetentionDriver, TrashDriver,
};
use peryx_driver::serving::{CachePage, PurgeReport};

struct Driver {
    ecosystem: Ecosystem,
}

impl EcosystemDriver for Driver {
    fn ecosystem(&self) -> Ecosystem {
        self.ecosystem.clone()
    }
}

impl JobDriver for Driver {
    fn compile_job(&self, _config: JobConfig<'_>) -> Option<Result<peryx_driver::jobs::PluginScheduledJob, String>> {
        None
    }
}

impl MetricsDriver for Driver {
    fn metric_families(&self) -> &'static [peryx_events::metrics::MetricFamily] {
        &[]
    }
}

impl NameDriver for Driver {
    fn normalize_name(&self, name: &str) -> String {
        name.to_uppercase()
    }
}

impl BlobReferenceDriver for Driver {
    fn referenced_blob_digests(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
    ) -> Result<std::collections::BTreeSet<String>, String> {
        Err("blob references".to_owned())
    }
}

impl FsckDriver for Driver {
    fn fsck_metadata(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _blobs: &peryx_storage::blob::BlobStorage,
        _out: &mut dyn std::io::Write,
    ) -> Result<u64, String> {
        Err("fsck".to_owned())
    }
}

impl RetentionDriver for Driver {
    fn plan_retention(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _index: &str,
        _policy: &peryx_policy::RetentionPolicy,
        _now: Option<i64>,
        emit: &mut dyn FnMut(peryx_policy::RetentionDecision) -> Result<(), String>,
    ) -> Result<peryx_policy::RetentionSummary, String> {
        emit(peryx_policy::RetentionDecision {
            resource: "resource".to_owned(),
            group: None,
            artifact: "artifact".to_owned(),
            digest: "digest".to_owned(),
            class: peryx_policy::RetentionClass::Hosted,
            visibility: peryx_policy::RetentionVisibility::Active,
            source: None,
            bytes: 1,
            outcome: peryx_policy::RetentionOutcome::Retain,
            rule: None,
            retained_groups: Vec::new(),
        })?;
        Err("retention".to_owned())
    }
}

impl CacheDriver for Driver {
    fn purge_resource(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _index: &str,
        _resource: &str,
        _apply: bool,
    ) -> Result<PurgeReport, String> {
        Err("purge".to_owned())
    }

    fn cache_pages(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _index_names: &[&str],
    ) -> Result<Vec<CachePage>, String> {
        Err("pages".to_owned())
    }

    fn cache_record_counts(&self, _meta: &peryx_storage::meta::MetaStore) -> Result<Vec<(String, u64)>, String> {
        Err("counts".to_owned())
    }
}

impl TrashDriver for Driver {
    fn trash_records(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _index_names: &[String],
    ) -> Result<Vec<peryx_core::TrashRecord>, String> {
        Err("trash".to_owned())
    }
}

impl ImportDriver for Driver {
    fn import_dir(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _blobs: &peryx_storage::blob::BlobStorage,
        _target_name: &str,
        _target_route: &str,
        _dir: &std::path::Path,
        _out: &mut dyn std::io::Write,
    ) -> Result<(), String> {
        Err("import".to_owned())
    }
}

#[async_trait]
impl BrowseDriver for Driver {
    async fn browse(
        &self,
        _state: Arc<peryx_driver::ServingState>,
        _position: usize,
        _raw_query: String,
    ) -> Result<Option<peryx_core::BrowsePage>, String> {
        Err("browse".to_owned())
    }
}

#[test]
fn driver_set_preserves_registration_order_when_replacing_a_driver() {
    let first_ecosystem = Ecosystem::new("first");
    let first: Arc<dyn EcosystemDriver> = Arc::new(Driver {
        ecosystem: first_ecosystem.clone(),
    });
    let second: Arc<dyn EcosystemDriver> = Arc::new(Driver {
        ecosystem: Ecosystem::new("second"),
    });
    let replacement: Arc<dyn EcosystemDriver> = Arc::new(Driver {
        ecosystem: first_ecosystem.clone(),
    });
    let set = DriverSet::default().with(first).with(second).with(replacement.clone());

    assert_eq!(
        set.present().map(|driver| driver.ecosystem()).collect::<Vec<_>>(),
        [first_ecosystem.clone(), Ecosystem::new("second")]
    );
    assert!(Arc::ptr_eq(set.get(&first_ecosystem).unwrap(), &replacement));
    assert!(set.get_index_summary(&first_ecosystem).is_none());
    assert!(set.get(&Ecosystem::new("missing")).is_none());
}

#[test]
fn driver_set_registers_and_dispatches_independent_capabilities() {
    let ecosystem = Ecosystem::new("example");
    let driver = Arc::new(Driver {
        ecosystem: ecosystem.clone(),
    });
    let mut set = DriverSet::default();
    set.register_job(ecosystem.clone(), driver.clone());
    set.register_metrics(ecosystem.clone(), driver.clone());
    set.register_name(ecosystem.clone(), driver.clone());
    set.register_blob_references(ecosystem.clone(), driver.clone());
    set.register_fsck(ecosystem.clone(), driver.clone());
    set.register_retention(ecosystem.clone(), driver.clone());
    set.register_cache(ecosystem.clone(), driver.clone());
    set.register_trash(ecosystem.clone(), driver.clone());
    set.register_import(ecosystem.clone(), driver);
    let directory = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStorage::filesystem(directory.path().join("blobs"));
    assert_eq!(set.jobs().count(), 1);
    assert_eq!(set.metrics().count(), 1);
    assert_eq!(set.blob_reference_drivers().count(), 1);
    assert_eq!(set.trash_drivers().count(), 1);
    assert_eq!(set.cache_drivers().count(), 1);
    assert_eq!(set.fsck_drivers().count(), 1);
    assert!(
        set.get_job(&ecosystem)
            .unwrap()
            .compile_job(JobConfig {
                kind: "none",
                settings: &toml::Table::new(),
                indexes: &[],
            })
            .is_none()
    );
    assert!(set.get_metrics(&ecosystem).unwrap().metric_families().is_empty());
    assert_eq!(set.get_name(&ecosystem).unwrap().normalize_name("catalog"), "CATALOG");
    assert_eq!(
        set.blob_reference_drivers()
            .next()
            .unwrap()
            .referenced_blob_digests(&meta),
        Err("blob references".to_owned())
    );
    assert_eq!(
        set.fsck_drivers()
            .next()
            .unwrap()
            .fsck_metadata(&meta, &blobs, &mut Vec::new()),
        Err("fsck".to_owned())
    );
    let mut decisions = Vec::new();
    let result = set.get_retention(&ecosystem).unwrap().plan_retention(
        &meta,
        "catalog",
        &peryx_policy::RetentionPolicy::compile(&peryx_policy::RetentionConfig::default()),
        None,
        &mut |decision| {
            decisions.push(decision);
            Ok(())
        },
    );
    assert_eq!(result, Err("retention".to_owned()));
    assert_eq!((decisions.len(), decisions[0].artifact.as_str()), (1, "artifact"));
    let cache = set.get_cache(&ecosystem).unwrap();
    assert_eq!(
        cache.purge_resource(&meta, "catalog", "resource", false),
        Err("purge".to_owned())
    );
    assert_eq!(cache.cache_pages(&meta, &["catalog"]), Err("pages".to_owned()));
    assert_eq!(cache.cache_record_counts(&meta), Err("counts".to_owned()));
    assert_eq!(
        set.trash_drivers()
            .next()
            .unwrap()
            .1
            .trash_records(&meta, &["catalog".to_owned()]),
        Err("trash".to_owned())
    );
    assert_eq!(
        set.get_import(&ecosystem).unwrap().import_dir(
            &meta,
            &blobs,
            "catalog",
            "catalog",
            directory.path(),
            &mut Vec::new(),
        ),
        Err("import".to_owned())
    );
}

#[tokio::test]
async fn driver_set_dispatches_browse_capability() {
    let ecosystem = Ecosystem::new("example");
    let mut set = DriverSet::default();
    set.register_browse(
        ecosystem.clone(),
        Arc::new(Driver {
            ecosystem: ecosystem.clone(),
        }),
    );
    let directory = tempfile::tempdir().unwrap();
    let state = Arc::new(peryx_driver::AppState::new(
        peryx_storage::meta::MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        peryx_storage::blob::BlobStore::new(directory.path().join("blobs")),
        60,
        Vec::new(),
    ));

    assert_eq!(
        set.get_browse(&ecosystem)
            .unwrap()
            .browse(state.serving.clone(), 0, String::new())
            .await,
        Err("browse".to_owned())
    );
}
