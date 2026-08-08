use peryx_driver::serving::MaintenanceDriver as _;

use super::PypiServing;

#[test]
fn serving_exposes_pypi_maintenance() {
    let serving = PypiServing;
    let capabilities = serving.maintenance_capabilities();

    assert_eq!(serving.ecosystem(), crate::ECOSYSTEM);
    assert!(capabilities.intent_finalizer.is_some());
    assert!(capabilities.cache_refresher.is_some());
    assert!(capabilities.idle_reclaimer.is_none());
}
