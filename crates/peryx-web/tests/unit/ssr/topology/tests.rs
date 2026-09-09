use peryx_core::{LocalStatus, NodeLiveness, NodeRole, TopologyView};
use peryx_driver::AppState;
use peryx_http::response_security::FieldClassification;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

use super::{local_status, local_status_from_observations, topology_view_for_class};

#[test]
fn topology_projects_authority_and_health() {
    for (class, expected) in [
        (FieldClassification::Public, TopologyView::Public),
        (FieldClassification::Repository, TopologyView::Public),
        (FieldClassification::Operator, TopologyView::Operator),
        (FieldClassification::Administrator, TopologyView::Administrator),
    ] {
        assert_eq!(topology_view_for_class(class), expected);
    }
    for (serial, blobs_healthy, expected) in [
        (
            Some(7),
            true,
            LocalStatus {
                role: NodeRole::Writer,
                liveness: NodeLiveness::Live,
                frontier: 7,
            },
        ),
        (
            Some(7),
            false,
            LocalStatus {
                role: NodeRole::Writer,
                liveness: NodeLiveness::Unready,
                frontier: 7,
            },
        ),
        (
            None,
            true,
            LocalStatus {
                role: NodeRole::Writer,
                liveness: NodeLiveness::Unready,
                frontier: 0,
            },
        ),
    ] {
        assert_eq!(
            local_status_from_observations(NodeRole::Writer, serial, blobs_healthy),
            expected
        );
    }
}

#[tokio::test]
async fn local_status_reads_the_blob_store_before_reporting_liveness() {
    for (blob_root_usable, expected) in [(true, NodeLiveness::Live), (false, NodeLiveness::Unready)] {
        let directory = tempfile::tempdir().unwrap();
        let blobs = directory.path().join("blobs");
        // A regular file where the blob root belongs is the seam: the health check creates and reads
        // the root as a directory, so it fails while the journal read beside it still succeeds.
        if !blob_root_usable {
            std::fs::write(&blobs, b"not a directory").unwrap();
        }
        let app = AppState::new(
            MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
            BlobStorage::filesystem(&blobs),
            60,
            Vec::new(),
        );

        assert_eq!(
            local_status(&app).await,
            LocalStatus {
                role: NodeRole::Writer,
                liveness: expected,
                frontier: 0,
            }
        );
    }
}
