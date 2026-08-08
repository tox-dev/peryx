use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use peryx_core::{NodeRole, TopologyConfig, TopologyMember, TopologyMode};
use peryx_identity::{
    ArtifactDigest, LdapBindMode, LdapLoginService, LdapProvider, LdapProviderSettings, OidcLoginProvider,
    OidcLoginService, OidcProviderSettings, ProviderId,
};
use peryx_storage::blob::{BlobStorage, BlobStore, Digest};
use peryx_storage::meta::{
    BackendLocation, BlobPlacementKey, BlobPlacementState, BlobPlacementTransition, DataCenterId, MetaStore,
};
use url::Url;

use super::{AppState, ServingState};
use crate::state::ControlPlane;

/// A well-formed lowercase-hex sha256 (`abcd` sixteen times) the wrapper accepts as an artifact digest.
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
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta, blobs, 60, Vec::new());
    state.set_availability_topology(topology);
    (dir, state)
}

fn local_state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, AppState::new(meta, blobs, 60, Vec::new()))
}

fn home_key(state: &ServingState, digest: &ArtifactDigest, dc: &str) -> BlobPlacementKey {
    BlobPlacementKey {
        digest: digest.clone(),
        backend: state.blobs.backend_id(),
        data_center: DataCenterId::new(dc).unwrap(),
        location: BackendLocation::for_digest(digest),
    }
}

#[test]
fn test_record_home_placement_verifies_the_home_datacenter() {
    let (_dir, state) = state_with(home_topology("home"));
    state.record_home_placement(DIGEST_HEX, 2_048, 3);
    let digest = ArtifactDigest::from_sha256(DIGEST_HEX).unwrap();
    let record = state
        .meta
        .blob_placement(&home_key(&state, &digest, "home"))
        .unwrap()
        .unwrap();
    assert_eq!(record.state, BlobPlacementState::Verified { size: 2_048 });
}

#[test]
fn test_record_home_placement_skips_a_node_without_a_local_datacenter() {
    let (_dir, state) = state_with(TopologyConfig::default());
    state.record_home_placement(DIGEST_HEX, 2_048, 3);
    let digest = ArtifactDigest::from_sha256(DIGEST_HEX).unwrap();
    assert!(
        state.meta.blob_placements(&digest).unwrap().is_empty(),
        "a node that resolves no local datacenter records nothing",
    );
}

#[test]
fn test_record_home_placement_swallows_a_malformed_digest() {
    let (_dir, state) = state_with(home_topology("home"));
    state.record_home_placement("not-a-sha256", 2_048, 3);
    let digest = ArtifactDigest::from_sha256(DIGEST_HEX).unwrap();
    assert!(
        state.meta.blob_placements(&digest).unwrap().is_empty(),
        "a malformed digest is swallowed and records nothing",
    );
}

#[test]
fn test_record_home_placement_swallows_an_invalid_datacenter_component() {
    let (_dir, state) = state_with(home_topology("bad\0dc"));
    state.record_home_placement(DIGEST_HEX, 2_048, 3);
    let digest = ArtifactDigest::from_sha256(DIGEST_HEX).unwrap();
    assert!(
        state.meta.blob_placements(&digest).unwrap().is_empty(),
        "a datacenter label the placement key rejects is swallowed",
    );
}

#[test]
fn test_record_home_placement_swallows_a_stale_fence() {
    let (_dir, state) = state_with(home_topology("home"));
    let digest = ArtifactDigest::from_sha256(DIGEST_HEX).unwrap();
    let key = home_key(&state, &digest, "home");
    state
        .meta
        .apply_blob_placement(&key, &BlobPlacementTransition::Stage, 5, 10)
        .unwrap();

    state.record_home_placement(DIGEST_HEX, 2_048, 2);

    let record = state.meta.blob_placement(&key).unwrap().unwrap();
    assert_eq!(
        record.state,
        BlobPlacementState::Pending,
        "a stale-fenced write is swallowed and leaves the staged record unchanged",
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
        _meta: &MetaStore,
        _expected: &[peryx_ha::ExpectedProducer],
        _query: &peryx_ha::CompletenessQuery,
    ) -> Result<peryx_ha::CompletenessReport, peryx_ha::CompletenessError> {
        Err(peryx_ha::CompletenessError)
    }
}

struct Membership;

#[async_trait]
impl peryx_ha::MembershipControl for Membership {
    async fn submit(
        &self,
        _command: peryx_ha::ControlCommand,
    ) -> Result<peryx_ha::CommandReceipt, peryx_ha::ControlError> {
        unreachable!()
    }
}

struct Reader;

#[async_trait]
impl peryx_ha::RemoteBlobReader for Reader {
    async fn read_through(
        &self,
        _meta: &MetaStore,
        _blobs: &BlobStorage,
        _digest: &Digest,
    ) -> Result<peryx_ha::ReadThroughOutcome, peryx_ha::ReadThroughError> {
        unreachable!()
    }
}

#[test]
fn test_process_metrics_render_registered_sources() {
    let (_dir, state) = state_with(TopologyConfig::default());
    state.register_prometheus(Arc::new(Metrics));
    let mut body = String::new();

    state.write_process_metrics(&mut body);

    assert_eq!(body, "metric 1\n");
}

#[tokio::test]
async fn test_local_state_has_no_distributed_runtime() {
    let (_dir, state) = local_state();

    assert!(state.is_ready(true).await);
    assert_eq!(state.availability_role(), NodeRole::Writer);
    assert!(state.write_acknowledger().is_none());
    assert!(state.analytics_completeness().is_none());
    assert!(state.dc_durability().is_none());
    assert!(state.control_plane().is_none());
    assert!(state.read_through().is_none());
}

#[test]
fn test_distributed_services_are_installed_once() {
    let (_dir, state) = state_with(TopologyConfig::default());
    let completeness: Arc<dyn peryx_ha::AnalyticsCompleteness> = Arc::new(Completeness);
    let control = Arc::new(ControlPlane::new(Arc::new(Membership), Arc::new(|| 0)));
    let reader: Arc<dyn peryx_ha::RemoteBlobReader> = Arc::new(Reader);

    state.set_analytics_completeness(completeness);
    state.set_control_plane(control.clone());
    state.set_read_through(reader);

    assert!(state.analytics_completeness().is_some());
    assert!(Arc::ptr_eq(state.control_plane().unwrap(), &control));
    assert!(state.read_through().is_some());
}

#[tokio::test]
async fn test_local_ownership_wrappers_keep_single_node_semantics() {
    let (_dir, state) = state_with(TopologyConfig::default());

    state.claim_first_publish_home("packages").await;

    assert_eq!(state.committed_authority_epoch("packages").await, 0);
    assert!(state.admit_authority_epoch("packages", 41).await);
    assert_eq!(state.cluster_term(), 0);
    assert_eq!(state.transfer_authority_home("packages", "west").await.unwrap(), None);
}

#[test]
fn test_login_services_and_session_sealer_install_by_provider_id() {
    let (_dir, mut state) = state_with(TopologyConfig::default());

    assert!(state.ldap_login("missing").is_none());
    assert!(state.oidc_login("missing").is_none());
    assert!(state.oidc_providers().is_empty());
    assert!(state.session_sealer().is_none());

    state.set_ldap_logins([LdapLoginService::new(
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
        state.meta.clone(),
        Vec::new(),
    )]);
    state.set_oidc_logins([OidcLoginService::new(
        OidcLoginProvider::new(OidcProviderSettings {
            id: ProviderId::new("browser").unwrap(),
            issuer: Url::parse("https://issuer.example").unwrap(),
            client_id: "peryx-web".to_owned(),
            client_secret: None,
            redirect_uri: Url::parse("https://registry.example/oidc/browser/callback").unwrap(),
            scopes: vec!["openid".to_owned()],
            subject_claim: "sub".to_owned(),
            display_name_claim: "name".to_owned(),
            groups_claim: None,
            clock_skew: Duration::from_mins(1),
            request_timeout: Duration::from_secs(5),
        })
        .unwrap(),
        state.meta.clone(),
        Vec::new(),
    )]);
    state.set_session_sealer(peryx_identity::SessionSealer::new(b"session-key"));

    assert!(state.ldap_login("directory").is_some());
    assert!(state.oidc_login("browser").is_some());
    assert_eq!(state.oidc_providers(), vec!["browser"]);
    assert!(state.session_sealer().is_some());
}

#[test]
fn test_webhook_host_borrows_runtime_store_and_clock() {
    use peryx_events::webhook::WebhookHost as _;

    let (_dir, state) = state_with(TopologyConfig::default());

    assert!(std::ptr::eq(state.webhooks(), &raw const state.webhooks));
    assert!(std::ptr::eq(state.meta(), &raw const state.meta));
    assert_eq!(state.now(), (state.clock)());
}

#[test]
fn test_app_describes_no_indexes_when_none_are_configured() {
    let (_dir, state) = local_state();

    assert!(state.describe_indexes().is_empty());
}
