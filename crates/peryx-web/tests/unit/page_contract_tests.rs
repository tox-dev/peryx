use std::sync::Arc;

use futures_util::StreamExt as _;
use leptos::prelude::*;
use leptos_router::location::RequestUrl;

use crate::{App, shell};
use peryx_driver::AppState;
use peryx_search::{ContentSource, IndexerCtx, SearchDocument, SearchDocumentProvider, SearchError};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

#[derive(Clone, Copy)]
enum Document {
    App,
    Shell,
}

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
        ("/missing", r#"<p class="dim">not found</p>"#),
    ] {
        let html = render(path, Document::App).await;
        assert!(html.contains(landmark), "{path}: {html}");
    }

    let html = render("/", Document::App).await;
    for fragment in [
        r"<main>",
        r#"placeholder="Search indexes""#,
        r#"method="get" action="/search""#,
        r#"name="page_size" value="25""#,
        r#"role="img" aria-label="peryx logo""#,
        r#"type="button" aria-label="Switch color theme""#,
        r#"href="/admin/topology""#,
        r#"href="https://peryx.readthedocs.io/""#,
        r#"href="https://github.com/tox-dev/peryx""#,
    ] {
        assert!(html.contains(fragment), "{fragment}: {html}");
    }
    assert!(!html.contains("All results"), "{html}");

    let html = render("/search?q=artifact&page_size=25", Document::App).await;
    for fragment in [
        r#"value="artifact""#,
        r#"href="/search?q=artifact&amp;page_size=25""#,
        "All results",
    ] {
        assert!(html.contains(fragment), "{fragment}: {html}");
    }

    let html = render("/", Document::Shell).await;
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

async fn render(path: &str, document: Document) -> String {
    render_document(path, document, false).await
}

async fn render_with_documents(path: &str) -> String {
    render_document(path, Document::App, true).await
}

async fn render_document(path: &str, document: Document, documents: bool) -> String {
    let _ = any_spawner::Executor::init_tokio();
    let directory = tempfile::tempdir().unwrap();
    let owner = Owner::new();
    owner.set();
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
    provide_context(Arc::new(app));
    provide_context(RequestUrl::new(path));
    match document {
        Document::App => App().into_any(),
        Document::Shell => shell(
            LeptosOptions::builder()
                .output_name("peryx_web")
                .site_root("ui")
                .site_pkg_dir("pkg")
                .build(),
        )
        .into_any(),
    }
    .to_html_stream_in_order()
    .collect::<String>()
    .await
}
