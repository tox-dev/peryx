use peryx_driver::serving::MaintenanceDriver as _;

use super::OciRegistry;

#[test]
fn registry_exposes_oci_idle_reclamation() {
    let registry = OciRegistry::default();

    assert_eq!(registry.ecosystem(), crate::ECOSYSTEM);
    assert!(registry.maintenance_capabilities().idle_reclaimer.is_some());
}
