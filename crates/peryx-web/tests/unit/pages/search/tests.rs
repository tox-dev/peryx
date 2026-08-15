use std::sync::Arc;

use futures_util::StreamExt as _;
use leptos::prelude::*;
use leptos_router::components::Router;
use leptos_router::location::RequestUrl;
use peryx_core::Ecosystem;
use peryx_driver::{AppState, Index, IndexKind};
use peryx_identity::IndexAcl;
use peryx_search::{ContentSource, IndexerCtx, SearchDocument, SearchDocumentProvider, SearchError};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::Search;

struct Documents;

impl SearchDocumentProvider for Documents {
    fn documents(&self, _context: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok((1..=28)
            .map(|index| {
                let source = match index {
                    27 => ContentSource::Uploaded,
                    28 => ContentSource::Override,
                    _ => ContentSource::Cached,
                };
                SearchDocument {
                    display_label: format!("{} Artifact {index}", source.label()),
                    resource_key: format!("artifact-{index}"),
                    route: format!("root/cache/{index}"),
                    index: "root/cache".to_owned(),
                    ecosystem: "fixture".to_owned(),
                    source,
                    available_locally: index % 2 == 0,
                    summary: (index == 28).then(|| "Hosted override".to_owned()),
                    text: format!("{} artifact {index}", source.label()),
                }
            })
            .collect())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn search_renders_query_states() {
    initialize_executor();
    let (_directory, app) = state(true, false);
    for (url, expected) in [
        (
            "/search",
            [
                "Showing 1-25 of 28",
                r#"value="all" selected"#,
                r#"value="25" selected"#,
                r#"href="/search?page=2&amp;page_size=25""#,
            ],
        ),
        (
            "/search?q=Override&type=override&availability=local&page=1&page_size=50",
            [
                "Showing 1-1 of 1",
                r#"value="override" selected"#,
                r#"value="local" selected"#,
                r#"value="50" selected"#,
            ],
        ),
        (
            "/search?type=invalid&availability=remote&page=invalid&page_size=7",
            [
                "Showing 1-25 of 28",
                r#"value="all" selected"#,
                r#"value="25" selected"#,
                ">Page 1</span>",
            ],
        ),
        (
            "/search?page=0&page_size=100",
            [
                "Showing 1-28 of 28",
                r#"value="100" selected"#,
                ">Page 1</span>",
                r#"class="page-link disabled">Next"#,
            ],
        ),
        (
            "/search?page=2&page_size=25",
            [
                "Showing 26-28 of 28",
                r#"href="/search?page_size=25""#,
                ">Page 2</span>",
                r#"class="page-link disabled">Next"#,
            ],
        ),
        (
            "/search?page=3&page_size=25",
            [
                "This page is past the last result of 28.",
                r#"href="/search?page=2&amp;page_size=25" class="page-link">Go to last page"#,
                r#"aria-label="Search pages""#,
                "Go to last page",
            ],
        ),
        (
            "/search?q=missing",
            [
                "Nothing matched this search.",
                r#"value="missing""#,
                r#"value="all" selected"#,
                r#"value="25" selected"#,
            ],
        ),
    ] {
        let html = render(url, Arc::clone(&app)).await;
        for value in expected {
            assert_rendered_contains(&html, value, url);
        }
    }
}

fn assert_rendered_contains(html: &str, expected: &str, context: &str) {
    assert!(
        html.replace("<!>", "").contains(expected),
        "missing {expected:?} for {context}: {html}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn search_renders_result_metadata() {
    initialize_executor();
    let (_directory, app) = state(true, false);
    let html = render(
        "/search?q=Override&type=override&availability=local&page_size=50",
        Arc::clone(&app),
    )
    .await;
    for value in [
        r#"href="/browse?index=root%2Fcache%2F28""#,
        "Override Artifact 28",
        "artifact-28",
        "Hosted override",
        "source-override",
        "available-local",
        "Hosted files or overrides affect this upstream entry",
        "Bytes are held locally and served without an upstream fetch",
    ] {
        assert!(html.contains(value), "missing {value:?}: {html}");
    }
    assert_eq!(html.matches("<td").count(), 7, "{html}");

    let html = render("/search?q=Cached%20Artifact%201&type=cached", app).await;
    for value in [
        "source-cached",
        "available-remote",
        "No local bytes; a request fetches from upstream if it can",
    ] {
        assert!(html.contains(value), "missing {value:?}: {html}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn search_renders_empty_and_private_states() {
    initialize_executor();
    let (_directory, empty) = state(false, false);
    let html = render("/search", empty).await;
    assert!(
        html.contains("Nothing indexed yet. Cached resources appear after their artifacts are requested."),
        "{html}"
    );

    let (_directory, private) = state(false, true);
    let html = render("/search", private).await;
    assert!(html.contains(r#"class="error""#), "{html}");
    assert!(html.contains("request headers:"), "{html}");
}

fn initialize_executor() {
    match any_spawner::Executor::init_tokio() {
        Ok(()) | Err(any_spawner::ExecutorError::AlreadySet) => {}
    }
}

async fn render(url: &str, app: Arc<AppState>) -> String {
    let owner = Owner::new();
    owner.set();
    provide_context(app);
    provide_context(RequestUrl::new(url));
    view! { <Router><Search /></Router> }
        .to_html_stream_in_order()
        .collect::<String>()
        .await
}

fn state(documents: bool, private: bool) -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let indexes = private.then(|| Index {
        name: "private".to_owned(),
        route: "private".to_owned(),
        ecosystem: Ecosystem::new("fixture"),
        kind: IndexKind::Hosted { volatile: false },
        policy: peryx_policy::Policy::default(),
        acl: IndexAcl {
            anonymous_read: false,
            ..IndexAcl::default()
        },
    });
    let mut app = AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStore::new(directory.path().join("blobs")),
        60,
        indexes.into_iter().collect(),
    );
    if documents {
        Arc::get_mut(&mut app.serving)
            .unwrap()
            .search
            .add_indexer(Arc::new(Documents));
    }
    (directory, Arc::new(app))
}
