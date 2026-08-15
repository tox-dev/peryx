#![cfg(feature = "serving")]

use peryx_driver::discovery::BaseUrl;
use peryx_driver::serving::{EcosystemRegistration as _, EcosystemSnippet as _};
use peryx_ecosystem_pypi::{ECOSYSTEM, PypiPlugin, registration};

#[test]
fn plugin_reports_identity_capabilities_and_rejects_unknown_snippets() {
    let plugin = PypiPlugin;
    let protocol = plugin.driver();

    assert_eq!(plugin.ecosystem(), ECOSYSTEM);
    assert!(protocol.indexed().is_some());
    let registry = peryx_plugin_registry::PluginRegistry::new(vec![registration()])
        .unwrap()
        .activate([ECOSYSTEM])
        .unwrap();
    assert!(registry.drivers().get_job(&ECOSYSTEM).is_some());
    assert!(registry.drivers().get_policy(&ECOSYSTEM).is_some());
    assert!(registry.drivers().get_policy_dry_run(&ECOSYSTEM).is_some());
    assert!(
        plugin
            .text(
                &BaseUrl::parse("https://packages.example/").unwrap(),
                "pypi",
                true,
                "unknown"
            )
            .is_err()
    );
}

#[test]
fn registration_builds_the_driver_only_after_activation() {
    let registry = peryx_plugin_registry::PluginRegistry::new(vec![registration()]).unwrap();

    assert!(registry.protocol(&ECOSYSTEM).is_none());
    assert!(registry.activate([ECOSYSTEM]).unwrap().protocol(&ECOSYSTEM).is_some());
}
