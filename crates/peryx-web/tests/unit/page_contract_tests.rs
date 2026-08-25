use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::Request;
use futures_util::StreamExt as _;
use leptos::prelude::*;
use leptos_router::location::RequestUrl;
use tower::ServiceExt as _;

use crate::App;
use crate::ssr::ui_router;
use peryx_driver::AppState;
use peryx_search::{ContentSource, IndexerCtx, SearchDocument, SearchDocumentProvider, SearchError};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

struct Documents;

impl SearchDocumentProvider for Documents {
    fn documents(&self, _context: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok(vec![SearchDocument {
            display_label: "Artifact A".to_owned(),
            resource_key: "artifact-a".to_owned(),
            route: "root/alpha".to_owned(),
            index: "root/alpha".to_owned(),
            ecosystem: "alpha".to_owned(),
            source: ContentSource::Cached,
            available_locally: true,
            summary: Some("Indexed artifact".to_owned()),
            text: "artifact a".to_owned(),
        }])
    }
}

#[tokio::test]
async fn public_pages_render_their_contracts() {
    for (path, landmark) in [
        ("/", r#"<div class="hero-brand">"#),
        ("/admin/status", r#"<section class="page ops-page">"#),
        ("/admin/topology", "<h1>Availability topology</h1>"),
        ("/admin/placements", "<h1>Artifact placement health</h1>"),
        ("/admin/operations", "<h1>Pending operations</h1>"),
        ("/admin/policy-decisions", "<h1>Policy decisions</h1>"),
        ("/admin/trash", "<h1>Trash</h1>"),
        ("/admin/analytics", r#"<section class="page analytics-page">"#),
        ("/browse", r#"<section class="page browse-page">"#),
        ("/search?q=artifact&page_size=25", "<h1>Search</h1>"),
        ("/stats", r#"class="breadcrumb""#),
        ("/stats?index=root%2Fcache&resource=artifact", "<span>artifact</span>"),
        ("/login", "<h1>Sign in</h1>"),
    ] {
        let html = render(path).await;
        assert!(html.contains(landmark), "{path}: {html}");
    }

    let html = render("/").await;
    for fragment in [
        r"<main>",
        r#"placeholder="Search indexes""#,
        r#"method="get" action="/search""#,
        r#"name="page_size" value="25""#,
        r#"src="/mark.svg" width="24" height="24" alt="peryx logo""#,
        r#"type="button" aria-label="Switch color theme""#,
        r#"href="/admin/topology""#,
        r#"href="https://peryx.readthedocs.io/""#,
        r#"href="https://github.com/tox-dev/peryx""#,
    ] {
        assert!(html.contains(fragment), "{fragment}: {html}");
    }
    assert!(!html.contains("All results"), "{html}");

    let html = render("/search?q=artifact&page_size=25").await;
    for fragment in [
        r#"value="artifact""#,
        r#"href="/search?q=artifact&amp;page_size=25""#,
        "All results",
    ] {
        assert!(html.contains(fragment), "{fragment}: {html}");
    }

    let html = render("/").await;
    for fragment in [
        "<!DOCTYPE html>",
        r#"<meta charset="utf-8">"#,
        r#"href="/favicon.svg""#,
        "localStorage.getItem('theme')",
    ] {
        assert!(html.contains(fragment), "{fragment}: {html}");
    }
}

#[tokio::test]
async fn header_search_renders_indexed_suggestions() {
    let html = render_with_documents("/search?q=artifact&page_size=25").await;

    assert!(html.contains(r#"href="/browse?index=root%2Falpha""#), "{html}");
    assert!(html.contains("Artifact A"), "{html}");
    assert!(html.contains(r#"class="badge source-cached">Cached</span>"#), "{html}");
}

#[tokio::test]
async fn client_router_renders_unknown_paths() {
    let _ = any_spawner::Executor::init_tokio();
    let owner = Owner::new();
    owner.set();
    let (_directory, app) = state(false);
    provide_context(app);
    provide_context(RequestUrl::new("/missing"));

    let html = App().to_html_stream_in_order().collect::<String>().await;

    assert!(html.contains(r#"<p class="dim">not found</p>"#), "{html}");
}

async fn render(path: &str) -> String {
    render_document(path, false).await
}

async fn render_with_documents(path: &str) -> String {
    render_document(path, true).await
}

async fn render_document(path: &str, documents: bool) -> String {
    let (_directory, app) = state(documents);
    let response = ui_router(app)
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    String::from_utf8(to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap()
}

fn state(documents: bool) -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let mut app = AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        Vec::new(),
    );
    if documents {
        Arc::get_mut(&mut app.serving)
            .unwrap()
            .search
            .add_indexer(Arc::new(Documents));
    }
    (directory, Arc::new(app))
}
