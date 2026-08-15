use peryx_ha::{ReplicaPage, ReplicaViewApplier as _};

use super::hosted_writable;

#[test]
fn replicated_registry_keys_do_not_block_the_shared_frontier() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _router) = hosted_writable(&dir, "secret");
    let key = crate::store::blob_membership_key("store", "app", "sha256:fixture");
    state.serving.meta.put_driver_value(&key, b"present").unwrap();

    state.apply(
        ReplicaPage {
            changes: 1,
            serial: 1,
            primary_serial: 1,
        },
        &[key],
    );

    assert_eq!(
        state
            .serving
            .meta
            .view_frontier(peryx_driver::state::SEARCH_VIEW)
            .unwrap(),
        Some(1)
    );
}
