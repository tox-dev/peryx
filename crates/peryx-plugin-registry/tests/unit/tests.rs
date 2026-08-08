use std::collections::HashMap;
use std::sync::Arc;

use peryx_core::{DefaultIndex, DefaultIndexKind, Ecosystem};
use peryx_driver::AppState;
use peryx_driver::discovery::BaseUrl;
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::serving::{CompiledEcosystemSettings, EcosystemCapability, EcosystemDriver, EcosystemPlugin};
use peryx_driver::state::IndexDescription;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use utoipa::openapi::PathsBuilder;

use super::*;

const PYPI: Ecosystem = Ecosystem::new("pypi");
const OCI: Ecosystem = Ecosystem::new("oci");

struct TestDriver(Ecosystem);

#[async_trait::async_trait]
impl EcosystemDriver for TestDriver {
    fn ecosystem(&self) -> Ecosystem {
        self.0
    }

    fn classify_route(&self, _: &str) -> RouteClass {
        RouteClass::Artifact
    }

    fn discover_index(&self, _: IndexDescription, _: Option<&BaseUrl>) -> serde_json::Value {
        serde_json::Value::Null
    }
}

struct TestPlugin(Ecosystem);

const DEFAULT_INDEXES: &[DefaultIndex] = &[DefaultIndex {
    name: "test",
    route: "test",
    ecosystem: PYPI,
    kind: DefaultIndexKind::Hosted,
}];

impl EcosystemPlugin for TestPlugin {
    fn ecosystem(&self) -> Ecosystem {
        self.0
    }

    fn default_indexes(&self) -> &'static [DefaultIndex] {
        DEFAULT_INDEXES
    }

    fn driver(&self) -> Arc<dyn EcosystemDriver> {
        Arc::new(TestDriver(self.0))
    }

    fn compile_index_settings(&self, name: &str, _: &toml::Table) -> Result<Option<CompiledEcosystemSettings>, String> {
        if name == "invalid" {
            Err("invalid settings".into())
        } else {
            Ok(Some(CompiledEcosystemSettings::new(self.0, name.to_owned())))
        }
    }

    fn install(&self, _: &mut AppState, settings: &[(&str, &CompiledEcosystemSettings)]) -> Result<(), String> {
        if settings.iter().any(|(name, _)| *name == "fail") {
            Err("local install failed".into())
        } else {
            Ok(())
        }
    }

    fn install_distributed(
        &self,
        _: &mut AppState,
        settings: &[(&str, &CompiledEcosystemSettings)],
    ) -> Result<(), String> {
        if settings.iter().any(|(name, _)| *name == "fail") {
            Err("distributed install failed".into())
        } else {
            Ok(())
        }
    }

    fn supports(&self, capability: EcosystemCapability) -> bool {
        capability == EcosystemCapability::TrustedPublishing
    }

    fn openapi_paths(&self, paths: PathsBuilder) -> PathsBuilder {
        paths
    }

    fn snippet_text(&self, _: &BaseUrl, route: &str, uploads: bool, format: &str) -> Result<Option<String>, String> {
        if format == "invalid" {
            Err("invalid format".into())
        } else {
            Ok(Some(format!("{route}:{uploads}:{format}")))
        }
    }
}

static PYPI_PLUGIN: TestPlugin = TestPlugin(PYPI);
static OCI_PLUGIN: TestPlugin = TestPlugin(OCI);

inventory::submit! {
    PluginRegistration {
        plugin: &PYPI_PLUGIN,
        priority: 10,
    }
}

fn state() -> (tempfile::TempDir, AppState) {
    let directory = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(directory.path().join("blobs"));
    (directory, AppState::new(meta, blobs, 60, Vec::new()))
}

#[test]
fn registrations_are_sorted_by_priority() {
    let late = PluginRegistration {
        plugin: &OCI_PLUGIN,
        priority: 2,
    };
    let early = PluginRegistration {
        plugin: &PYPI_PLUGIN,
        priority: 1,
    };
    let plugins = ordered_plugins(vec![&late, &early]);
    assert_eq!(
        plugins.iter().map(|plugin| plugin.ecosystem()).collect::<Vec<_>>(),
        [PYPI, OCI]
    );
}

#[test]
#[should_panic(expected = "the binary must link at least one ecosystem plugin")]
fn empty_registration_set_is_rejected() {
    ordered_plugins(Vec::new());
}

#[test]
#[should_panic(expected = "duplicate ecosystem plugin")]
fn duplicate_ecosystems_are_rejected() {
    let first = PluginRegistration {
        plugin: &PYPI_PLUGIN,
        priority: 1,
    };
    let second = PluginRegistration {
        plugin: &PYPI_PLUGIN,
        priority: 2,
    };
    ordered_plugins(vec![&first, &second]);
}

#[test]
#[should_panic(expected = "duplicate plugin priority")]
fn duplicate_priorities_are_rejected() {
    let first = PluginRegistration {
        plugin: &PYPI_PLUGIN,
        priority: 1,
    };
    let second = PluginRegistration {
        plugin: &OCI_PLUGIN,
        priority: 1,
    };
    ordered_plugins(vec![&first, &second]);
}

#[test]
fn inventory_queries_expose_the_linked_plugin() {
    assert_eq!(default_ecosystem(), PYPI);
    assert!(is_installed(PYPI));
    assert!(!is_installed(OCI));
    assert_eq!(default_indexes().copied().collect::<Vec<_>>(), DEFAULT_INDEXES);
    assert_eq!(drivers().get(PYPI).unwrap().ecosystem(), PYPI);
    assert!(drivers().get(OCI).is_none());
    assert!(supports(PYPI, EcosystemCapability::TrustedPublishing));
    assert!(!supports(PYPI, EcosystemCapability::CatalogSync));
    assert!(!supports(OCI, EcosystemCapability::TrustedPublishing));
    drop(openapi_paths(PathsBuilder::new()).build());
}

#[test]
fn index_settings_dispatch_to_the_owning_plugin() {
    let compiled = compile_index_settings(PYPI, "test", &toml::Table::new())
        .unwrap()
        .unwrap();
    assert_eq!(compiled.ecosystem(), PYPI);
    assert_eq!(compiled.value::<String>().unwrap(), "test");
    assert_eq!(
        compile_index_settings(PYPI, "invalid", &toml::Table::new()).unwrap_err(),
        "invalid settings"
    );
    assert_eq!(
        compile_index_settings(OCI, "test", &toml::Table::new()).unwrap_err(),
        "ecosystem oci is not installed"
    );
}

#[test]
fn local_and_distributed_installation_are_distinct() {
    let (_directory, mut state) = state();
    assert!(install_drivers(&mut state, &HashMap::new()).is_ok());
    assert!(install_distributed_drivers(&mut state, &HashMap::new()).is_ok());

    let settings = HashMap::from([("fail".to_owned(), CompiledEcosystemSettings::new(PYPI, ()))]);
    assert_eq!(
        install_drivers(&mut state, &settings).unwrap_err(),
        "local install failed"
    );
    assert_eq!(
        install_distributed_drivers(&mut state, &settings).unwrap_err(),
        "distributed install failed"
    );
}

#[test]
fn snippets_dispatch_or_report_missing_plugins() {
    let base = BaseUrl::parse("https://packages.example").unwrap();
    assert_eq!(
        snippet_text(PYPI, &base, "hosted", true, "test").unwrap().unwrap(),
        "hosted:true:test"
    );
    assert_eq!(
        snippet_text(PYPI, &base, "hosted", false, "invalid").unwrap_err(),
        "invalid format"
    );
    assert_eq!(
        snippet_text(OCI, &base, "hosted", false, "test").unwrap_err(),
        "ecosystem oci is not installed"
    );
}
