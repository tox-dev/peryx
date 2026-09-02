use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use peryx_core::{NodeRole, TopologyConfig, TopologyMember, TopologyMode};
use peryx_identity::{
    LdapBindMode, LdapLoginService, LdapProvider, LdapProviderSettings, OidcLoginProvider, OidcLoginService,
    OidcProviderSettings, ProviderId,
};
use peryx_storage::blob::{BlobDurability, BlobMetadata, BlobStore, Digest, WriteEvidence};
use peryx_storage::meta::MetaStore;
use tracing_subscriber::layer::SubscriberExt as _;
use url::Url;

use super::AppState;

const DIGEST_HEX: &str = "abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd";

fn member(node: &str, dc: &str) -> TopologyMember {
    TopologyMember {
        node: node.to_owned(),
        dc: dc.to_owned(),
        address: format!("{node}:8080"),
        role: NodeRole::Writer,
    }
}

fn home_topology(dc: &str) -> TopologyConfig {
    TopologyConfig {
        mode: TopologyMode::Dc,
        group: Some("group".to_owned()),
        members: vec![member("writer", dc)],
        local_node: Some("writer".to_owned()),
    }
}

fn state_with(topology: TopologyConfig) -> (tempfile::TempDir, AppState) {
    state_with_capabilities(topology, peryx_ha::AvailabilityCapabilities::default())
}

fn state_with_capabilities(
    topology: TopologyConfig,
    capabilities: peryx_ha::AvailabilityCapabilities,
) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.initialize_distributed_state().unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta, blobs, 60, Vec::new());
    state
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: NodeRole::Writer,
            topology,
            blobs: peryx_ha::BlobServices::new(None, Arc::new(Durability::new(peryx_ha::WriteDurability::Unavailable))),
            analytics: Arc::new(Completeness),
            capabilities,
            authority_drainer: None,
        })
        .unwrap();
    (dir, state)
}

fn local_state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, AppState::new(meta, blobs, 60, Vec::new()))
}

#[test]
fn test_state_reports_unreadable_metrics_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    drop(MetaStore::open(&path).unwrap());
    let database = redb::Database::open(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .delete_table(redb::TableDefinition::<&str, &[u8]>::new("analytics"))
        .unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, u64>::new("analytics"))
        .unwrap();
    transaction.commit().unwrap();
    drop(database);
    let log = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::ERROR)
        .with_writer(log.reopen().unwrap())
        .finish();

    let state = tracing::subscriber::with_default(subscriber, || {
        AppState::new(
            MetaStore::open_existing(path).unwrap(),
            BlobStore::new(dir.path().join("blobs")),
            60,
            Vec::new(),
        )
    });

    assert!(
        state
            .serving
            .metrics
            .durability_failure()
            .is_some_and(|error| error.contains("analytics"))
    );
    let output = std::fs::read_to_string(log.path()).unwrap();
    assert!(output.contains("durable metrics startup failed"), "{output}");
    assert!(output.contains("analytics"), "{output}");
}

struct AvailabilityCapability;

#[async_trait]
impl peryx_ha::CrossDcCopier for AvailabilityCapability {
    async fn copy_pass(
        &self,
        _cancelled: &(dyn Fn() -> bool + Send + Sync),
        _concurrency: NonZeroUsize,
    ) -> Result<peryx_ha::AvailabilityTaskReport, peryx_ha::AvailabilityTaskError> {
        Ok(peryx_ha::AvailabilityTaskReport::default())
    }
}

#[async_trait]
impl peryx_ha::BlobReclaimer for AvailabilityCapability {
    async fn reclaim_pass(
        &self,
        _cancelled: &(dyn Fn() -> bool + Send + Sync),
        _fence: u64,
        _batch: NonZeroUsize,
    ) -> Result<peryx_ha::AvailabilityTaskReport, peryx_ha::AvailabilityTaskError> {
        Ok(peryx_ha::AvailabilityTaskReport::default())
    }
}

#[async_trait]
impl peryx_ha::PlacementReconciler for AvailabilityCapability {
    async fn reconcile_pass(
        &self,
        _cancelled: &(dyn Fn() -> bool + Send + Sync),
        _batch: NonZeroUsize,
    ) -> Result<peryx_ha::AvailabilityTaskReport, peryx_ha::AvailabilityTaskError> {
        Ok(peryx_ha::AvailabilityTaskReport::default())
    }
}

struct HomePlacementCapability {
    observed: Mutex<Option<(String, u64, u64)>>,
    error: Option<String>,
}

#[derive(Clone, Default)]
struct EventCapture(Arc<Mutex<Vec<String>>>);

impl<Subscriber> tracing_subscriber::Layer<Subscriber> for EventCapture
where
    Subscriber: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _context: tracing_subscriber::layer::Context<'_, Subscriber>) {
        struct Visitor<'a>(&'a mut Vec<String>);

        impl tracing::field::Visit for Visitor<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0.push(format!("{value:?}"));
                }
            }
        }

        event.record(&mut Visitor(&mut self.0.lock().unwrap()));
    }
}

impl peryx_ha::HomePlacementRecorder for HomePlacementCapability {
    fn record(&self, digest: &str, size: u64, fence: u64) -> Result<(), String> {
        *self.observed.lock().unwrap() = Some((digest.to_owned(), size, fence));
        self.error.clone().map_or(Ok(()), Err)
    }
}

#[test]
fn test_record_home_placement_delegates_to_the_installed_capability() {
    let capability = Arc::new(HomePlacementCapability {
        observed: Mutex::new(None),
        error: None,
    });
    let (_dir, state) = state_with_capabilities(
        home_topology("home"),
        peryx_ha::AvailabilityCapabilities {
            home_placement: Some(capability.clone()),
            ..Default::default()
        },
    );

    state.serving.record_home_placement(DIGEST_HEX, 2_048, 3);

    assert_eq!(
        *capability.observed.lock().unwrap(),
        Some((DIGEST_HEX.to_owned(), 2_048, 3))
    );
}

#[test]
fn test_record_home_placement_exposes_capability_failures_in_metrics() {
    let capability = Arc::new(HomePlacementCapability {
        observed: Mutex::new(None),
        error: Some("stale fence".to_owned()),
    });
    let (_dir, state) = state_with_capabilities(
        home_topology("home"),
        peryx_ha::AvailabilityCapabilities {
            home_placement: Some(capability.clone()),
            ..Default::default()
        },
    );

    state.serving.record_home_placement(DIGEST_HEX, 2_048, 2);
    let mut metrics = String::new();
    state.write_process_metrics(&mut metrics);

    assert_eq!(
        (capability.observed.lock().unwrap().clone(), metrics),
        (Some((DIGEST_HEX.to_owned(), 2_048, 2)), home_placement_metrics(1))
    );
}

#[test]
fn test_record_home_placement_reports_a_missing_capability() {
    let capture = EventCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    let (_dir, state) = state_with(home_topology("home"));

    state.serving.record_home_placement(DIGEST_HEX, 2_048, 2);
    let mut metrics = String::new();
    state.write_process_metrics(&mut metrics);

    assert_eq!(
        (capture.0.lock().unwrap().clone(), metrics),
        (
            vec!["home placement recorder is unavailable".to_owned()],
            home_placement_metrics(1),
        )
    );
}

struct Metrics;

impl super::PrometheusSource for Metrics {
    fn write_metrics(&self, body: &mut String) {
        body.push_str("metric 1\n");
    }
}

struct Completeness;

impl peryx_ha::AnalyticsCompleteness for Completeness {
    fn assess(
        &self,
        _meta: &dyn peryx_ha::AnalyticsSnapshotStore,
        _expected: &[peryx_ha::ExpectedProducer],
        _query: &peryx_ha::CompletenessQuery,
    ) -> Result<peryx_ha::CompletenessReport, peryx_ha::CompletenessError> {
        Err(peryx_ha::CompletenessError)
    }
}

struct Availability {
    observed: std::sync::Mutex<Option<String>>,
}

impl Availability {
    fn new() -> Self {
        Self {
            observed: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait]
impl peryx_ha::BlobAvailability for Availability {
    async fn ensure_local(&self, digest: &Digest) -> Result<Option<BlobMetadata>, peryx_ha::BlobAvailabilityError> {
        *self.observed.lock().unwrap() = Some(digest.as_str().to_owned());
        Ok(Some(BlobMetadata {
            bytes: 17,
            modified: None,
        }))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ObservedWrite {
    digest: String,
    authority: String,
    epoch: peryx_ha::AuthorityEpoch,
    evidence: WriteEvidence,
}

struct Durability {
    outcome: peryx_ha::WriteDurability,
    observed: std::sync::Mutex<Option<ObservedWrite>>,
}

struct MetadataDurability {
    observed: Mutex<Option<(String, peryx_ha::AuthorityEpoch, u64)>>,
}

#[async_trait]
impl peryx_ha::MetadataWriteDurability for MetadataDurability {
    async fn confirm_metadata(&self, write: peryx_ha::CommittedMetadata<'_>) -> peryx_ha::WriteDurability {
        *self.observed.lock().unwrap() = Some((write.authority().to_owned(), write.epoch(), write.commit().serial()));
        peryx_ha::WriteDurability::Pending
    }
}

impl Durability {
    fn new(outcome: peryx_ha::WriteDurability) -> Self {
        Self {
            outcome,
            observed: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait]
impl peryx_ha::BlobWriteDurability for Durability {
    async fn confirm(&self, write: peryx_ha::CommittedBlob<'_>) -> peryx_ha::WriteDurability {
        *self.observed.lock().unwrap() = Some(ObservedWrite {
            digest: write.digest().as_str().to_owned(),
            authority: write.authority().to_owned(),
            epoch: write.epoch(),
            evidence: write.evidence(),
        });
        self.outcome
    }
}

#[test]
fn test_process_metrics_render_registered_sources() {
    let (_dir, state) = state_with(TopologyConfig::default());
    state.register_prometheus(Arc::new(Metrics));
    let mut body = String::new();

    state.write_process_metrics(&mut body);

    assert_eq!(body, format!("{}metric 1\n", home_placement_metrics(0)));
}

fn home_placement_metrics(failures: u64) -> String {
    format!(
        "# HELP peryx_home_placement_record_failures_total Home placement record failures.\n\
         # TYPE peryx_home_placement_record_failures_total counter\n\
         peryx_home_placement_record_failures_total {failures}\n"
    )
}

#[tokio::test]
async fn test_none_mode_has_no_distributed_runtime() {
    let (_dir, state) = local_state();
    let serving = state.serving.as_ref();
    let digest = Digest::of(b"local");

    assert!(serving.is_ready(true).await);
    assert_eq!(serving.availability_role(), NodeRole::Writer);
    assert!(serving.analytics_completeness().is_none());
    assert_eq!(serving.ensure_blob_local(&digest).await.unwrap(), None);
    assert_eq!(
        serving
            .confirm_blob_write(peryx_ha::CommittedBlob::new(
                &digest,
                b"local".len() as u64,
                "catalog",
                peryx_ha::AuthorityEpoch(3),
                None,
                WriteEvidence::NodeLocal,
                peryx_ha::OperationKind::Publish,
            ))
            .await,
        peryx_ha::WriteDurability::Confirmed {
            scope: BlobDurability::Filesystem,
        }
    );
    assert_eq!(
        serving
            .confirm_metadata_write(peryx_ha::CommittedMetadata::new(
                "catalog",
                peryx_ha::AuthorityEpoch(3),
                peryx_storage::meta::JournalCommit::new(1),
                peryx_ha::OperationKind::Publish,
            ))
            .await,
        peryx_ha::WriteDurability::Confirmed {
            scope: BlobDurability::Filesystem,
        }
    );
}

#[tokio::test]
async fn test_readiness_rejects_uninitialized_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    drop(redb::Database::create(&path).unwrap());
    let state = AppState::new(
        MetaStore::open_existing_read_only(path).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    );

    assert!(!state.serving.is_ready(false).await);
}

#[tokio::test]
async fn test_readiness_rejects_an_unusable_blob_root() {
    let dir = tempfile::tempdir().unwrap();
    let blob_root = dir.path().join("blobs");
    std::fs::write(&blob_root, b"not a directory").unwrap();
    let state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStore::new(blob_root),
        60,
        Vec::new(),
    );

    assert!(!state.serving.is_ready(false).await);
}

#[rstest::rstest]
#[case(false, true)]
#[case(true, false)]
#[tokio::test]
async fn test_read_only_readiness_distinguishes_requests(#[case] writes: bool, #[case] expected: bool) {
    let (_dir, mut state) = local_state();
    state.set_read_only(true).unwrap();

    assert_eq!(state.serving.is_ready(writes).await, expected);
}

#[tokio::test]
async fn test_distributed_state_without_remote_blob_availability_stays_local() {
    let (_dir, state) = state_with(TopologyConfig::default());
    let serial = state.serving.meta.current_serial().unwrap();

    assert_eq!(
        state
            .serving
            .ensure_blob_local(&Digest::of(b"local-only"))
            .await
            .unwrap(),
        None
    );
    state.serving.record_home_placement(DIGEST_HEX, 1, 1);
    assert_eq!(state.serving.meta.current_serial().unwrap(), serial);
}

#[tokio::test]
async fn test_configured_blob_services_receive_requests() {
    let (_dir, mut state) = local_state();
    let availability = Arc::new(Availability::new());
    let durability = Arc::new(Durability::new(peryx_ha::WriteDurability::Pending));
    let metadata_durability = Arc::new(MetadataDurability {
        observed: Mutex::new(None),
    });
    state
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: NodeRole::Replica,
            topology: home_topology("home"),
            blobs: peryx_ha::BlobServices::new(Some(availability.clone()), durability.clone())
                .with_metadata_durability(metadata_durability.clone()),
            analytics: Arc::new(Completeness),
            capabilities: peryx_ha::AvailabilityCapabilities::default(),
            authority_drainer: None,
        })
        .unwrap();
    let serving = state.serving.as_ref();
    let digest = Digest::of(b"remote");

    assert!(
        serving
            .analytics_completeness()
            .unwrap()
            .assess(
                &serving.meta,
                &[],
                &peryx_ha::CompletenessQuery {
                    from_day: 1,
                    to_day: 2,
                    today: 3,
                    repository: Some("catalog".to_owned()),
                },
            )
            .is_err()
    );
    assert_eq!(serving.availability_role(), NodeRole::Replica);
    assert_eq!(
        serving.ensure_blob_local(&digest).await.unwrap(),
        Some(BlobMetadata {
            bytes: 17,
            modified: None,
        })
    );
    assert_eq!(availability.observed.lock().unwrap().as_deref(), Some(digest.as_str()));
    assert_eq!(
        serving
            .confirm_blob_write(peryx_ha::CommittedBlob::new(
                &digest,
                17,
                "catalog",
                peryx_ha::AuthorityEpoch(7),
                None,
                WriteEvidence::ObjectStoreVerified,
                peryx_ha::OperationKind::Publish,
            ))
            .await,
        peryx_ha::WriteDurability::Pending
    );
    assert_eq!(
        *durability.observed.lock().unwrap(),
        Some(ObservedWrite {
            digest: digest.as_str().to_owned(),
            authority: "catalog".to_owned(),
            epoch: peryx_ha::AuthorityEpoch(7),
            evidence: WriteEvidence::ObjectStoreVerified,
        })
    );
    let commit = serving
        .meta
        .commit_driver_txn_with_commit::<(), peryx_storage::meta::MetaError>(|txn| {
            txn.put("metadata", b"committed")?;
            Ok(((), vec![b"metadata".to_vec()]))
        })
        .unwrap()
        .journal
        .unwrap();
    assert_eq!(
        serving
            .confirm_metadata_write(peryx_ha::CommittedMetadata::new(
                "catalog",
                peryx_ha::AuthorityEpoch(8),
                commit,
                peryx_ha::OperationKind::Publish,
            ))
            .await,
        peryx_ha::WriteDurability::Pending
    );
    assert_eq!(
        *metadata_durability.observed.lock().unwrap(),
        Some(("catalog".to_owned(), peryx_ha::AuthorityEpoch(8), commit.serial()))
    );
}

#[tokio::test]
async fn test_configured_availability_capabilities_are_exposed() {
    let (_dir, mut state) = local_state();
    let copier: Arc<dyn peryx_ha::CrossDcCopier> = Arc::new(AvailabilityCapability);
    let reclaimer: Arc<dyn peryx_ha::BlobReclaimer> = Arc::new(AvailabilityCapability);
    let placement: Arc<dyn peryx_ha::PlacementReconciler> = Arc::new(AvailabilityCapability);
    state
        .install_distributed_availability(peryx_ha::AvailabilityStateInstall {
            role: NodeRole::Writer,
            topology: TopologyConfig::default(),
            blobs: peryx_ha::BlobServices::new(None, Arc::new(Durability::new(peryx_ha::WriteDurability::Unavailable))),
            analytics: Arc::new(Completeness),
            capabilities: peryx_ha::AvailabilityCapabilities {
                copier: Some(copier.clone()),
                reclaimer: Some(reclaimer.clone()),
                placement: Some(placement.clone()),
                ..Default::default()
            },
            authority_drainer: None,
        })
        .unwrap();
    let serving = state.serving.as_ref();
    let cancelled = || false;

    assert!(Arc::ptr_eq(serving.cross_dc_copier().unwrap(), &copier));
    assert_eq!(
        serving
            .cross_dc_copier()
            .unwrap()
            .copy_pass(&cancelled, NonZeroUsize::MIN)
            .await
            .unwrap(),
        peryx_ha::AvailabilityTaskReport::default()
    );
    assert!(Arc::ptr_eq(serving.blob_reclaimer().unwrap(), &reclaimer));
    assert_eq!(
        serving
            .blob_reclaimer()
            .unwrap()
            .reclaim_pass(&cancelled, 1, NonZeroUsize::MIN)
            .await
            .unwrap(),
        peryx_ha::AvailabilityTaskReport::default()
    );
    assert!(Arc::ptr_eq(serving.placement_reconciler().unwrap(), &placement));
    assert_eq!(
        serving
            .placement_reconciler()
            .unwrap()
            .reconcile_pass(&cancelled, NonZeroUsize::MIN)
            .await
            .unwrap(),
        peryx_ha::AvailabilityTaskReport::default()
    );
}

#[tokio::test]
async fn test_none_mode_preserves_single_node_ownership_semantics() {
    let (_dir, state) = local_state();
    let serving = state.serving.as_ref();

    serving.claim_first_publish_home("catalog").await.unwrap();

    assert_eq!(serving.committed_authority_epoch("catalog").await, 0);
    assert!(serving.admit_authority_epoch("catalog", 41).await);
    assert_eq!(serving.transfer_authority_home("catalog", "west").await.unwrap(), None);
}

#[test]
fn test_login_services_and_session_sealer_install_by_provider_id() {
    let (_dir, mut state) = state_with(TopologyConfig::default());

    assert!(state.serving.ldap_login("missing").is_none());
    assert!(state.serving.oidc_login("missing").is_none());
    assert!(state.serving.oidc_providers().is_empty());
    assert!(state.serving.session_sealer().is_none());

    state
        .set_ldap_logins([LdapLoginService::new(
            LdapProvider::new(LdapProviderSettings {
                id: ProviderId::new("directory").unwrap(),
                url: Url::parse("ldap://127.0.0.1:9").unwrap(),
                base_dn: "ou=people,dc=example,dc=com".to_owned(),
                bind: LdapBindMode::Direct {
                    dn_attribute: "uid".to_owned(),
                },
                subject_attribute: "entryUUID".to_owned(),
                display_name_attribute: "displayName".to_owned(),
                group_attribute: None,
                custom_ca_pem: None,
                connect_timeout: Duration::from_millis(20),
                request_timeout: Duration::from_millis(40),
                max_connections: NonZeroU32::new(1).unwrap(),
            })
            .unwrap(),
            state.serving.meta.clone(),
            Vec::new(),
        )])
        .unwrap();
    state
        .set_oidc_logins([OidcLoginService::new(
            OidcLoginProvider::new(
                OidcProviderSettings {
                    id: ProviderId::new("browser").unwrap(),
                    issuer: "https://issuer.example".to_owned(),
                    client_id: "peryx-web".to_owned(),
                    client_secret: None,
                    redirect_uri: Url::parse("https://registry.example/oidc/browser/callback").unwrap(),
                    scopes: vec!["openid".to_owned()],
                    subject_claim: "sub".to_owned(),
                    display_name_claim: "name".to_owned(),
                    groups_claim: None,
                    clock_skew: Duration::from_mins(1),
                },
                Arc::new(
                    crate::oidc::GuardedOidcTransport::new(
                        ["https://issuer.example"],
                        std::iter::empty::<&str>(),
                        Duration::from_secs(5),
                    )
                    .unwrap(),
                ),
            )
            .unwrap(),
            state.serving.meta.clone(),
            Vec::new(),
        )])
        .unwrap();
    state
        .set_session_sealer(peryx_identity::SessionSealer::new(b"session-key"))
        .unwrap();
    let serving = state.serving.as_ref();

    assert!(serving.ldap_login("directory").is_some());
    assert!(serving.oidc_login("browser").is_some());
    assert_eq!(serving.oidc_providers(), vec!["browser"]);
    assert!(serving.session_sealer().is_some());
}

#[test]
fn test_app_describes_no_indexes_when_none_are_configured() {
    let (_dir, state) = local_state();

    assert!(state.serving.describe_indexes().is_empty());
}

struct OwnershipCapability;

#[async_trait]
impl peryx_ha::OwnershipAuthority for OwnershipCapability {
    async fn claim_home(&self, authority: &str) -> Result<peryx_ha::HomeClaim, peryx_ha::OwnershipError> {
        Ok(peryx_ha::HomeClaim {
            home: if authority == "catalog" { "home" } else { "west" }.to_owned(),
            epoch: self.committed_epoch(authority).await,
        })
    }

    fn cluster_status(&self) -> peryx_ha::ClusterStatus {
        peryx_ha::ClusterStatus {
            leader: Some("home".to_owned()),
            term: 7,
            voters: vec!["home".to_owned()],
        }
    }

    async fn committed_epoch(&self, authority: &str) -> u64 {
        u64::from(authority == "catalog") * 7
    }

    async fn admit_epoch(&self, authority: &str, presented: u64) -> bool {
        presented >= self.committed_epoch(authority).await
    }

    async fn begin_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<peryx_ha::AuthorityWriteLease>, peryx_ha::OwnershipError> {
        Ok(self
            .admit_epoch(authority, presented)
            .await
            .then(|| peryx_ha::AuthorityWriteLease {
                authority: authority.to_owned(),
                epoch: presented,
                id: "write-1".to_owned(),
                expires_at_unix: i64::MAX,
            }))
    }

    async fn finish_epoch_write(&self, _lease: &peryx_ha::AuthorityWriteLease) -> Result<(), peryx_ha::OwnershipError> {
        Ok(())
    }

    async fn transfer_home(
        &self,
        authority: &str,
        new_home: &str,
    ) -> Result<Option<peryx_ha::TransferOutcome>, peryx_ha::OwnershipError> {
        Ok(Some(peryx_ha::TransferOutcome {
            from: authority.to_owned(),
            to: new_home.to_owned(),
            epoch: 8,
        }))
    }
}

#[tokio::test]
async fn test_distributed_ownership_and_topology_delegate_to_the_capability() {
    let ownership: Arc<dyn peryx_ha::OwnershipAuthority> = Arc::new(OwnershipCapability);
    let topology = home_topology("home");
    let (_dir, state) = state_with_capabilities(
        topology.clone(),
        peryx_ha::AvailabilityCapabilities {
            ownership: Some(ownership.clone()),
            ..Default::default()
        },
    );
    let serving = state.serving.as_ref();

    assert_eq!(serving.availability_topology(), &topology);
    assert!(Arc::ptr_eq(serving.ownership_authority().unwrap(), &ownership));
    assert_eq!(
        ownership.claim_home("catalog").await.unwrap(),
        peryx_ha::HomeClaim {
            home: "home".to_owned(),
            epoch: 7,
        }
    );
    assert_eq!(ownership.cluster_status().term, 7);
    assert_eq!(serving.committed_authority_epoch("catalog").await, 7);
    assert!(!serving.admit_authority_epoch("catalog", 6).await);
    let lease = serving
        .begin_authority_epoch_write("catalog", 7)
        .await
        .unwrap()
        .unwrap();
    serving.finish_authority_epoch_write(&lease).await.unwrap();
    assert_eq!(
        serving.transfer_authority_home("catalog", "west").await.unwrap(),
        Some(peryx_ha::TransferOutcome {
            from: "catalog".to_owned(),
            to: "west".to_owned(),
            epoch: 8,
        })
    );
}

#[tokio::test]
async fn test_none_mode_and_webhook_host_expose_process_state() {
    let (_dir, state) = local_state();
    let serving = state.serving.as_ref();

    assert_eq!(serving.availability_topology().mode, peryx_core::TopologyMode::None);
    assert!(serving.authority_drainer().is_none());
    assert!(serving.ownership_authority().is_none());
    assert!(serving.cross_dc_copier().is_none());
    assert!(serving.blob_reclaimer().is_none());
    assert!(serving.placement_reconciler().is_none());
    assert_eq!(serving.begin_authority_epoch_write("catalog", 7).await.unwrap(), None);
    serving
        .finish_authority_epoch_write(&peryx_ha::AuthorityWriteLease {
            authority: "catalog".to_owned(),
            epoch: 7,
            id: "write-1".to_owned(),
            expires_at_unix: 100,
        })
        .await
        .unwrap();
    let serial = serving.meta.current_serial().unwrap();
    serving.record_home_placement(DIGEST_HEX, 1, 1);
    assert_eq!(serving.meta.current_serial().unwrap(), serial);
    assert!(std::ptr::eq(
        peryx_events::webhook::WebhookHost::webhooks(serving),
        &raw const serving.webhooks
    ));
    assert!(std::ptr::eq(
        peryx_events::webhook::WebhookHost::meta(serving),
        &raw const serving.meta
    ));
    assert_eq!(peryx_events::webhook::WebhookHost::now(serving), (serving.clock)());
}
