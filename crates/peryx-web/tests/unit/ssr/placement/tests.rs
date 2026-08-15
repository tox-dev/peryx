use peryx_core::{
    BlobDatacenterPlacement, BlobPlacementStatus, BlobPlacementView, PlacementHealth, PlacementRow, PlacementView,
    UiArtifactSource, UiByteAvailability,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::meta::{
    ArtifactPlacementHealth, ArtifactPlacementPage, ArtifactPlacementRow, ArtifactSource, BackendId, BackendLocation,
    BlobPlacementFailure, BlobPlacementKey, BlobPlacementRecord, BlobPlacementState, ByteAvailability, DataCenterId,
};

use super::{blob_placements_for_digest, parse_digest, placements_for_class};

const DIGEST_HEX: &str = "abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd";

macro_rules! placement {
    ($digest:literal, $source:expr, $availability:expr) => {
        ArtifactPlacementRow {
            digest: $digest.to_owned(),
            source: $source,
            availability: $availability,
        }
    };
}

macro_rules! projected_placement {
    ($digest:literal, $source:expr, $availability:expr) => {
        PlacementRow {
            digest: $digest.to_owned(),
            source: $source,
            availability: $availability,
        }
    };
}

macro_rules! blob_record {
    ($digest:expr, $data_center:literal, $state:expr, $updated_at:literal $(,)?) => {
        BlobPlacementRecord {
            key: BlobPlacementKey {
                digest: $digest.clone(),
                backend: BackendId::new(format!("backend-{}-{}", $data_center, $updated_at)).unwrap(),
                data_center: DataCenterId::new($data_center).unwrap(),
                location: BackendLocation::new(format!("location-{}-{}", $data_center, $updated_at)).unwrap(),
            },
            state: $state,
            fence: 1,
            generation: 2,
            updated_at_unix: $updated_at,
        }
    };
}

macro_rules! datacenter {
    ($data_center:literal, $status:expr, $size:expr, $updated_at:literal) => {
        BlobDatacenterPlacement {
            data_center: $data_center.to_owned(),
            status: $status,
            size: $size,
            updated_at: $updated_at,
        }
    };
}

#[test]
fn placements_projects_operator_health_without_rows() {
    assert_eq!(
        placements_for_class(
            40,
            Ok(ArtifactPlacementHealth {
                local: 2,
                remote_only: 3,
                unavailable: 5,
            }),
            None,
        ),
        Ok(PlacementView {
            captured_at: 40,
            health: PlacementHealth {
                local: 2,
                remote_only: 3,
                unavailable: 5,
                total: 10,
            },
            rows: None,
            next_cursor: None,
        })
    );
}

#[test]
fn placements_projects_administrator_rows_and_cursor() {
    assert_eq!(
        placements_for_class(
            40,
            Ok(ArtifactPlacementHealth::default()),
            Some(Ok(ArtifactPlacementPage {
                rows: vec![
                    placement!("sha256:1", ArtifactSource::Hosted, ByteAvailability::Local),
                    placement!("sha256:2", ArtifactSource::Proxy, ByteAvailability::RemoteOnly),
                    placement!("sha256:3", ArtifactSource::Generated, ByteAvailability::Unavailable),
                ],
                next_cursor: Some("sha256:3".to_owned()),
            })),
        ),
        Ok(PlacementView {
            captured_at: 40,
            health: PlacementHealth::default(),
            rows: Some(vec![
                projected_placement!("sha256:1", UiArtifactSource::Hosted, UiByteAvailability::Local),
                projected_placement!("sha256:2", UiArtifactSource::Proxy, UiByteAvailability::RemoteOnly),
                projected_placement!("sha256:3", UiArtifactSource::Generated, UiByteAvailability::Unavailable),
            ]),
            next_cursor: Some("sha256:3".to_owned()),
        })
    );
}

#[test]
fn placements_reports_health_errors() {
    assert_eq!(
        placements_for_class(40, Err(()), None),
        Err("Placement health could not be read.".to_owned())
    );
}

#[test]
fn placements_reports_row_errors() {
    assert_eq!(
        placements_for_class(40, Ok(ArtifactPlacementHealth::default()), Some(Err(())),),
        Err("Placement rows could not be read.".to_owned())
    );
}

#[test]
fn blob_placements_reports_store_errors() {
    let digest: ArtifactDigest = format!("sha256:{DIGEST_HEX}").parse().expect("digest is valid");
    assert_eq!(
        blob_placements_for_digest(&digest, Err(())),
        Err("Blob placement could not be read.".to_owned())
    );
}

#[test]
fn blob_placements_rejects_invalid_digests() {
    assert_eq!(
        parse_digest("invalid"),
        Err("That is not a valid artifact digest.".to_owned())
    );
}

#[test]
fn blob_placements_projects_each_state_and_sorts_datacenters() {
    let digest: ArtifactDigest = format!("sha256:{DIGEST_HEX}").parse().expect("digest is valid");
    assert_eq!(
        blob_placements_for_digest(
            &digest,
            Ok(vec![
                blob_record!(digest, "zeta", BlobPlacementState::Pending, 4),
                blob_record!(digest, "alpha", BlobPlacementState::Verified { size: 9 }, 3),
                blob_record!(
                    digest,
                    "alpha",
                    BlobPlacementState::Failed {
                        class: BlobPlacementFailure::SourceUnavailable,
                    },
                    1,
                ),
                blob_record!(digest, "beta", BlobPlacementState::Revoked, 2),
            ]),
        ),
        Ok(BlobPlacementView {
            digest: format!("sha256:{DIGEST_HEX}"),
            datacenters: vec![
                datacenter!("alpha", BlobPlacementStatus::Failed, None, 1),
                datacenter!("alpha", BlobPlacementStatus::Verified, Some(9), 3),
                datacenter!("beta", BlobPlacementStatus::Revoked, None, 2),
                datacenter!("zeta", BlobPlacementStatus::Pending, None, 4),
            ],
        })
    );
}
