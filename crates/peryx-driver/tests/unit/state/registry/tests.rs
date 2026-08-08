use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use peryx_core::{Ecosystem, Lexicon};
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_search::EmptyIndexer;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::AppState;
use crate::rate_limit::RouteClass;
use crate::serving::{
    DriverCapabilities, EcosystemDriver, IndexSummary, IndexSummaryDriver, MaintenanceCapabilities, MaintenanceDriver,
    MirrorDriver, MirrorRequest, ReplicatedApplyDriver, RouteMount,
};
use crate::state::{IndexDescription, ViewBlock};

struct Driver;

struct BareDriver;

#[async_trait]
impl EcosystemDriver for BareDriver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("bare")
    }

    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    fn discover_index(&self, _index: IndexDescription, _base: Option<&crate::discovery::BaseUrl>) -> serde_json::Value {
        serde_json::Value::Null
    }
}

impl IndexSummaryDriver for Driver {
    fn summarize_indexes(
        &self,
        _meta: &MetaStore,
        index_names: &[String],
        _recent_limit: usize,
    ) -> Result<std::collections::HashMap<String, IndexSummary>, String> {
        Ok(index_names
            .iter()
            .map(|name| (name.clone(), IndexSummary::default()))
            .collect())
    }
}

#[async_trait]
impl EcosystemDriver for Driver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }

    fn capabilities(&self) -> DriverCapabilities<'_> {
        DriverCapabilities {
            index_summary: Some(self),
            ..DriverCapabilities::default()
        }
    }

    fn mount(&self) -> RouteMount {
        RouteMount::Absolute(&["/artifacts"])
    }

    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    fn discover_index(&self, _index: IndexDescription, _base: Option<&crate::discovery::BaseUrl>) -> serde_json::Value {
        serde_json::Value::Null
    }

    async fn serve(&self, _state: Arc<crate::ServingState>, _request: Request) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

impl MaintenanceDriver for Driver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }

    fn maintenance_capabilities(&self) -> MaintenanceCapabilities<'_> {
        MaintenanceCapabilities::default()
    }
}

impl ReplicatedApplyDriver for Driver {
    fn apply_replicated_changes(
        &self,
        _state: &crate::ServingState,
        _changed_keys: &[String],
    ) -> Result<(), ViewBlock> {
        Ok(())
    }
}

#[async_trait]
impl MirrorDriver for Driver {
    async fn mirror(
        &self,
        _state: Arc<AppState>,
        _request: MirrorRequest<'_>,
        output: &mut (dyn std::io::Write + Send),
    ) -> Result<(), String> {
        output.write_all(b"mirror").map_err(|error| error.to_string())
    }
}

struct Acknowledger;

#[async_trait]
impl peryx_ha::WriteAcknowledger for Acknowledger {
    async fn acknowledge(&self, _request: peryx_ha::WriteAckRequest<'_>) -> peryx_ha::DcAck {
        peryx_ha::DcAck::Unknown
    }
}

struct Exchange;

#[async_trait]
impl peryx_identity::IdentityExchange for Exchange {
    fn audience(&self) -> &'static str {
        "peryx"
    }

    async fn exchange(
        &self,
        _token: &str,
        _now: i64,
    ) -> Result<peryx_identity::ExchangedToken, peryx_identity::ExchangeError> {
        Err(peryx_identity::ExchangeError::Configuration)
    }
}

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, AppState::new(meta, blobs, 60, Vec::new()))
}

#[test]
fn test_registry_installs_neutral_driver_capabilities() {
    let (_dir, mut state) = state();
    let driver = Arc::new(Driver);
    let ecosystem = Ecosystem::new("example");

    state.register_lexicon(ecosystem, &Lexicon::NEUTRAL);
    state.register_ecosystem(driver.clone(), Arc::new(EmptyIndexer));
    state.register_maintenance_driver(ecosystem, driver.clone());
    state.register_replicated_apply_driver(ecosystem, driver.clone());
    state.register_mirror_driver(ecosystem, driver);

    assert!(state.has_any_driver());
    assert!(state.driver_for(ecosystem).is_some());
    assert!(state.driver_for_name("example").is_some());
    assert_eq!(state.drivers().count(), 1);
    assert_eq!(state.maintenance_drivers().count(), 1);
    assert_eq!(state.replicated_apply_drivers().count(), 1);
    assert!(state.mirror_driver_for(ecosystem).is_some());
    assert!(state.absolute_driver_for_path("/artifacts/item").is_some());
    assert_eq!(
        state.absolute_mounts().map(|(prefix, _)| prefix).collect::<Vec<_>>(),
        vec!["/artifacts"]
    );
    assert!(std::ptr::eq(state.indexer_ctx().meta, &raw const state.meta));
    assert_eq!(state.search_ctx().lexicons.get(ecosystem).server, "index");
}

#[test]
fn test_registry_runtime_settings_replace_defaults() {
    let (_dir, mut state) = state();

    state.set_openapi("{\"openapi\":\"3.1.0\"}");
    state.set_token_realm(peryx_identity::Signer::new(b"signing-key", "peryx"), 41);
    state.set_trusted_publishing(Exchange);
    state.set_availability_topology(peryx_core::TopologyConfig::default());
    state.set_write_acknowledger(Arc::new(Acknowledger));
    state.set_availability_role(peryx_core::NodeRole::Replica);

    assert_eq!(state.openapi(), "{\"openapi\":\"3.1.0\"}");
    assert!(state.signer.is_some());
    assert!(state.trusted_publishing.is_some());
    assert_eq!(state.token_ttl_secs, 41);
    assert_eq!(state.availability_role(), peryx_core::NodeRole::Replica);
    assert!(state.write_acknowledger().is_some());
}

#[test]
fn test_index_summaries_skip_missing_drivers() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let state = AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "packages".to_owned(),
            route: "packages".to_owned(),
            ecosystem: Ecosystem::new("missing"),
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );

    assert!(state.index_summaries(5).is_empty());
}

#[test]
fn test_index_summaries_skip_drivers_without_the_capability() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "packages".to_owned(),
            route: "packages".to_owned(),
            ecosystem: Ecosystem::new("bare"),
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    state.register_ecosystem(Arc::new(BareDriver), Arc::new(EmptyIndexer));

    assert!(state.index_summaries(5).is_empty());
}

#[test]
fn test_index_summaries_dispatch_to_matching_capability() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "packages".to_owned(),
            route: "packages".to_owned(),
            ecosystem: Ecosystem::new("example"),
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    state.register_ecosystem(Arc::new(Driver), Arc::new(EmptyIndexer));

    assert!(state.index_summaries(5).contains_key("packages"));
}
