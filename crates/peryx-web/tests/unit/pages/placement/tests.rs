use std::sync::Arc;

use futures_util::StreamExt as _;
use leptos::prelude::*;
use leptos_router::location::RequestUrl;
use peryx_core::{PlacementHealth, PlacementRow, PlacementView, UiArtifactSource, UiByteAvailability};
use peryx_driver::AppState;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

use super::{ArtifactPlacements, PlacementBody};

#[tokio::test(flavor = "current_thread")]
async fn artifact_placements_reports_public_access_denial() {
    initialize_executor();
    let directory = tempfile::tempdir().unwrap();
    let owner = Owner::new();
    owner.set();
    provide_context(Arc::new(AppState::new(
        MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(directory.path().join("blobs")),
        60,
        Vec::new(),
    )));
    provide_context(RequestUrl::new("/placements"));

    let html = view! { <ArtifactPlacements /> }
        .to_html_stream_in_order()
        .collect::<String>()
        .await;

    assert!(html.contains("You do not have access to placement health."), "{html}");
}

#[tokio::test(flavor = "current_thread")]
async fn placement_body_renders_rows_and_pager() {
    let _ = any_spawner::Executor::init_tokio();
    let owner = Owner::new();
    owner.set();
    let (_, set_cursor) = signal(None::<String>);
    let html = view! {
        <PlacementBody
            view=PlacementView {
                captured_at: 0,
                health: PlacementHealth { local: 1, remote_only: 2, unavailable: 3, total: 6 },
                rows: Some(vec![
                    PlacementRow {
                        digest: "sha256:hosted".to_owned(),
                        source: UiArtifactSource::Hosted,
                        availability: UiByteAvailability::Local,
                    },
                    PlacementRow {
                        digest: "sha256:proxy".to_owned(),
                        source: UiArtifactSource::Proxy,
                        availability: UiByteAvailability::RemoteOnly,
                    },
                    PlacementRow {
                        digest: "sha256:generated".to_owned(),
                        source: UiArtifactSource::Generated,
                        availability: UiByteAvailability::Unavailable,
                    },
                ]),
                next_cursor: Some("next".to_owned()),
            }
            set_cursor
        />
    }
    .to_html();
    for (digest, source, availability_hint) in [
        ("sha256:hosted", "hosted", "Bytes are held by this instance"),
        ("sha256:proxy", "proxy", "Bytes require a remote source"),
        ("sha256:generated", "generated", "No source can serve these bytes"),
    ] {
        let (_, rest) = html.split_once(digest).expect("placement row is rendered");
        let (row, _) = rest.split_once("</tr>").expect("placement row is complete");
        assert!(
            row.contains(&format!(">{source}</span>")),
            "missing source {source:?} in {row}"
        );
        assert!(
            row.contains(availability_hint),
            "missing availability {availability_hint:?} in {row}"
        );
    }
    assert!(html.contains("Next page"), "{html}");
    let withheld = view! {
        <PlacementBody
            view=PlacementView {
                captured_at: 0,
                health: PlacementHealth { local: 1, remote_only: 2, unavailable: 3, total: 6 },
                rows: None,
                next_cursor: None,
            }
            set_cursor
        />
    }
    .to_html();
    let empty = view! {
        <PlacementBody
            view=PlacementView {
                captured_at: 0,
                health: PlacementHealth { local: 1, remote_only: 2, unavailable: 3, total: 6 },
                rows: Some(Vec::new()),
                next_cursor: None,
            }
            set_cursor
        />
    }
    .to_html();
    assert!(withheld.contains("need administrator access"), "{withheld}");
    assert!(empty.contains("No artifact placements are recorded yet."), "{empty}");
}

fn initialize_executor() {
    match any_spawner::Executor::init_tokio() {
        Ok(()) | Err(any_spawner::ExecutorError::AlreadySet) => {}
    }
}
