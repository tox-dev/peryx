use peryx_driver::discovery::BaseUrl;
use peryx_driver::serving::{EcosystemCapability, EcosystemPlugin};
use peryx_ecosystem_oci::{ECOSYSTEM, OciPlugin};

#[test]
fn plugin_reports_identity_capabilities_and_snippet_failure() {
    let plugin = OciPlugin;

    assert_eq!(plugin.ecosystem(), ECOSYSTEM);
    assert!(!plugin.supports(EcosystemCapability::CatalogSync));
    assert!(!plugin.supports(EcosystemCapability::TrustedPublishing));
    assert_eq!(
        plugin
            .snippet_text(
                &BaseUrl::parse("https://registry.example/").unwrap(),
                "oci",
                false,
                "docker"
            )
            .unwrap_err(),
        "OCI does not provide client snippet \"docker\""
    );
}
