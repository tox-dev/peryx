use std::collections::BTreeSet;
use std::sync::Arc;

use peryx_driver::serving::EcosystemDriver;
use peryx_driver::state::AppState;
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::{
    BackendId, BackendLocation, BlobPlacementKey, BlobPlacementTransition, DataCenterId, MetaStore, ReclamationState,
    ReclamationStatus,
};

use super::*;
use crate::config::{DcMember, DcMembership, DcRole};

fn meta() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn app(meta: MetaStore) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    (dir, Arc::new(AppState::new(meta, blobs, 60, Vec::new())))
}

/// Plant `content` in the node's content store so the pass's scan finds its digest.
fn store_blob(state: &AppState, content: &[u8]) -> (Digest, ArtifactDigest) {
    let blob = Digest::of(content);
    state
        .blobs
        .filesystem_store()
        .unwrap()
        .write_verified(content, &blob)
        .unwrap();
    let artifact = ArtifactDigest::from_sha256(blob.as_str()).unwrap();
    (blob, artifact)
}

/// Advance the metadata serial to `serial`, so a selection stamps a nonzero required frontier a lagging
/// plane must clear.
fn advance_serial(meta: &MetaStore, serial: u64) {
    for _ in 0..serial {
        meta.next_serial().unwrap();
    }
}

fn verified_placement(meta: &MetaStore, digest: &ArtifactDigest) {
    let key = BlobPlacementKey {
        digest: digest.clone(),
        backend: BackendId::new("filesystem").unwrap(),
        data_center: DataCenterId::new("home").unwrap(),
        location: BackendLocation::new("home/loc").unwrap(),
    };
    meta.apply_blob_placement(&key, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    meta.apply_blob_placement(
        &key,
        &BlobPlacementTransition::Verify {
            observed: digest.clone(),
            size: 1,
        },
        1,
        11,
    )
    .unwrap();
}

struct StubRefs(BTreeSet<String>);

impl ReferenceInventory for StubRefs {
    fn referenced(&self, _meta: &MetaStore) -> Result<BTreeSet<String>, String> {
        Ok(self.0.clone())
    }
}

struct FailingRefs;

impl ReferenceInventory for FailingRefs {
    fn referenced(&self, _meta: &MetaStore) -> Result<BTreeSet<String>, String> {
        Err("reference scan failed".to_owned())
    }
}

fn selector(referenced: &[&ArtifactDigest], frontier: ObservedFrontier) -> BlobReclamationSelector {
    let set = referenced.iter().map(|digest| digest.sha256().to_owned()).collect();
    BlobReclamationSelector {
        references: Arc::new(StubRefs(set)),
        frontiers: Arc::new(StubFrontiers(frontier)),
    }
}

struct StubFrontiers(ObservedFrontier);

impl ReclamationFrontiers for StubFrontiers {
    fn observe(&self) -> ObservedFrontier {
        self.0
    }
}

fn tombstone_status(meta: &MetaStore, digest: &ArtifactDigest) -> Option<ReclamationStatus> {
    meta.reclamation_tombstone(digest)
        .unwrap()
        .map(|tombstone| tombstone.state.status())
}

// --- fence -------------------------------------------------------------------

#[tokio::test]
async fn test_reclaim_pass_is_a_no_op_without_a_cluster_term() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    let report = selector(&[], ObservedFrontier { replica: 9, backup: 9 })
        .reclaim_pass(&state, &|| false, 0, ReclamationParameters::new())
        .await
        .unwrap();

    assert_eq!(report, JobReport::default(), "term 0 fences the pass shut");
    assert_eq!(tombstone_status(&state.meta, &artifact), None);
}

// --- select ------------------------------------------------------------------

#[tokio::test]
async fn test_pass_selects_an_unreferenced_digest_and_stamps_the_frontier() {
    let (_meta_dir, meta) = meta();
    advance_serial(&meta, 5);
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    let report = selector(&[], ObservedFrontier { replica: 0, backup: 0 })
        .reclaim_pass(&state, &|| false, 9, ReclamationParameters::new())
        .await
        .unwrap();

    assert_eq!(report.processed, 1);
    let tombstone = state.meta.reclamation_tombstone(&artifact).unwrap().unwrap();
    assert_eq!(tombstone.state, ReclamationState::Pending);
    assert_eq!(
        tombstone.required_frontier, 5,
        "the current metadata serial gates deletion"
    );
    assert_eq!(tombstone.fence, 9);
}

#[tokio::test]
async fn test_pass_leaves_a_referenced_digest_untouched() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"kept");

    let report = selector(&[&artifact], ObservedFrontier { replica: 9, backup: 9 })
        .reclaim_pass(&state, &|| false, 9, ReclamationParameters::new())
        .await
        .unwrap();

    assert_eq!(report.processed, 1);
    assert_eq!(report.changed, 0);
    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        None,
        "a referenced digest is never selected"
    );
}

#[tokio::test]
async fn test_pass_leaves_a_serveable_digest_untouched() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"serveable");
    verified_placement(&state.meta, &artifact);

    let report = selector(&[], ObservedFrontier { replica: 9, backup: 9 })
        .reclaim_pass(&state, &|| false, 9, ReclamationParameters::new())
        .await
        .unwrap();

    assert_eq!(report.changed, 0);
    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        None,
        "a verified placement a replica can serve blocks selection"
    );
}

#[tokio::test]
async fn test_pass_scans_only_the_batch() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    store_blob(&state, b"one");
    store_blob(&state, b"two");
    let mut params = ReclamationParameters::new();
    params.batch = std::num::NonZeroUsize::new(1).unwrap();

    let report = selector(&[], ObservedFrontier { replica: 0, backup: 0 })
        .reclaim_pass(&state, &|| false, 9, params)
        .await
        .unwrap();

    assert_eq!(report.processed, 1, "the batch bounds one pass to a single candidate");
}

// --- finalize / frontier gate ------------------------------------------------

#[tokio::test]
async fn test_a_covered_frontier_marks_a_candidate_ready() {
    let (_meta_dir, meta) = meta();
    advance_serial(&meta, 4);
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    let report = selector(&[], ObservedFrontier { replica: 4, backup: 6 })
        .reclaim_pass(&state, &|| false, 9, ReclamationParameters::new())
        .await
        .unwrap();

    assert_eq!(tombstone_status(&state.meta, &artifact), Some(ReclamationStatus::Ready));
    assert_eq!(report.changed, 2, "one selection and one readiness advance");
}

#[tokio::test]
async fn test_a_lagging_replica_blocks_readiness() {
    let (_meta_dir, meta) = meta();
    advance_serial(&meta, 5);
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    selector(&[], ObservedFrontier { replica: 2, backup: 9 })
        .reclaim_pass(&state, &|| false, 9, ReclamationParameters::new())
        .await
        .unwrap();

    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Pending),
        "a replica short of the required frontier keeps the candidate pending"
    );
}

#[tokio::test]
async fn test_a_lagging_backup_blocks_readiness() {
    let (_meta_dir, meta) = meta();
    advance_serial(&meta, 5);
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    selector(&[], ObservedFrontier { replica: 9, backup: 2 })
        .reclaim_pass(&state, &|| false, 9, ReclamationParameters::new())
        .await
        .unwrap();

    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Pending),
        "a backup short of the required frontier keeps the candidate pending"
    );
}

#[tokio::test]
async fn test_a_reference_returning_before_the_final_check_skips_the_candidate() {
    let (_meta_dir, meta) = meta();
    advance_serial(&meta, 3);
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");
    // First pass with a lagging replica arms a pending tombstone but cannot mark it ready.
    selector(&[], ObservedFrontier { replica: 0, backup: 0 })
        .reclaim_pass(&state, &|| false, 9, ReclamationParameters::new())
        .await
        .unwrap();
    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Pending)
    );

    // A reference reappears; the frontier is now covered, but the final check abandons the candidate.
    selector(&[&artifact], ObservedFrontier { replica: 9, backup: 9 })
        .reclaim_pass(&state, &|| false, 9, ReclamationParameters::new())
        .await
        .unwrap();

    assert_eq!(
        tombstone_status(&state.meta, &artifact),
        Some(ReclamationStatus::Skipped),
        "a reference that returned skips the candidate rather than marking it ready"
    );
}

// --- fencing -----------------------------------------------------------------

#[tokio::test]
async fn test_a_stale_worker_is_fenced_out_of_selection() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"contested");
    // A newer holder already owns the tombstone under term 12.
    state
        .meta
        .select_reclamation_candidate(&artifact, false, 0, 12, 10)
        .unwrap();

    let error = selector(&[], ObservedFrontier { replica: 9, backup: 9 })
        .reclaim_pass(&state, &|| false, 5, ReclamationParameters::new())
        .await
        .unwrap_err();

    assert_eq!(error.code(), "reclamation_select");
    assert_eq!(
        state.meta.reclamation_tombstone(&artifact).unwrap().unwrap().fence,
        12,
        "the superseded worker leaves the newer holder's tombstone untouched"
    );
}

// --- cancellation and failure ------------------------------------------------

#[tokio::test]
async fn test_a_cancelled_pass_selects_nothing() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    let (_blob, artifact) = store_blob(&state, b"orphan");

    let report = selector(&[], ObservedFrontier { replica: 9, backup: 9 })
        .reclaim_pass(&state, &|| true, 9, ReclamationParameters::new())
        .await
        .unwrap();

    assert_eq!(report, JobReport::default());
    assert_eq!(tombstone_status(&state.meta, &artifact), None);
}

#[tokio::test]
async fn test_a_reference_scan_failure_surfaces() {
    let (_meta_dir, meta) = meta();
    let (_app_dir, state) = app(meta);
    store_blob(&state, b"orphan");
    let reclaimer = BlobReclamationSelector {
        references: Arc::new(FailingRefs),
        frontiers: Arc::new(StubFrontiers(ObservedFrontier { replica: 0, backup: 0 })),
    };

    let error = reclaimer
        .reclaim_pass(&state, &|| false, 9, ReclamationParameters::new())
        .await
        .unwrap_err();

    assert_eq!(error.code(), "reclamation_references");
}

// --- production reference and frontier sources -------------------------------

#[test]
fn test_driver_references_reads_the_reference_inventory() {
    let (_meta_dir, meta) = meta();
    let inventory = DriverReferences {
        index_names: vec!["pypi".to_owned()],
    };

    let referenced = inventory.referenced(&meta).unwrap();

    assert!(referenced.is_empty(), "an empty store references no blobs");
}

struct StubDriver {
    trash: Result<Vec<peryx_core::TrashRecord>, String>,
}

#[async_trait]
impl EcosystemDriver for StubDriver {
    fn ecosystem(&self) -> peryx_core::Ecosystem {
        peryx_ecosystem_registry::PYPI
    }

    fn classify_route(&self, _path: &str) -> peryx_driver::rate_limit::RouteClass {
        peryx_driver::rate_limit::RouteClass::Listing
    }

    fn discover_index(
        &self,
        _index: peryx_driver::state::IndexDescription,
        _base: Option<&peryx_driver::discovery::BaseUrl>,
    ) -> serde_json::Value {
        serde_json::Value::Null
    }

    fn trash_records(
        &self,
        _meta: &MetaStore,
        _index_names: &[String],
    ) -> Result<Vec<peryx_core::TrashRecord>, String> {
        self.trash.clone()
    }
}

fn trashed(digest: Option<&str>) -> peryx_core::TrashRecord {
    peryx_core::TrashRecord {
        ecosystem: peryx_ecosystem_registry::PYPI,
        repository: "pypi".to_owned(),
        name: "demo".to_owned(),
        reference: None,
        digest: digest.map(ToOwned::to_owned),
        reason: None,
        actor: None,
        deleted_at_unix: 0,
        retained: true,
    }
}

#[test]
fn test_collect_references_surfaces_a_reference_scan_error() {
    let (_meta_dir, meta) = meta();
    let drivers: Vec<Arc<dyn EcosystemDriver>> = Vec::new();

    let error = collect_references(
        Err(anyhow::anyhow!("reference read failed")),
        drivers.iter(),
        &meta,
        &[],
    )
    .unwrap_err();

    assert_eq!(
        error, "reference read failed",
        "a base reference read error surfaces verbatim"
    );
}

#[test]
fn test_collect_references_surfaces_a_trash_scan_error() {
    let (_meta_dir, meta) = meta();
    let drivers: Vec<Arc<dyn EcosystemDriver>> = vec![Arc::new(StubDriver {
        trash: Err("store unreadable".to_owned()),
    })];

    let error = collect_references(Ok(BTreeSet::new()), drivers.iter(), &meta, &[]).unwrap_err();

    assert_eq!(
        error, "scan pypi trash: store unreadable",
        "a trash scan error names its ecosystem"
    );
}

#[test]
fn test_collect_references_folds_trashed_digests_over_the_base_set() {
    let (_meta_dir, meta) = meta();
    let base = BTreeSet::from(["already".to_owned()]);
    let drivers: Vec<Arc<dyn EcosystemDriver>> = vec![Arc::new(StubDriver {
        trash: Ok(vec![trashed(Some("sha256:aabb")), trashed(Some("ccdd")), trashed(None)]),
    })];

    let referenced = collect_references(Ok(base), drivers.iter(), &meta, &[]).unwrap();

    assert_eq!(
        referenced,
        BTreeSet::from(["already".to_owned(), "aabb".to_owned(), "ccdd".to_owned()]),
        "a prefixed digest is stripped, a bare digest is kept, and a digestless record is skipped"
    );
}

#[test]
fn test_deferred_frontiers_report_the_closed_frontier() {
    assert_eq!(DeferredFrontiers.observe(), ObservedFrontier { replica: 0, backup: 0 });
}

// --- from_config -------------------------------------------------------------

fn member(node: &str, data_center: &str) -> DcMember {
    DcMember {
        node: node.to_owned(),
        dc: data_center.to_owned(),
        address: format!("http://{node}/"),
        role: DcRole::Writer,
    }
}

fn config_with(membership: Option<DcMembership>, identity: Option<&str>) -> Config {
    Config {
        writer_identity: identity.map(ToOwned::to_owned),
        dc_membership: membership,
        ..Config::default()
    }
}

#[test]
fn test_from_config_builds_a_selector_for_a_rostered_node() {
    let config = config_with(
        Some(DcMembership {
            group: "group".to_owned(),
            members: vec![member("local", "home"), member("peer", "east")],
        }),
        Some("local"),
    );

    assert!(BlobReclamationSelector::from_config(&config).unwrap().is_some());
}

#[test]
fn test_from_config_reclaims_nothing_without_membership() {
    let config = config_with(None, Some("local"));

    assert!(BlobReclamationSelector::from_config(&config).unwrap().is_none());
}

#[test]
fn test_from_config_reclaims_nothing_when_this_node_is_unrostered() {
    let config = config_with(
        Some(DcMembership {
            group: "group".to_owned(),
            members: vec![member("someone-else", "east")],
        }),
        Some("local"),
    );

    assert!(BlobReclamationSelector::from_config(&config).unwrap().is_none());
}
