use std::sync::Arc;

use peryx_core::Ecosystem;
use peryx_driver::PolicyDryRunDriver;
use peryx_driver::serving::{CapabilityRegistrar, CompiledEcosystemSettings, EcosystemDriver, PolicyDriver};

struct PolicyCapabilities;

impl EcosystemDriver for PolicyCapabilities {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }
}

impl PolicyDriver for PolicyCapabilities {
    fn compile_policy(&self, _policy: &toml::Table) -> Result<peryx_policy::PolicyCapabilities, String> {
        Ok(peryx_policy::PolicyCapabilities::default())
    }
}

impl PolicyDryRunDriver for PolicyCapabilities {
    fn policy_dry_run(
        &self,
        _meta: &peryx_storage::meta::MetaStore,
        _indexes: &[peryx_index::Index],
        _index_filter: Option<&str>,
        _resource_filter: Option<&str>,
        out: &mut dyn std::io::Write,
    ) -> Result<(), String> {
        out.write_all(b"dry-run").map_err(|error| error.to_string())
    }
}

#[test]
fn compiled_settings_expose_only_their_ecosystem_and_typed_value() {
    let ecosystem = Ecosystem::new("example");
    let settings = CompiledEcosystemSettings::new(ecosystem.clone(), 42_u64);

    assert_eq!(settings.ecosystem(), ecosystem);
    assert_eq!(settings.value::<u64>(), Some(&42));
    assert_eq!(settings.value::<String>(), None);
    assert_eq!(
        format!("{settings:?}"),
        "CompiledEcosystemSettings { ecosystem: Ecosystem(Static(\"example\")), .. }"
    );
}

#[test]
fn policy_capabilities_register_independently() {
    let mut drivers = peryx_driver::DriverSet::default();
    let driver = Arc::new(PolicyCapabilities);
    let ecosystem = Ecosystem::new("example");
    assert_eq!(driver.ecosystem(), ecosystem);
    drivers.register_policy(ecosystem.clone(), driver.clone());

    assert!(
        drivers
            .get_policy(&ecosystem)
            .unwrap()
            .compile_policy(&toml::Table::new())
            .unwrap()
            .is_empty()
    );
    assert!(drivers.get_policy_dry_run(&ecosystem).is_none());

    drivers.register_policy_dry_run(ecosystem.clone(), driver);
    let directory = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    let mut output = Vec::new();
    drivers
        .get_policy_dry_run(&ecosystem)
        .unwrap()
        .policy_dry_run(&meta, &[], None, None, &mut output)
        .unwrap();
    assert_eq!(output, b"dry-run");
}
