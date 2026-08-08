use peryx_driver::discovery::BaseUrl;
use peryx_driver::serving::{EcosystemCapability, EcosystemPlugin};
use peryx_ecosystem_pypi::{ECOSYSTEM, PypiPlugin};

#[test]
fn plugin_reports_identity_capabilities_and_rejects_unknown_snippets() {
    let plugin = PypiPlugin;

    assert_eq!(plugin.ecosystem(), ECOSYSTEM);
    assert!(plugin.supports(EcosystemCapability::CatalogSync));
    assert!(plugin.supports(EcosystemCapability::TrustedPublishing));
    assert!(
        plugin
            .snippet_text(
                &BaseUrl::parse("https://packages.example/").unwrap(),
                "pypi",
                true,
                "unknown"
            )
            .is_err()
    );
}
