use crate::{CoreMetadata, Yanked};

#[test]
fn test_simple_field_deserializers_reject_invalid_types() {
    assert!(serde_json::from_str::<Yanked>("123").is_err());
    assert!(serde_json::from_str::<CoreMetadata>("123").is_err());
}
