use super::{
    BlobDatacenterPlacement, BlobPlacementStatus, BlobPlacementView, PlacementHealth, PlacementRow, PlacementView,
};
use crate::view::{UiArtifactSource, UiByteAvailability};

#[test]
fn test_blob_placement_view_round_trips_and_omits_a_pending_size() {
    let view = BlobPlacementView {
        digest: "sha256:aa".to_owned(),
        datacenters: vec![
            BlobDatacenterPlacement {
                data_center: "east-1".to_owned(),
                status: BlobPlacementStatus::Verified,
                size: Some(4096),
                updated_at: 1_800_000_000,
            },
            BlobDatacenterPlacement {
                data_center: "west-2".to_owned(),
                status: BlobPlacementStatus::Pending,
                size: None,
                updated_at: 1_800_000_050,
            },
        ],
    };
    let encoded = serde_json::to_string(&view).unwrap();
    assert!(
        !encoded.contains("\"size\":null"),
        "a pending copy omits size: {encoded}"
    );
    assert_eq!(serde_json::from_str::<BlobPlacementView>(&encoded).unwrap(), view);
}

#[test]
fn test_view_round_trips_through_json() {
    let view = PlacementView {
        captured_at: 1_800_000_000,
        health: PlacementHealth {
            local: 3,
            remote_only: 1,
            unavailable: 2,
            total: 6,
        },
        rows: Some(vec![PlacementRow {
            digest: "sha256:aa".to_owned(),
            source: UiArtifactSource::Proxy,
            availability: UiByteAvailability::RemoteOnly,
        }]),
        next_cursor: Some("sha256:aa".to_owned()),
    };
    let encoded = serde_json::to_string(&view).unwrap();
    assert_eq!(serde_json::from_str::<PlacementView>(&encoded).unwrap(), view);
}

#[test]
fn test_operator_view_omits_rows_and_cursor() {
    let view = PlacementView {
        captured_at: 7,
        health: PlacementHealth::default(),
        rows: None,
        next_cursor: None,
    };
    let value: serde_json::Value = serde_json::from_str(&serde_json::to_string(&view).unwrap()).unwrap();
    assert!(value.get("rows").is_none(), "{value}");
    assert!(value.get("next_cursor").is_none(), "{value}");
    assert_eq!(value["health"]["total"], 0);
}
