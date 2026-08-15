use std::num::NonZeroU16;

use peryx_ecosystem_oci_fuzz::{MAX_MANIFEST_BYTES, ManifestAdapter, fuzz_manifest, fuzz_reference};

#[test]
fn manifest_adapter_smoke_uses_the_global_harness() {
    assert!(fuzz_manifest(b"{}").is_some());
}

#[test]
fn manifest_adapter_property_bounds_inputs_without_advancing_state() {
    let mut adapter = ManifestAdapter::new(NonZeroU16::new(2).unwrap());
    assert_eq!(adapter.run(&vec![0; MAX_MANIFEST_BYTES + 1]), None);
    assert_eq!(adapter.run(&[]), Some(1));
}

#[test]
fn manifest_adapter_property_resets_at_its_configured_interval() {
    let mut adapter = ManifestAdapter::new(NonZeroU16::new(2).unwrap());
    assert_eq!(adapter.run(b"{}"), Some(1));
    assert_eq!(adapter.run(b"{}"), Some(0));
    assert_eq!(adapter.run(b"{}"), Some(1));
}

#[test]
fn manifest_adapter_property_accepts_every_bounded_shape() {
    let mut adapter = ManifestAdapter::default();
    for data in [Vec::new(), vec![0], vec![0; MAX_MANIFEST_BYTES]] {
        assert!(adapter.run(&data).is_some());
    }
}

#[test]
fn reference_adapter_smoke_classifies_a_reference() {
    assert!(fuzz_reference(b"latest"));
}

#[test]
fn reference_adapter_property_matches_the_utf8_domain() {
    for byte in u8::MIN..=u8::MAX {
        let data = [byte];
        assert_eq!(fuzz_reference(&data), std::str::from_utf8(&data).is_ok());
    }
}
