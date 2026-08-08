use peryx_storage::meta::MetaError;

use super::FinalizeError;

#[test]
fn test_a_store_fault_maps_to_a_finalize_store_error() {
    let err = MetaError::Decode(serde_json::from_str::<serde_json::Value>("x").unwrap_err());
    assert!(matches!(FinalizeError::from(err), FinalizeError::Store(_)));
}
