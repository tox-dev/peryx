use super::*;
use crate::config::{DcMember, DcMembership, DcRole};
use peryx_driver::state::AppState;
use peryx_storage::meta::MetaStore;

fn member(node: &str, dc: &str) -> DcMember {
    DcMember {
        node: node.to_owned(),
        dc: dc.to_owned(),
        address: format!("http://{node}/"),
        role: DcRole::Writer,
    }
}

fn store() -> (tempfile::TempDir, BlobStore) {
    let dir = tempfile::tempdir().unwrap();
    let storage = peryx_storage::blob::BlobStorage::filesystem(dir.path().join("blobs"));
    (dir, storage.filesystem_store().unwrap().clone())
}

fn state() -> (tempfile::TempDir, tempfile::TempDir, Arc<AppState>) {
    let meta_dir = tempfile::tempdir().unwrap();
    let blob_dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(meta_dir.path().join("peryx.redb")).unwrap();
    let state = Arc::new(AppState::new(
        meta,
        peryx_storage::blob::BlobStorage::filesystem(blob_dir.path().join("blobs")),
        60,
        Vec::new(),
    ));
    (meta_dir, blob_dir, state)
}

#[test]
fn multi_datacenter_membership_builds_a_reconciler() {
    let config = Config {
        node_identity: Some("local".to_owned()),
        dc_membership: Some(DcMembership {
            group: "group".to_owned(),
            members: vec![member("local", "home"), member("peer", "east")],
        }),
        ..Config::default()
    };
    let (_dir, store) = store();

    let reconciler = FilesystemPlacementReconciler::from_config(&config, store)
        .unwrap()
        .unwrap();
    let (_meta_dir, _blob_dir, state) = state();

    drop(reconciler.bind(state.serving.clone()));
}

#[test]
fn unrostered_node_has_no_reconciler() {
    let config = Config {
        node_identity: Some("local".to_owned()),
        dc_membership: Some(DcMembership {
            group: "group".to_owned(),
            members: vec![member("peer", "east")],
        }),
        ..Config::default()
    };
    let (_dir, store) = store();

    assert!(
        FilesystemPlacementReconciler::from_config(&config, store)
            .unwrap()
            .is_none()
    );
}

#[test]
fn missing_membership_has_no_reconciler() {
    let (_dir, store) = store();

    assert!(
        FilesystemPlacementReconciler::from_config(&Config::default(), store)
            .unwrap()
            .is_none()
    );
}

#[test]
fn invalid_datacenter_is_rejected() {
    let config = Config {
        node_identity: Some("local".to_owned()),
        dc_membership: Some(DcMembership {
            group: "group".to_owned(),
            members: vec![member("local", "home"), member("peer", &"d".repeat(600))],
        }),
        ..Config::default()
    };
    let (_dir, store) = store();

    assert!(FilesystemPlacementReconciler::from_config(&config, store).is_err());
}
