#[cfg(feature = "ssr")]
#[tokio::test]
async fn topology_loader_reads_ssr_state() {
    use std::sync::Arc;

    use leptos::prelude::*;
    use peryx_core::{LocalNode, NodeRole, TopologyMode, TopologySnapshot};
    use peryx_driver::AppState;
    use peryx_storage::blob::BlobStore;
    use peryx_storage::meta::MetaStore;

    let directory = tempfile::tempdir().unwrap();
    let owner = Owner::new();
    owner.set();
    provide_context(Arc::new(AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStore::new(directory.path().join("blobs")),
        60,
        Vec::new(),
    )));

    let topology = super::load_topology().await.unwrap();
    assert_eq!(
        topology,
        TopologySnapshot {
            mode: TopologyMode::None,
            group: None,
            captured_at: topology.captured_at,
            node_count: 0,
            local: LocalNode {
                role: NodeRole::Writer,
                liveness: None,
                frontier: None,
            },
            nodes: Vec::new(),
        }
    );
}

#[cfg(all(not(feature = "ssr"), not(feature = "hydrate")))]
#[tokio::test]
async fn topology_loader_reports_missing_runtime() {
    assert_eq!(
        super::load_topology().await,
        Err("The availability topology is unavailable.".to_owned())
    );
}
