use peryx_driver::discovery::BaseUrl;
use peryx_driver::serving::{EcosystemCapability, EcosystemPlugin as _};

use super::PypiPlugin;

#[test]
fn plugin_exposes_capabilities_and_validates_snippet_formats() {
    let plugin = PypiPlugin;
    let base = BaseUrl::parse("https://packages.example/").unwrap();

    assert!(plugin.supports(EcosystemCapability::CatalogSync));
    assert!(plugin.supports(EcosystemCapability::TrustedPublishing));
    assert!(plugin.snippet_text(&base, "pypi", true, "pip.conf").unwrap().is_some());
    assert!(plugin.snippet_text(&base, "pypi", true, "unknown").is_err());
}
