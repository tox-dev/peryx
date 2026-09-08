use std::sync::Arc;

use futures_util::StreamExt as _;
use leptos::prelude::*;
use leptos_router::components::Router;
use leptos_router::location::RequestUrl;
use peryx_driver::AppState;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use crate::model::{stats_index, stats_resource, stats_routes};

use super::{Stats, StatsBody};

#[test]
fn stats_body_renders_each_drill_level() {
    let value = serde_json::json!({
        "totals": {"base": {"pages": 4, "reads": 2, "bytes": 2048, "writes": 1}},
        "routes": {"root/cache": {"base": {"pages": 4}}},
        "resources": {"artifact": {"base": {"pages": 4}}},
        "artifacts": {"artifact.bin": {"reads": 2, "bytes": 2048}},
    });
    for (route, resource, label, breadcrumb) in [
        (None, None, "Index", r#"<p class="breadcrumb"><span>usage</span></p>"#),
        (
            Some("root/cache"),
            None,
            "Resource",
            r#"<p class="breadcrumb"><a href="/stats">usage</a> / <span>root/cache</span></p>"#,
        ),
        (
            Some("root/cache"),
            Some("artifact"),
            "Artifact",
            r#"<a href="/stats?index=root%2Fcache">root/cache</a> / <span>artifact</span>"#,
        ),
    ] {
        let data = match (route, resource) {
            (Some(_), Some(_)) => stats_resource(&value),
            (Some(_), None) => stats_index(&value),
            _ => stats_routes(&value),
        };
        let html = view! {
            <StatsBody route=route.map(str::to_owned) resource=resource.map(str::to_owned) data />
        }
        .to_html();
        assert!(html.contains(breadcrumb), "{html}");
        assert!(html.contains(label), "{html}");
        assert!(html.contains("Listings"), "{html}");
        assert!(html.contains("2.0 kB"), "{html}");
    }
}

#[test]
fn stats_body_reports_empty_level() {
    let html = view! { <StatsBody route=None resource=None data=stats_routes(&serde_json::json!({})) /> }.to_html();
    assert!(html.contains("Nothing recorded at this level yet."), "{html}");
}

fn app() -> (tempfile::TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().unwrap();
    let app = AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStore::new(directory.path().join("blobs")),
        60,
        Vec::new(),
    );
    (directory, Arc::new(app))
}

/// The breadcrumb is where the drill level shows, so it reports which of `index` and `resource` the
/// page decided it was given.
async fn breadcrumb(url: &str) -> String {
    match any_spawner::Executor::init_tokio() {
        Ok(()) | Err(any_spawner::ExecutorError::AlreadySet) => {}
    }
    let (_directory, app) = app();
    let owner = Owner::new();
    owner.set();
    provide_context(app);
    provide_context(RequestUrl::new(url));
    let html = view! { <Router><Stats /></Router> }
        .to_html_stream_in_order()
        .collect::<String>()
        .await;
    let start = html.find(r#"<p class="breadcrumb">"#).expect("breadcrumb opens");
    let end = html[start..].find("</p>").expect("breadcrumb closes") + start;
    html[start..end].to_owned()
}

/// An `index=` with nothing after it names no index, so the page stays at the top level rather than
/// drilling into an index whose name is the empty string.
#[tokio::test]
async fn stats_treats_an_empty_index_query_as_no_index() {
    assert_eq!(
        breadcrumb("/stats?index=").await,
        r#"<p class="breadcrumb"><span>usage</span>"#
    );
}

/// A named index does drill in, which is what makes the empty case above a decision rather than a
/// page that never drills at all.
#[tokio::test]
async fn stats_drills_into_a_named_index() {
    assert!(breadcrumb("/stats?index=root/cache").await.contains("root/cache"));
}

/// The same rule holds for the resource level: an empty `resource=` leaves the page at its index.
#[tokio::test]
async fn stats_treats_an_empty_resource_query_as_no_resource() {
    assert_eq!(
        breadcrumb("/stats?index=root/cache&resource=").await,
        r#"<p class="breadcrumb"><a href="/stats">usage</a> / <span>root/cache</span>"#
    );
}

/// A named resource drills to the deepest level, so the empty case is a decision here too.
#[tokio::test]
async fn stats_drills_into_a_named_resource() {
    assert!(
        breadcrumb("/stats?index=root/cache&resource=artifact")
            .await
            .contains("artifact")
    );
}
