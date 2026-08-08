use std::sync::Arc;

use async_trait::async_trait;
use peryx_core::{Ecosystem, ShadowCandidate, ShadowSource};
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyAction, PolicyDecisionState};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::{MetaStore, NewPolicyDecision};

use crate::rate_limit::RouteClass;
use crate::serving::{DriverCapabilities, EcosystemDriver, ShadowDriver};
use crate::shadow::{ShadowPage, ShadowQuery};
use crate::state::{AppState, IndexDescription};

fn candidate(filename: &str) -> ShadowCandidate {
    ShadowCandidate {
        repository: "repository".to_owned(),
        project: "project".to_owned(),
        member: "hosted".to_owned(),
        source: ShadowSource::Hosted,
        filename: filename.to_owned(),
        digest: None,
        selected: true,
        reason: None,
    }
}

struct Driver;

impl ShadowDriver for Driver {
    fn shadowed_candidates(
        &self,
        _state: &crate::ServingState,
        _position: usize,
        _project: &str,
    ) -> Result<Vec<ShadowCandidate>, String> {
        Ok(vec![candidate("artifact.bin")])
    }
}

#[async_trait]
impl EcosystemDriver for Driver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }

    fn capabilities(&self) -> DriverCapabilities<'_> {
        DriverCapabilities {
            shadow: Some(self),
            ..DriverCapabilities::default()
        }
    }

    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    fn discover_index(&self, _index: IndexDescription, _base: Option<&crate::discovery::BaseUrl>) -> serde_json::Value {
        serde_json::Value::Null
    }
}

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, AppState::new(meta, blobs, 60, Vec::new()))
}

fn state_with_driver() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "repository".to_owned(),
            route: "repository".to_owned(),
            ecosystem: Ecosystem::new("example"),
            kind: IndexKind::Virtual {
                layers: Vec::new(),
                upload: None,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    state.register_ecosystem(Arc::new(Driver), Arc::new(peryx_search::EmptyIndexer));
    (dir, state)
}

fn record_decision(
    state: &AppState,
    action: PolicyAction,
    filename: Option<&str>,
    rule: Option<&str>,
    evaluated_at_unix: i64,
) {
    state
        .meta
        .record_policy_decision(NewPolicyDecision {
            repository: "repository",
            project: "project",
            version: None,
            filename,
            source: None,
            action,
            state: PolicyDecisionState::Deny,
            rule,
            reason: None,
            evaluated_at_unix,
            next_eligible_at_unix: None,
        })
        .unwrap();
}

#[test]
fn test_inspection_of_empty_shadow_page_reads_no_decisions() {
    let (_dir, state) = state();
    let inspection = state
        .inspect_shadowed(&ShadowQuery::new("missing".to_owned(), "project".to_owned()))
        .unwrap();

    assert!(inspection.candidates.is_empty());
    assert!(inspection.next_cursor.is_none());
}

#[test]
fn test_filename_decisions_keep_the_latest_matching_serve_record() {
    let (_dir, state) = state();
    record_decision(&state, PolicyAction::Serve, Some("artifact.bin"), Some("old"), 1);
    record_decision(&state, PolicyAction::Cached, Some("artifact.bin"), Some("latest"), 2);
    record_decision(&state, PolicyAction::Upload, Some("artifact.bin"), Some("upload"), 3);
    record_decision(&state, PolicyAction::Serve, None, Some("unnamed"), 4);
    record_decision(&state, PolicyAction::Serve, Some("other.bin"), Some("other"), 5);
    let page = ShadowPage {
        candidates: vec![candidate("artifact.bin")],
        next_cursor: None,
    };

    let decisions = state.filename_decisions("repository", "project", &page).unwrap();

    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions["artifact.bin"].rule.as_deref(), Some("latest"));
}

#[test]
fn test_inspect_shadowed_joins_driver_candidates_with_decisions() {
    let (_dir, state) = state_with_driver();
    record_decision(&state, PolicyAction::Serve, Some("artifact.bin"), Some("blocked"), 41);

    let inspection = state
        .inspect_shadowed(&ShadowQuery::new("repository".to_owned(), "project".to_owned()))
        .unwrap();

    assert_eq!(inspection.candidates.len(), 1);
    assert_eq!(
        inspection.candidates[0].decision.as_ref().unwrap().rule.as_deref(),
        Some("blocked")
    );
}

#[test]
fn test_inspect_shadowed_surfaces_a_decision_read_failure() {
    let fault = peryx_storage::meta::test_support::FaultStore::new();
    let dir = tempfile::tempdir().unwrap();
    let mut state = AppState::new(
        fault.reopen(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        vec![Index {
            name: "repository".to_owned(),
            route: "repository".to_owned(),
            ecosystem: Ecosystem::new("example"),
            kind: IndexKind::Virtual {
                layers: Vec::new(),
                upload: None,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    state.register_ecosystem(Arc::new(Driver), Arc::new(peryx_search::EmptyIndexer));
    fault.arm(0);

    assert!(matches!(
        state.inspect_shadowed(&ShadowQuery::new("repository".to_owned(), "project".to_owned())),
        Err(crate::shadow::ShadowQueryError::Store(_))
    ));
}
