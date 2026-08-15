use std::sync::Arc;

use bytes::Bytes;
use peryx_policy::{
    RetentionClass, RetentionConfig, RetentionDecision, RetentionOutcome, RetentionPolicy, RetentionVisibility,
};
use peryx_storage::meta::MetaStore;

use super::{RetentionPlanError, RetentionQuery, decode_cursor, encode_cursor, plan, summary};
use crate::serving::RetentionDriver;

#[derive(Default)]
struct StubDriver {
    decisions: Vec<RetentionDecision>,
    fail: Option<String>,
}

impl RetentionDriver for StubDriver {
    fn plan_retention(
        &self,
        _meta: &MetaStore,
        _index: &str,
        policy: &RetentionPolicy,
        _now: Option<i64>,
        emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
    ) -> Result<peryx_policy::RetentionSummary, String> {
        for decision in &self.decisions {
            emit(decision.clone())?;
        }
        if let Some(reason) = &self.fail {
            return Err(reason.clone());
        }
        Ok(peryx_policy::RetentionSummary {
            policy_version: policy.version(),
            frontier: peryx_policy::RetentionFrontier::default(),
        })
    }
}

fn decision(artifact: &str) -> RetentionDecision {
    RetentionDecision {
        resource: "demo".to_owned(),
        group: Some("1.0".to_owned()),
        artifact: artifact.to_owned(),
        digest: format!("sha-{artifact}"),
        class: RetentionClass::Hosted,
        visibility: RetentionVisibility::Active,
        source: None,
        bytes: 10,
        outcome: RetentionOutcome::Remove,
        rule: Some("resource-prefix"),
        retained_groups: Vec::new(),
    }
}

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn empty_policy() -> RetentionPolicy {
    RetentionPolicy::compile(&RetentionConfig::default())
}

#[test]
fn test_summary_surfaces_a_metadata_read_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.redb");
    drop(redb::Database::create(&path).unwrap());
    let store = MetaStore::open_existing(path).unwrap();

    assert!(summary(&store, "alpha", &empty_policy()).is_err());
}

fn collect(
    driver: &dyn RetentionDriver,
    meta: &MetaStore,
    policy: &RetentionPolicy,
    after: u64,
    limit: Option<usize>,
    expect: Option<peryx_policy::RetentionSummary>,
) -> Result<(Vec<String>, super::RetentionPage), RetentionPlanError> {
    let mut seen = Vec::new();
    let query = RetentionQuery {
        index: "alpha",
        policy,
        now: None,
        after,
        limit,
        expect,
    };
    let page = plan(driver, meta, &query, &mut |decision| {
        seen.push(decision.artifact.clone());
        Ok(())
    })?;
    Ok((seen, page))
}

#[test]
fn test_plan_streams_every_decision_for_an_unbounded_export() {
    let (_dir, meta) = store();
    let driver = StubDriver {
        decisions: vec![decision("a"), decision("b"), decision("c")],
        fail: None,
    };

    let (seen, page) = collect(&driver, &meta, &empty_policy(), 0, None, None).unwrap();

    assert_eq!(seen, vec!["a", "b", "c"]);
    assert_eq!(page.emitted, 3);
    assert!(page.next_cursor.is_none());
}

#[test]
fn test_plan_pages_and_resumes_through_the_cursor() {
    let (_dir, meta) = store();
    let policy = empty_policy();
    let driver = StubDriver {
        decisions: vec![
            decision("a"),
            decision("b"),
            decision("c"),
            decision("d"),
            decision("e"),
        ],
        fail: None,
    };

    let (first, page) = collect(&driver, &meta, &policy, 0, Some(2), None).unwrap();
    assert_eq!(first, vec!["a", "b"]);
    let cursor = page.next_cursor.expect("a full page carries a cursor");

    let resume = decode_cursor(&cursor).unwrap();
    assert_eq!(resume.after, 2);
    let (second, page) = collect(&driver, &meta, &policy, resume.after, Some(2), Some(resume.expect)).unwrap();
    assert_eq!(second, vec!["c", "d"]);
    let cursor = page.next_cursor.expect("a full page carries a cursor");

    let resume = decode_cursor(&cursor).unwrap();
    let (third, page) = collect(&driver, &meta, &policy, resume.after, Some(2), Some(resume.expect)).unwrap();
    assert_eq!(third, vec!["e"]);
    assert!(page.next_cursor.is_none(), "the exhausted tail carries no cursor");
}

#[test]
fn test_plan_returns_an_empty_tail_past_the_end() {
    let (_dir, meta) = store();
    let driver = StubDriver {
        decisions: vec![decision("a")],
        fail: None,
    };

    let (seen, page) = collect(&driver, &meta, &empty_policy(), 5, Some(2), None).unwrap();

    assert!(seen.is_empty());
    assert_eq!(page.emitted, 0);
    assert!(page.next_cursor.is_none());
}

#[test]
fn test_plan_rejects_a_cursor_whose_identity_no_longer_matches() {
    let (_dir, meta) = store();
    let driver = StubDriver {
        decisions: vec![decision("a")],
        fail: None,
    };
    let stale = peryx_policy::RetentionSummary {
        policy_version: 999,
        frontier: peryx_policy::RetentionFrontier::default(),
    };

    let result = collect(&driver, &meta, &empty_policy(), 0, None, Some(stale));

    assert!(
        matches!(&result, Err(RetentionPlanError::Stale { expected, current })
            if expected.policy_version == 999 && current.policy_version != 999),
        "{result:?}"
    );
}

#[test]
fn test_plan_reports_interruption_when_the_sink_stops() {
    let (_dir, meta) = store();
    let driver = StubDriver {
        decisions: vec![decision("a"), decision("b")],
        fail: None,
    };
    let query = RetentionQuery {
        index: "alpha",
        policy: &empty_policy(),
        now: None,
        after: 0,
        limit: None,
        expect: None,
    };

    let result = plan(&driver, &meta, &query, &mut |_| Err("client hung up".to_owned()));

    assert!(
        matches!(&result, Err(RetentionPlanError::Interrupted(reason)) if reason == "client hung up"),
        "{result:?}"
    );
}

#[test]
fn test_plan_surfaces_a_store_failure_the_driver_raised() {
    let (_dir, meta) = store();
    let driver = StubDriver {
        decisions: vec![decision("a")],
        fail: Some("meta read failed".to_owned()),
    };

    let result = collect(&driver, &meta, &empty_policy(), 0, None, None);

    assert!(
        matches!(&result, Err(RetentionPlanError::Store(reason)) if reason == "meta read failed"),
        "{result:?}"
    );
}

#[test]
fn test_decode_cursor_rejects_a_non_base64_token() {
    assert_eq!(
        decode_cursor("not base64!!").unwrap_err(),
        "invalid retention plan cursor"
    );
}

#[test]
fn test_decode_cursor_rejects_base64_that_is_not_a_cursor() {
    use base64::Engine as _;
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
    assert_eq!(decode_cursor(&token).unwrap_err(), "invalid retention plan cursor");
}

#[test]
fn test_encode_cursor_round_trips_offset_and_identity() {
    let summary = peryx_policy::RetentionSummary {
        policy_version: 7,
        frontier: peryx_policy::RetentionFrontier {
            repository: 1,
            catalog: 2,
            policy: 3,
        },
    };

    let resume = decode_cursor(&encode_cursor(4, summary)).unwrap();

    assert_eq!(resume.after, 4);
    assert_eq!(resume.expect, summary);
}

fn seed_generation(meta: &MetaStore) {
    meta.advance_policy_generation("alpha").unwrap();
}

#[test]
fn test_summary_reads_the_policy_version_and_metadata_frontier() {
    let (_dir, meta) = store();
    seed_generation(&meta);
    let policy = empty_policy();

    let summary = super::summary(&meta, "alpha", &policy).unwrap();

    assert_eq!(summary.policy_version, policy.version());
    assert_eq!(summary.frontier.policy, 1);
}

fn export_lines(
    driver: &dyn RetentionDriver,
    meta: &MetaStore,
    after: u64,
    sink: &mut dyn FnMut(Bytes) -> Result<(), ()>,
) -> Result<(), RetentionPlanError> {
    let policy = empty_policy();
    let summary = super::summary(meta, "alpha", &policy).unwrap();
    let query = RetentionQuery {
        index: "alpha",
        policy: &policy,
        now: None,
        after,
        limit: None,
        expect: None,
    };
    super::write_export(driver, meta, &query, summary, sink)
}

#[test]
fn test_write_export_emits_a_header_then_one_decision_per_line() {
    let (_dir, meta) = store();
    let driver = StubDriver {
        decisions: vec![decision("a"), decision("b")],
        fail: None,
    };
    let mut lines: Vec<serde_json::Value> = Vec::new();

    export_lines(&driver, &meta, 0, &mut |bytes: Bytes| {
        lines.push(serde_json::from_slice(&bytes).unwrap());
        Ok(())
    })
    .unwrap();

    assert_eq!(lines.len(), 3);
    assert!(
        lines[0].get("summary").is_some(),
        "the first line carries the plan identity"
    );
    assert_eq!(lines[1]["artifact"], "a");
    assert_eq!(lines[2]["artifact"], "b");
}

#[test]
fn test_write_export_skips_past_the_resume_offset() {
    let (_dir, meta) = store();
    let driver = StubDriver {
        decisions: vec![decision("a"), decision("b"), decision("c")],
        fail: None,
    };
    let mut artifacts: Vec<String> = Vec::new();

    export_lines(&driver, &meta, 2, &mut |bytes: Bytes| {
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        if let Some(artifact) = value.get("artifact") {
            artifacts.push(artifact.as_str().unwrap().to_owned());
        }
        Ok(())
    })
    .unwrap();

    assert_eq!(artifacts, vec!["c"]);
}

#[test]
fn test_write_export_stops_when_the_header_sink_is_gone() {
    let (_dir, meta) = store();
    let driver = StubDriver {
        decisions: vec![decision("a")],
        fail: None,
    };

    let result = export_lines(&driver, &meta, 0, &mut |_| Err(()));

    assert!(matches!(result, Err(RetentionPlanError::Interrupted(_))));
}

#[test]
fn test_write_export_stops_when_the_reader_drops_mid_stream() {
    let (_dir, meta) = store();
    let driver = StubDriver {
        decisions: vec![decision("a"), decision("b")],
        fail: None,
    };
    let mut sent = 0_u32;

    let result = export_lines(&driver, &meta, 0, &mut |_| {
        sent += 1;
        if sent > 1 { Err(()) } else { Ok(()) }
    });

    assert!(matches!(result, Err(RetentionPlanError::Interrupted(_))));
    assert_eq!(
        sent, 2,
        "the header and one decision were attempted before the reader dropped"
    );
}

#[test]
fn test_write_export_surfaces_a_store_failure() {
    let (_dir, meta) = store();
    let driver = StubDriver {
        decisions: vec![decision("a")],
        fail: Some("meta read failed".to_owned()),
    };

    let result = export_lines(&driver, &meta, 0, &mut |_| Ok(()));

    assert!(matches!(result, Err(RetentionPlanError::Store(_))));
}

#[tokio::test]
async fn test_export_body_streams_the_whole_plan() {
    let (_dir, meta) = store();
    let driver: Arc<dyn RetentionDriver> = Arc::new(StubDriver {
        decisions: vec![decision("a"), decision("b")],
        fail: None,
    });
    let policy = empty_policy();
    let summary = super::summary(&meta, "alpha", &policy).unwrap();
    let gates = super::RetentionGates::new(1);
    let permit = gates.try_enter("alpha").unwrap();
    let export = super::RetentionExport {
        index: "alpha".to_owned(),
        policy,
        now: None,
        after: 0,
        summary,
    };

    let body = super::export_body(driver, meta, export, permit);
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();

    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains("summary"));
    assert!(lines[1].contains("\"artifact\":\"a\""));
}

#[tokio::test]
async fn test_export_body_poisons_the_stream_on_a_store_failure() {
    let (_dir, meta) = store();
    let driver: Arc<dyn RetentionDriver> = Arc::new(StubDriver {
        decisions: vec![decision("a")],
        fail: Some("meta read failed".to_owned()),
    });
    let policy = empty_policy();
    let summary = super::summary(&meta, "alpha", &policy).unwrap();
    let gates = super::RetentionGates::new(1);
    let permit = gates.try_enter("alpha").unwrap();
    let export = super::RetentionExport {
        index: "alpha".to_owned(),
        policy,
        now: None,
        after: 0,
        summary,
    };

    let body = super::export_body(driver, meta, export, permit);
    let collected = axum::body::to_bytes(body, usize::MAX).await;

    assert!(collected.is_err(), "a store failure poisons the body");
}

#[test]
fn test_retention_gates_bound_concurrency_per_repository_and_release_on_drop() {
    let gates = super::RetentionGates::new(2);

    let first = gates.try_enter("alpha").expect("first plan is admitted");
    let second = gates.try_enter("alpha").expect("a second is admitted up to the bound");
    assert!(gates.try_enter("alpha").is_none(), "a third concurrent plan is refused");
    let other = gates.try_enter("beta").expect("a different repository is independent");

    drop(first);
    let reused = gates.try_enter("alpha").expect("the freed slot is available again");

    drop(second);
    drop(reused);
    drop(other);
    assert!(
        gates.try_enter("alpha").is_some(),
        "a fully released repository admits again"
    );
}

#[test]
fn test_plan_error_display_names_each_failure() {
    let stale = RetentionPlanError::Stale {
        expected: peryx_policy::RetentionSummary {
            policy_version: 1,
            frontier: peryx_policy::RetentionFrontier::default(),
        },
        current: peryx_policy::RetentionSummary {
            policy_version: 2,
            frontier: peryx_policy::RetentionFrontier::default(),
        },
    };
    assert!(stale.to_string().contains("stale"));
    assert!(
        RetentionPlanError::Interrupted("gone".to_owned())
            .to_string()
            .contains("gone")
    );
    assert!(
        RetentionPlanError::Store("boom".to_owned())
            .to_string()
            .contains("boom")
    );
}
