use super::*;
use crate::config::{DcMember, DcMembership, DcRole};
use peryx_driver::state::AppState;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::ObservedFrontier;

struct Frontiers;

impl ReclamationFrontiers for Frontiers {
    fn observe(&self) -> Option<ObservedFrontier> {
        None
    }
}

#[test]
fn missing_membership_has_no_reclaimer() {
    assert!(BlobReclamationSelector::from_config(&Config::default(), Arc::new(Frontiers)).is_none());
}

#[test]
fn reference_collection_surfaces_the_base_scan_failure() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();

    assert_eq!(
        collect_references(
            Err(anyhow::anyhow!("failed")),
            std::iter::empty::<&Arc<dyn EcosystemDriver>>(),
            &meta,
            &[],
        )
        .unwrap_err(),
        "failed"
    );
}

#[tokio::test]
async fn rostered_node_builds_and_runs_the_reclaimer() {
    let config = Config {
        writer_identity: Some("local".to_owned()),
        dc_membership: Some(DcMembership {
            group: "group".to_owned(),
            members: vec![DcMember {
                node: "local".to_owned(),
                dc: "home".to_owned(),
                address: "http://local/".to_owned(),
                role: DcRole::Writer,
            }],
        }),
        ..Config::default()
    };
    let meta_dir = tempfile::tempdir().unwrap();
    let blob_dir = tempfile::tempdir().unwrap();
    let state = Arc::new(AppState::new(
        MetaStore::open(meta_dir.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(blob_dir.path().join("blobs")),
        60,
        Vec::new(),
    ));
    let reclaimer = BlobReclamationSelector::from_config(&config, Arc::new(Frontiers)).unwrap();

    assert_eq!(
        reclaimer
            .bind(state.serving.clone())
            .reclaim_pass(&|| false, 1, std::num::NonZeroUsize::MIN)
            .await
            .unwrap(),
        peryx_ha::AvailabilityTaskReport::default()
    );
}
