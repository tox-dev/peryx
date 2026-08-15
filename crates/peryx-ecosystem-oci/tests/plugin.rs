use peryx_driver::serving::EcosystemRegistration as _;
use peryx_ecosystem_oci::{ECOSYSTEM, OciPlugin, registration};

#[test]
fn plugin_reports_identity_and_absent_snippet_capability() {
    let plugin = OciPlugin;

    assert_eq!(plugin.ecosystem(), ECOSYSTEM);
    let protocol = plugin.driver();
    let driver = protocol.absolute().unwrap();
    assert_eq!(driver.prefixes(), &["/v2/"]);
    assert!(registration().snippets.is_none());
}
