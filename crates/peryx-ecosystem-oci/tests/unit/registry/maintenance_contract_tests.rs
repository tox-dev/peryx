use peryx_driver::AppState;
use peryx_driver::serving::{BlobReferenceDriver as _, TrashDriver as _};
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::OciRegistry;

#[test]
fn registry_exposes_oci_idle_reclamation() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        vec![Index {
            name: "images".to_owned(),
            route: "images".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    peryx_plugin_registry::PluginRegistry::new(vec![crate::registration()])
        .unwrap()
        .activate([crate::ECOSYSTEM])
        .unwrap()
        .install_drivers(
            &mut state.runtime_install_context().unwrap(),
            &std::collections::HashMap::new(),
        )
        .unwrap();

    assert_eq!(
        state
            .idle_reclaimers()
            .map(|(ecosystem, _)| ecosystem.clone())
            .collect::<Vec<_>>(),
        vec![crate::ECOSYSTEM]
    );
    assert_eq!(state.intent_finalizers().count(), 0);
    assert_eq!(state.cache_refreshers().count(), 0);
}

#[test]
fn registry_exposes_storage_contracts() {
    let registry = OciRegistry::default();
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    assert!(registry.referenced_blob_digests(&meta).unwrap().is_empty());
    assert!(
        registry
            .trash_records(&meta, &["private".to_owned()])
            .unwrap()
            .is_empty()
    );
}
