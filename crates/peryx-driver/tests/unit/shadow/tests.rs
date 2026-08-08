use std::sync::Arc;

use async_trait::async_trait;
use peryx_core::Ecosystem;
use peryx_core::{ShadowCandidate, ShadowReason, ShadowSource};
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

use super::{DEFAULT_LIMIT, ShadowQuery, ShadowQueryError, paginate};
use crate::rate_limit::RouteClass;
use crate::serving::{DriverCapabilities, EcosystemDriver, ShadowDriver};
use crate::state::{AppState, IndexDescription};

fn candidate(filename: &str, member: &str, selected: bool) -> ShadowCandidate {
    ShadowCandidate {
        repository: "root/alpha".to_owned(),
        project: "flask".to_owned(),
        member: member.to_owned(),
        source: if selected {
            ShadowSource::Hosted
        } else {
            ShadowSource::Cached
        },
        filename: filename.to_owned(),
        digest: Some("sha256:abc".to_owned()),
        selected,
        reason: (!selected).then_some(ShadowReason::Precedence),
    }
}

fn query(limit: usize) -> ShadowQuery {
    ShadowQuery {
        limit,
        ..ShadowQuery::new("root/alpha".to_owned(), "flask".to_owned())
    }
}

#[test]
fn test_new_query_carries_the_default_page_size() {
    assert_eq!(ShadowQuery::new(String::new(), String::new()).limit, DEFAULT_LIMIT);
}

#[test]
fn test_validate_rejects_bad_limit_cursor_and_project() {
    assert_eq!(query(0).validate(), Err(ShadowQueryError::InvalidLimit));
    assert_eq!(query(101).validate(), Err(ShadowQueryError::InvalidLimit));
    assert_eq!(
        ShadowQuery {
            cursor: Some(String::new()),
            ..query(25)
        }
        .validate(),
        Err(ShadowQueryError::InvalidCursor)
    );
    assert_eq!(
        ShadowQuery {
            cursor: Some("x".repeat(1_025)),
            ..query(25)
        }
        .validate(),
        Err(ShadowQueryError::InvalidCursor)
    );
    assert_eq!(
        ShadowQuery {
            project: "p".repeat(513),
            ..query(25)
        }
        .validate(),
        Err(ShadowQueryError::ProjectTooLong)
    );
    assert_eq!(query(25).validate(), Ok(()));
}

#[test]
fn test_error_messages_are_actionable() {
    assert_eq!(
        ShadowQueryError::InvalidLimit.to_string(),
        "limit must be between 1 and 100"
    );
    assert_eq!(ShadowQueryError::InvalidCursor.to_string(), "invalid shadow cursor");
    assert_eq!(
        ShadowQueryError::ProjectTooLong.to_string(),
        "project filter exceeds 512 bytes"
    );
    assert_eq!(ShadowQueryError::Store("boom".to_owned()).to_string(), "boom");
}

#[test]
fn test_paginate_orders_by_filename_then_selection() {
    let candidates = vec![
        candidate("flask-1.0.bin", "alpha", false),
        candidate("flask-1.0.bin", "hosted", true),
        candidate("flask-2.0.bin", "hosted", true),
    ];

    let page = paginate(candidates, &query(25));

    assert_eq!(page.next_cursor, None);
    let rows: Vec<(&str, &str)> = page
        .candidates
        .iter()
        .map(|candidate| (candidate.filename.as_str(), candidate.member.as_str()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("flask-1.0.bin", "hosted"),
            ("flask-1.0.bin", "alpha"),
            ("flask-2.0.bin", "hosted")
        ],
        "the selected candidate leads its filename group"
    );
}

#[test]
fn test_paginate_cursor_resumes_after_the_last_row_and_stays_stable() {
    let candidates = vec![
        candidate("a.bin", "hosted", true),
        candidate("b.bin", "hosted", true),
        candidate("c.bin", "hosted", true),
    ];

    let first = paginate(candidates.clone(), &query(2));
    assert_eq!(
        first
            .candidates
            .iter()
            .map(|candidate| candidate.filename.clone())
            .collect::<Vec<_>>(),
        vec!["a.bin", "b.bin"]
    );
    let cursor = first.next_cursor.expect("a third row remains");

    let second = paginate(
        candidates,
        &ShadowQuery {
            cursor: Some(cursor),
            ..query(2)
        },
    );
    assert_eq!(
        second
            .candidates
            .iter()
            .map(|candidate| candidate.filename.clone())
            .collect::<Vec<_>>(),
        vec!["c.bin"],
        "the resumed page holds without skipping or duplicating a candidate"
    );
    assert_eq!(second.next_cursor, None);
}

struct Driver {
    error: bool,
}

impl ShadowDriver for Driver {
    fn shadowed_candidates(
        &self,
        _state: &crate::ServingState,
        _position: usize,
        _project: &str,
    ) -> Result<Vec<ShadowCandidate>, String> {
        if self.error {
            Err("shadow unavailable".to_owned())
        } else {
            Ok(vec![candidate("artifact.bin", "hosted", true)])
        }
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

fn app(error: bool) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "root/alpha".to_owned(),
            route: "root/alpha".to_owned(),
            ecosystem: Ecosystem::new("example"),
            kind: IndexKind::Virtual {
                layers: Vec::new(),
                upload: None,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    state.register_ecosystem(Arc::new(Driver { error }), Arc::new(peryx_search::EmptyIndexer));
    (dir, state)
}

#[test]
fn test_app_state_queries_driver_candidates() {
    let (_dir, state) = app(false);

    assert_eq!(state.query_shadowed(&query(25)).unwrap().candidates.len(), 1);
}

#[test]
fn test_app_state_surfaces_driver_shadow_failure() {
    let (_dir, state) = app(true);

    assert_eq!(
        state.query_shadowed(&query(25)),
        Err(ShadowQueryError::Store("shadow unavailable".to_owned()))
    );
}
