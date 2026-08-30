use std::sync::Arc;

use peryx_core::BrowseBadge;
use peryx_driver::AppState;
use peryx_driver::serving::{BrowseDriver as _, BrowseRequest};
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;

use super::PypiServing;
use crate::store::{CachedIndex, PypiStore as _};

#[tokio::test]
async fn project_commands_quote_the_served_display_name() {
    for (display, versions, expected) in [
        (
            "flask",
            Vec::new(),
            "uv pip install --index-url <origin>/root/packages/simple/ flask",
        ),
        (
            "flask[async]",
            vec!["1.2.3"],
            "uv pip install --index-url <origin>/root/packages/simple/ 'flask[async]==1.2.3'",
        ),
        (
            "o'hara",
            vec!["1.2.3"],
            r"uv pip install --index-url <origin>/root/packages/simple/ 'o'\''hara==1.2.3'",
        ),
    ] {
        let (_directory, state) = cached_project(display, &versions);
        let access = peryx_driver::access::ReadAccess::from_headers(&state.serving, &axum::http::HeaderMap::new());
        let page = PypiServing
            .browse(BrowseRequest {
                state: state.serving.clone(),
                position: 0,
                raw_query: "index=root%2Fpackages&project=demo".to_owned(),
                access: &access,
                base: None,
            })
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            (page.command, page.badges),
            (
                Some(expected.to_owned()),
                vec![BrowseBadge {
                    label: "archived".to_owned(),
                    class: "status-archived".to_owned(),
                    hint: Some("read only".to_owned()),
                }]
            ),
            "{display}"
        );
    }
}

fn cached_project(display: &str, versions: &[&str]) -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        vec![Index {
            name: "cached".to_owned(),
            route: "root/packages".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: UpstreamClient::new("https://example.invalid/simple/").unwrap(),
                offline: true,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    crate::tests::install(&mut state);
    state
        .serving
        .meta
        .put_index(
            "cached/demo",
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 0,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: None,
                body: serde_json::to_vec(&serde_json::json!({
                    "meta": {
                        "api-version": "1.4",
                    },
                    "project-status": {"status": "archived", "reason": "read only"},
                    "name": display,
                    "versions": versions,
                    "files": [],
                }))
                .unwrap(),
            },
        )
        .unwrap();
    (directory, Arc::new(state))
}
