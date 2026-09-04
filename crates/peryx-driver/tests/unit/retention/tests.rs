use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use peryx_policy::{
    RetentionClass, RetentionConfig, RetentionDecision, RetentionOutcome, RetentionPolicy, RetentionVisibility,
};
use peryx_storage::meta::MetaStore;

use super::{RetentionPlanError, RetentionQuery, decode_cursor, encode_cursor, plan};
use crate::serving::RetentionDriver;

#[derive(Default)]
struct StubDriver {
    decisions: Vec<RetentionDecision>,
    fail: Option<String>,
}

enum SnapshotViolation {
    Missing,
    DecisionFirst,
    Repeated,
}

struct InvalidDriver(SnapshotViolation);

impl RetentionDriver for InvalidDriver {
    fn validate_retention(&self, _policy: &RetentionPolicy) -> Result<(), String> {
        Ok(())
    }

    fn plan_retention(
        &self,
        scan: &crate::serving::RetentionScan<'_>,
        start: &mut dyn FnMut(peryx_policy::RetentionSummary) -> Result<(), String>,
        emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate_retention(scan.policy)?;
        let summary = peryx_policy::RetentionSummary {
            policy_version: scan.policy.version(),
            frontier: peryx_policy::RetentionFrontier::default(),
        };
        match self.0 {
            SnapshotViolation::Missing => Ok(()),
            SnapshotViolation::DecisionFirst => emit(decision("a")),
            SnapshotViolation::Repeated => {
                start(summary)?;
                start(summary)
            }
        }
    }
}

impl RetentionDriver for StubDriver {
    fn validate_retention(&self, _policy: &RetentionPolicy) -> Result<(), String> {
        Ok(())
    }

    fn plan_retention(
        &self,
        scan: &crate::serving::RetentionScan<'_>,
        start: &mut dyn FnMut(peryx_policy::RetentionSummary) -> Result<(), String>,
        emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate_retention(scan.policy)?;
        start(peryx_policy::RetentionSummary {
            policy_version: scan.policy.version(),
            frontier: peryx_policy::RetentionFrontier::default(),
        })?;
        for decision in self
            .decisions
            .iter()
            .skip(usize::try_from(scan.skip).unwrap_or(usize::MAX))
        {
            emit(decision.clone())?;
        }
        if let Some(reason) = &self.fail {
            return Err(reason.clone());
        }
        Ok(())
    }
}

/// A driver that builds each decision only as it emits it, so `built` counts expansions rather than
/// rows walked, and records the offset it was handed.
struct CountingDriver {
    total: u64,
    skip: AtomicU64,
    built: AtomicU64,
}

impl RetentionDriver for CountingDriver {
    fn validate_retention(&self, _policy: &RetentionPolicy) -> Result<(), String> {
        Ok(())
    }

    fn plan_retention(
        &self,
        scan: &crate::serving::RetentionScan<'_>,
        start: &mut dyn FnMut(peryx_policy::RetentionSummary) -> Result<(), String>,
        emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate_retention(scan.policy)?;
        self.skip.store(scan.skip, Ordering::Relaxed);
        start(peryx_policy::RetentionSummary {
            policy_version: scan.policy.version(),
            frontier: peryx_policy::RetentionFrontier::default(),
        })?;
        for index in scan.skip..self.total {
            self.built.fetch_add(1, Ordering::Relaxed);
            emit(decision(&index.to_string()))?;
        }
        Ok(())
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
    RetentionPolicy::compile(&RetentionConfig::default(), str::to_owned)
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
        ecosystem: "example",
        policy,
        now: None,
        after,
        limit,
        expect,
    };
    let page = plan(
        driver,
        meta,
        &query,
        &crate::ScanCancellation::new(),
        &mut |_| Ok(()),
        &mut |decision| {
            seen.push(decision.artifact.clone());
            Ok(())
        },
    )?;
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
fn test_plan_builds_only_the_decisions_a_deep_page_returns() {
    let (_dir, meta) = store();
    let driver = CountingDriver {
        total: 1_000,
        skip: AtomicU64::new(0),
        built: AtomicU64::new(0),
    };

    let (seen, page) = collect(&driver, &meta, &empty_policy(), 997, None, None).unwrap();

    assert_eq!(seen, vec!["997", "998", "999"]);
    assert_eq!(page.emitted, 3);
    assert_eq!(driver.skip.into_inner(), 997);
    assert_eq!(driver.built.into_inner(), 3);
}

#[test]
fn test_plan_uses_the_snapshot_that_produced_its_decisions() {
    let (_dir, meta) = store();
    seed_generation(&meta);

    let (_, page) = collect(&StubDriver::default(), &meta, &empty_policy(), 0, None, None).unwrap();

    assert_eq!(page.summary.frontier, peryx_policy::RetentionFrontier::default());
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
        ecosystem: "example",
        policy: &empty_policy(),
        now: None,
        after: 0,
        limit: None,
        expect: None,
    };

    let result = plan(
        &driver,
        &meta,
        &query,
        &crate::ScanCancellation::new(),
        &mut |_| Ok(()),
        &mut |_| Err("client hung up".to_owned()),
    );

    assert!(
        matches!(&result, Err(RetentionPlanError::Interrupted(reason)) if reason == "client hung up"),
        "{result:?}"
    );
}

#[test]
fn test_plan_reports_cooperative_cancellation() {
    let (_dir, meta) = store();
    let policy = empty_policy();
    let query = RetentionQuery {
        index: "alpha",
        ecosystem: "example",
        policy: &policy,
        now: None,
        after: 0,
        limit: None,
        expect: None,
    };
    let cancellation = crate::ScanCancellation::new();
    let mut seen = Vec::new();

    let result = plan(
        &StubDriver {
            decisions: vec![decision("a")],
            fail: None,
        },
        &meta,
        &query,
        &cancellation,
        &mut |_| Ok(()),
        &mut |decision| {
            cancellation.cancel();
            seen.push(decision.artifact.clone());
            Ok(())
        },
    );

    assert!(matches!(
        &result,
        Err(RetentionPlanError::Interrupted(reason)) if reason == "request cancelled"
    ));
    assert_eq!(seen, ["a".to_owned()]);
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

#[rstest::rstest]
#[case::missing(SnapshotViolation::Missing, "without opening a snapshot")]
#[case::decision_first(SnapshotViolation::DecisionFirst, "before opening a snapshot")]
#[case::repeated(SnapshotViolation::Repeated, "more than one snapshot")]
fn test_plan_rejects_an_invalid_snapshot_sequence(#[case] violation: SnapshotViolation, #[case] message: &str) {
    let (_dir, meta) = store();

    let result = collect(&InvalidDriver(violation), &meta, &empty_policy(), 0, None, None);

    assert!(
        matches!(&result, Err(RetentionPlanError::Store(reason)) if reason.contains(message)),
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
fn test_decode_cursor_rejects_an_unknown_version() {
    use base64::Engine as _;
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&serde_json::json!({
            "version": 2,
            "repository": "alpha",
            "ecosystem": "example",
            "evaluated_at": 42,
            "after": 4,
            "summary": {
                "policy_version": 7,
                "frontier": {"repository": 1, "catalog": 2, "policy": 3},
            },
        }))
        .unwrap(),
    );

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

    let resume = decode_cursor(&encode_cursor("alpha", "example", Some(42), 4, summary)).unwrap();

    assert_eq!(resume.repository, "alpha");
    assert_eq!(resume.ecosystem, "example");
    assert_eq!(resume.evaluated_at, Some(42));
    assert_eq!(resume.after, 4);
    assert_eq!(resume.expect, summary);
}

fn seed_generation(meta: &MetaStore) {
    meta.advance_policy_generation("alpha").unwrap();
}

fn export_lines(
    driver: &dyn RetentionDriver,
    meta: &MetaStore,
    after: u64,
    sink: &mut dyn FnMut(Bytes) -> Result<(), ()>,
) -> Result<(), RetentionPlanError> {
    let policy = empty_policy();
    let query = RetentionQuery {
        index: "alpha",
        ecosystem: "example",
        policy: &policy,
        now: None,
        after,
        limit: None,
        expect: None,
    };
    super::write_export(driver, meta, &query, &mut |_| Ok(()), sink)
}

#[test]
fn test_write_export_emits_a_header_then_one_decision_per_line() {
    let (_dir, meta) = store();
    seed_generation(&meta);
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
    assert_eq!(lines[0]["summary"]["frontier"]["policy"], 0);
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
    let gates = super::RetentionGates::new(1);
    let permit = gates.try_enter("alpha").unwrap();
    let export = super::RetentionExport {
        index: "alpha".to_owned(),
        ecosystem: "example".to_owned(),
        policy,
        now: None,
        after: 0,
        expect: None,
    };

    let (_, body) = super::export_body(driver, meta, export, permit).await.unwrap();
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
    let gates = super::RetentionGates::new(1);
    let permit = gates.try_enter("alpha").unwrap();
    let export = super::RetentionExport {
        index: "alpha".to_owned(),
        ecosystem: "example".to_owned(),
        policy,
        now: None,
        after: 0,
        expect: None,
    };

    let (_, body) = super::export_body(driver, meta, export, permit).await.unwrap();
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

struct PanickingDriver;

impl RetentionDriver for PanickingDriver {
    fn validate_retention(&self, _policy: &RetentionPolicy) -> Result<(), String> {
        Ok(())
    }

    fn plan_retention(
        &self,
        _scan: &crate::serving::RetentionScan<'_>,
        _start: &mut dyn FnMut(peryx_policy::RetentionSummary) -> Result<(), String>,
        _emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
    ) -> Result<(), String> {
        panic!("the plugin driver came apart");
    }
}

/// A driver that comes apart takes the worker with it, so nothing ever sends the summary the request
/// is waiting on. The wait has to end as an interruption rather than hanging on a sender that will
/// never write. A driver returning without a snapshot does not reach this: the export validates that
/// itself and forwards a store error through the channel.
#[tokio::test]
async fn test_export_body_ends_the_wait_when_the_worker_dies() {
    let (_dir, meta) = store();
    let driver: Arc<dyn RetentionDriver> = Arc::new(PanickingDriver);
    let gates = super::RetentionGates::new(1);
    let permit = gates.try_enter("alpha").unwrap();
    let export = super::RetentionExport {
        index: "alpha".to_owned(),
        ecosystem: "example".to_owned(),
        policy: empty_policy(),
        now: None,
        after: 0,
        expect: None,
    };

    let error = super::export_body(driver, meta, export, permit).await.unwrap_err();

    assert!(format!("{error:?}").contains("export worker stopped"), "{error:?}");
}

/// A driver that opens a second snapshot has already sent the first, so the request is answered and
/// the violation has to reach the caller through the body it is already streaming.
#[tokio::test]
async fn test_export_body_reports_a_second_snapshot_through_the_stream() {
    let (_dir, meta) = store();
    let driver: Arc<dyn RetentionDriver> = Arc::new(InvalidDriver(SnapshotViolation::Repeated));
    let gates = super::RetentionGates::new(1);
    let permit = gates.try_enter("alpha").unwrap();
    let export = super::RetentionExport {
        index: "alpha".to_owned(),
        ecosystem: "example".to_owned(),
        policy: empty_policy(),
        now: None,
        after: 0,
        expect: None,
    };

    let (_, body) = super::export_body(driver, meta, export, permit).await.unwrap();
    let error = axum::body::to_bytes(body, usize::MAX).await.unwrap_err();

    assert!(format!("{error:?}").contains("more than one snapshot"), "{error:?}");
}

/// Parks inside the plan until the test releases it, so the request can be abandoned first and the
/// summary send lands on a receiver that is already gone.
struct GatedDriver {
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    saw: tokio::sync::mpsc::UnboundedSender<String>,
}

impl RetentionDriver for GatedDriver {
    fn validate_retention(&self, _policy: &RetentionPolicy) -> Result<(), String> {
        Ok(())
    }

    fn plan_retention(
        &self,
        scan: &crate::serving::RetentionScan<'_>,
        start: &mut dyn FnMut(peryx_policy::RetentionSummary) -> Result<(), String>,
        _emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
    ) -> Result<(), String> {
        self.entered.send(()).unwrap();
        // Blocking is correct here: the plan already runs on the blocking pool.
        let release = self.release.lock().unwrap().take().expect("released once");
        release.recv().unwrap();
        let summary = peryx_policy::RetentionSummary {
            policy_version: scan.policy.version(),
            frontier: peryx_policy::RetentionFrontier::default(),
        };
        start(summary).inspect_err(|reason| self.saw.send(reason.clone()).unwrap())
    }
}

/// A client that abandons an export leaves nothing waiting for the summary, so the send that opens
/// the snapshot has nowhere to go. The worker has to learn the request is gone rather than carry on
/// producing a body no one will read.
///
/// The order is forced rather than raced: the driver parks inside the plan, the test drops the
/// request and waits for that drop to complete, and only then is the driver released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_export_body_tells_the_worker_when_the_request_is_gone() {
    let (_dir, meta) = store();
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (saw, mut saw_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let driver: Arc<dyn RetentionDriver> = Arc::new(GatedDriver {
        entered,
        release: std::sync::Mutex::new(Some(release_rx)),
        saw,
    });
    let gates = super::RetentionGates::new(1);
    let permit = gates.try_enter("alpha").unwrap();
    let export = super::RetentionExport {
        index: "alpha".to_owned(),
        ecosystem: "example".to_owned(),
        policy: empty_policy(),
        now: None,
        after: 0,
        expect: None,
    };
    let request = tokio::spawn(async move { super::export_body(driver, meta, export, permit).await.map(|_| ()) });

    entered_rx.recv().await.expect("the plan parked");
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();

    assert_eq!(saw_rx.recv().await.as_deref(), Some("export request gone"));
}
