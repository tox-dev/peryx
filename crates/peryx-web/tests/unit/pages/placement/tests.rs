use std::sync::Arc;

use futures_util::StreamExt as _;
use leptos::prelude::*;
use leptos_router::location::RequestUrl;
use peryx_core::{PlacementHealth, PlacementRow, PlacementView, UiArtifactSource, UiByteAvailability};
use peryx_driver::AppState;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

use super::{ArtifactPlacements, PlacementBody, blob_placement_detail};
use crate::model::{BlobDatacenterPlacement, BlobPlacementStatus, BlobPlacementView};

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

#[test]
fn blob_placement_detail_says_when_no_datacenter_holds_the_blob() {
    let html: String = blob_placement_detail(&BlobPlacementView {
        digest: "sha256:abc".to_owned(),
        datacenters: Vec::new(),
    })
    .to_html();

    assert!(html.contains("No datacenter holds sha256:abc yet."), "{html}");
}

#[test]
fn blob_placement_detail_lists_each_datacenter_with_its_status_and_size() {
    let html: String = blob_placement_detail(&BlobPlacementView {
        digest: "sha256:abc".to_owned(),
        datacenters: vec![
            BlobDatacenterPlacement {
                data_center: "east".to_owned(),
                status: BlobPlacementStatus::Verified,
                size: Some(42),
                updated_at: 0,
            },
            BlobDatacenterPlacement {
                data_center: "west".to_owned(),
                status: BlobPlacementStatus::Pending,
                size: None,
                updated_at: 0,
            },
        ],
    })
    .to_html();

    assert!(html.contains("Datacenters holding sha256:abc"), "{html}");
    assert!(
        html.contains(
            r#"<td>east</td><td><span class="badge health-live">Verified</span></td><td class="num">42</td>"#
        ),
        "{html}"
    );
    assert!(
        html.contains(
            r#"<td>west</td><td><span class="badge health-unready">Pending</span></td><td class="num">-</td>"#
        ),
        "{html}"
    );
}

fn initialize_executor() {
    match any_spawner::Executor::init_tokio() {
        Ok(()) | Err(any_spawner::ExecutorError::AlreadySet) => {}
    }
}
