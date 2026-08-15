use std::sync::Arc;

use leptos::prelude::*;
use peryx_core::{LocalNode, NodeRole, TopologyMode, TopologySnapshot};
use peryx_driver::AppState;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

fn state() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, Arc::new(AppState::new(meta, blobs, 60, Vec::new())))
}

#[tokio::test]
async fn missing_index_fails_browse() {
    let (_dir, app) = state();
    let owner = Owner::new();
    owner.set();
    provide_context(app);

    assert_eq!(
        super::browse("index=missing&opaque=segment%2Fvalue%3Adetail").await,
        Err("index \"missing\" is not configured".to_owned())
    );
}

#[tokio::test]
async fn public_server_views_project_empty_local_state() {
    let (_dir, app) = state();
    let owner = Owner::new();
    owner.set();
    provide_context(app);

    assert_eq!(super::login_state().await, crate::model::UiLoginState::default());
    assert_eq!(
        super::snapshot().await,
        crate::model::UiSnapshot {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            ..Default::default()
        }
    );
    assert_eq!(
        super::admin_snapshot().await,
        crate::model::UiSnapshot {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            ..Default::default()
        }
    );
    assert_eq!(super::stats(None, None).await, serde_json::json!({}));
    assert_eq!(
        super::search("", "invalid", "invalid", 0, 7).await,
        Ok(crate::model::UiSearchPage {
            source_type: "all".to_owned(),
            availability: "all".to_owned(),
            page: 1,
            page_size: 25,
            ..Default::default()
        })
    );
    let topology = super::topology().await;
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
    assert_eq!(
        super::operations().await,
        Err("You do not have access to operation health.".to_owned())
    );
    assert_eq!(
        super::placements().await,
        Err("You do not have access to placement health.".to_owned())
    );
    assert_eq!(
        super::blob_placements("invalid".to_owned()).await,
        Err("You do not have access to blob placement.".to_owned())
    );
}
