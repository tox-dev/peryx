//! The neutral retention-plan query the CLI and HTTP surfaces share.
//!
//! One request names a repository, a compiled [`RetentionPolicy`], and where in the plan to resume. The
//! query resolves the repository's ecosystem driver, streams its decisions in deterministic order, and
//! bounds what it holds: a page keeps at most `limit` decisions, an export holds one at a time. Every
//! result carries the plan's [identity](RetentionSummary), the policy version and the metadata
//! frontier, so a resumed page can prove its inputs have not shifted before it streams stale rows.
//!
//! A [cursor](encode_cursor) folds the resume offset and that identity into one opaque token. Presenting
//! it back both places the reader where it left off and, through [`plan`], rejects the resume when the
//! repository has changed underneath it. The whole path only reads metadata, so an interrupted plan
//! writes nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use peryx_policy::{RetentionDecision, RetentionFrontier, RetentionPolicy, RetentionSummary};
use peryx_storage::meta::MetaStore;
use serde::{Deserialize, Serialize};

use crate::serving::{EcosystemDriver, RetentionDriver};

/// Bounds how many plans one repository may compute at once, so a burst of full-scan previews on a
/// single repository cannot starve the rest. Shared across handlers; a permit is held for one request.
#[derive(Clone)]
pub struct RetentionGates {
    inner: Arc<Mutex<GateState>>,
}

struct GateState {
    inflight: HashMap<String, usize>,
    per_repository: usize,
}

/// A held slot in a repository's plan gate, released when the request drops it.
pub struct RetentionPermit {
    gates: RetentionGates,
    repository: String,
}

impl RetentionGates {
    /// Admit at most `per_repository` concurrent plans for any one repository.
    #[must_use]
    pub fn new(per_repository: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(GateState {
                inflight: HashMap::new(),
                per_repository,
            })),
        }
    }

    /// Claim a slot for `repository`, or `None` when it is already at its concurrency bound.
    #[must_use]
    pub fn try_enter(&self, repository: &str) -> Option<RetentionPermit> {
        let mut state = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let limit = state.per_repository;
        let count = state.inflight.entry(repository.to_owned()).or_insert(0);
        let admitted = *count < limit;
        if admitted {
            *count += 1;
        }
        drop(state);
        admitted.then(|| RetentionPermit {
            gates: self.clone(),
            repository: repository.to_owned(),
        })
    }
}

impl Drop for RetentionPermit {
    fn drop(&mut self) {
        let mut state = self
            .gates
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = state
            .inflight
            .get_mut(&self.repository)
            .expect("a held permit keeps its repository counted");
        *count -= 1;
        if *count == 0 {
            state.inflight.remove(&self.repository);
        }
    }
}

/// One retention-plan request against a resolved repository.
pub struct RetentionQuery<'a> {
    /// The repository (index) name whose records the driver adapts into candidates.
    pub index: &'a str,
    /// The compiled policy the planner evaluates each candidate against.
    pub policy: &'a RetentionPolicy,
    /// The evaluation clock an age rule ages against, or `None` to date nothing.
    pub now: Option<i64>,
    /// Decisions to skip: the exclusive offset a resumed page begins after.
    pub after: u64,
    /// One page's decision cap, or `None` to stream the whole plan for export.
    pub limit: Option<usize>,
    /// The plan identity a resumed page must still match, decoded from the cursor it carried. Absent on
    /// the first page, which establishes the identity rather than checking it.
    pub expect: Option<RetentionSummary>,
}

/// One page of a plan: its identity and, when more decisions remain, the cursor that resumes after it.
#[derive(Debug)]
pub struct RetentionPage {
    pub summary: RetentionSummary,
    /// The opaque resume token when the page filled its limit, or `None` when the plan is exhausted.
    pub next_cursor: Option<String>,
    /// How many decisions this page emitted, so a caller can report an empty tail without recounting.
    pub emitted: u64,
}

/// Why a retention plan did not complete.
#[derive(Debug)]
pub enum RetentionPlanError {
    /// The repository has no ecosystem driver that plans retention, so there is no plan to page.
    Unsupported,
    /// A resumed page's cursor names an identity the repository no longer has; streaming it would mix
    /// rows from two snapshots. Carries what the cursor expected and what the repository holds now.
    Stale {
        expected: RetentionSummary,
        current: RetentionSummary,
    },
    /// The sink stopped the stream: a disconnected export client or a failed write. Nothing was
    /// mutated, so the caller may restart from the cursor it last emitted.
    Interrupted(String),
    /// The metadata store could not be read or a record did not decode.
    Store(String),
}

impl std::fmt::Display for RetentionPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => f.write_str("the repository does not support retention planning"),
            Self::Stale { .. } => f.write_str("the plan cursor is stale: the repository changed since it was issued"),
            Self::Interrupted(reason) => write!(f, "the retention plan was interrupted: {reason}"),
            Self::Store(reason) => write!(f, "the retention plan could not read the store: {reason}"),
        }
    }
}

impl std::error::Error for RetentionPlanError {}

/// Stream one page (or the whole plan) of retention decisions to `emit`.
///
/// Reads the repository's current metadata frontier up front so the plan's identity is known before any
/// row streams: a resumed page whose `expect` no longer matches is rejected as [stale](RetentionPlanError::Stale)
/// rather than served from a shifted snapshot. `emit` receives each decision after the skip offset, up
/// to `limit`; returning an error from it stops the read-only scan at once.
///
/// # Errors
/// Returns [`RetentionPlanError`] when the repository plans no retention, the cursor is stale, `emit`
/// stopped the stream, or the store could not be read.
pub fn plan(
    driver: &dyn RetentionDriver,
    meta: &MetaStore,
    query: &RetentionQuery<'_>,
    emit: &mut dyn FnMut(&RetentionDecision) -> Result<(), String>,
) -> Result<RetentionPage, RetentionPlanError> {
    let summary = summary(meta, query.index, query.policy).map_err(RetentionPlanError::Store)?;
    if let Some(expected) = query.expect
        && expected != summary
    {
        return Err(RetentionPlanError::Stale {
            expected,
            current: summary,
        });
    }
    let mut seen = 0_u64;
    let mut emitted = 0_u64;
    let mut stop: Option<Stop> = None;
    let outcome = driver.plan_retention(meta, query.index, query.policy, query.now, &mut |decision| {
        if seen < query.after {
            seen += 1;
            return Ok(());
        }
        if query.limit.is_some_and(|limit| emitted >= limit as u64) {
            stop = Some(Stop::Full);
            return Err(HALT.to_owned());
        }
        seen += 1;
        emit(&decision).inspect_err(|reason| stop = Some(Stop::Interrupted(reason.clone())))?;
        emitted += 1;
        Ok(())
    });
    match (outcome, stop) {
        (_, Some(Stop::Interrupted(reason))) => Err(RetentionPlanError::Interrupted(reason)),
        (Ok(_), _) => Ok(RetentionPage {
            summary,
            next_cursor: None,
            emitted,
        }),
        (Err(_), Some(Stop::Full)) => Ok(RetentionPage {
            summary,
            next_cursor: Some(encode_cursor(query.after + emitted, summary)),
            emitted,
        }),
        (Err(reason), None) => Err(RetentionPlanError::Store(reason)),
    }
}

/// The plan's identity from the current metadata snapshot, without reading any candidate.
///
/// A resumed export reads this before streaming so it can send the identity as its first line and
/// reject a stale cursor before any row leaves.
///
/// # Errors
/// Returns the reason the store's policy-input generation could not be read.
pub fn summary(meta: &MetaStore, index: &str, policy: &RetentionPolicy) -> Result<RetentionSummary, String> {
    let generation = meta.policy_input_generation(index).map_err(|err| err.to_string())?;
    Ok(RetentionSummary {
        policy_version: policy.version(),
        frontier: RetentionFrontier {
            repository: generation.repository,
            catalog: generation.catalog,
            policy: generation.policy,
        },
    })
}

/// A retention export's first JSON Lines record: the plan identity, so a saved export retains the
/// policy version and metadata frontier a later apply must still match.
#[derive(Serialize)]
struct ExportHeader {
    summary: RetentionSummary,
}

/// One repository's whole-plan export: the query it streams and the identity its header carries. Owned,
/// so a blocking export task can hold it past the handler that built it.
pub struct RetentionExport {
    pub index: String,
    pub policy: RetentionPolicy,
    pub now: Option<i64>,
    pub after: u64,
    pub summary: RetentionSummary,
}

/// Serialize a whole plan to `sink` as JSON Lines: a header line carrying `summary`, then one decision
/// per line. `sink` returns `Err(())` to stop, which a disconnected reader signals.
///
/// # Errors
/// Returns [`RetentionPlanError`] when the sink stopped, the store could not be read, or the repository
/// plans no retention.
fn write_export(
    driver: &dyn RetentionDriver,
    meta: &MetaStore,
    query: &RetentionQuery<'_>,
    summary: RetentionSummary,
    sink: &mut dyn FnMut(Bytes) -> Result<(), ()>,
) -> Result<(), RetentionPlanError> {
    sink(line(&ExportHeader { summary }))
        .map_err(|()| RetentionPlanError::Interrupted("export client gone".to_owned()))?;
    plan(driver, meta, query, &mut |decision| {
        sink(line(decision)).map_err(|()| "export client disconnected".to_owned())
    })
    .map(drop)
}

/// A JSON Lines record: one compact JSON value and a trailing newline.
fn line(value: &impl Serialize) -> Bytes {
    let mut bytes = serde_json::to_vec(value).expect("a plan record always serializes");
    bytes.push(b'\n');
    Bytes::from(bytes)
}

/// Stream a whole plan as JSON Lines to an HTTP body.
///
/// The scan runs on a blocking task feeding a bounded channel, so a slow reader backpressures the scan
/// and a disconnected reader stops it. `permit` rides along so the repository's concurrency slot stays
/// held for the stream's whole life. The export's `summary` is the
/// identity the caller already read and validated the cursor against; the header carries it first.
pub fn export_body(
    driver: Arc<dyn EcosystemDriver>,
    meta: MetaStore,
    export: RetentionExport,
    permit: RetentionPermit,
) -> Body {
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(8);
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let query = RetentionQuery {
            index: &export.index,
            policy: &export.policy,
            now: export.now,
            after: export.after,
            limit: None,
            expect: None,
        };
        let Some(retention) = driver.capabilities().retention else {
            let _ = tx.blocking_send(Err(std::io::Error::other("retention capability unavailable")));
            return;
        };
        let result = write_export(retention, &meta, &query, export.summary, &mut |bytes| {
            tx.blocking_send(Ok(bytes)).map_err(drop)
        });
        // A store failure mid-stream poisons the body so the reader sees a truncated transfer rather
        // than a plan it might mistake for complete. A disconnected reader already closed the channel,
        // so this send is a harmless no-op.
        if let Err(RetentionPlanError::Store(reason)) = result {
            let _ = tx.blocking_send(Err(std::io::Error::other(reason)));
        }
    });
    Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    }))
}

/// The sentinel a filled page returns from the sink so the driver's scan aborts; the outer match reads
/// [`Stop`], never this string.
const HALT: &str = "retention plan page is full";

enum Stop {
    Full,
    Interrupted(String),
}

/// A resume offset paired with the plan identity it belongs to, decoded from a cursor.
#[derive(Debug)]
pub struct RetentionResume {
    pub after: u64,
    pub expect: RetentionSummary,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    after: u64,
    summary: RetentionSummary,
}

/// Fold a resume offset and plan identity into one opaque, URL-safe token.
///
/// # Panics
/// Never in practice: a cursor is a small fixed structure that always serializes.
#[must_use]
pub fn encode_cursor(after: u64, summary: RetentionSummary) -> String {
    let bytes = serde_json::to_vec(&Cursor { after, summary }).expect("a plan cursor always serializes");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode a cursor back into its resume offset and expected plan identity.
///
/// # Errors
/// Returns the reason a token is not a cursor this server issued.
pub fn decode_cursor(cursor: &str) -> Result<RetentionResume, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| "invalid retention plan cursor".to_owned())?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| "invalid retention plan cursor".to_owned())?;
    Ok(RetentionResume {
        after: cursor.after,
        expect: cursor.summary,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use bytes::Bytes;
    use peryx_core::Ecosystem;
    use peryx_policy::{
        RetentionClass, RetentionConfig, RetentionDecision, RetentionOutcome, RetentionPolicy, RetentionVisibility,
    };
    use peryx_storage::meta::MetaStore;

    use super::{RetentionPlanError, RetentionQuery, decode_cursor, encode_cursor, plan};
    use crate::serving::{DriverCapabilities, EcosystemDriver, RetentionDriver};

    #[derive(Default)]
    struct StubDriver {
        decisions: Vec<RetentionDecision>,
        /// A store error the driver raises mid-scan instead of finishing, so the store-failure branch
        /// is reachable without a broken store.
        fail: Option<String>,
    }

    #[async_trait]
    impl EcosystemDriver for StubDriver {
        fn ecosystem(&self) -> Ecosystem {
            Ecosystem::new("example")
        }

        fn classify_route(&self, _path: &str) -> crate::rate_limit::RouteClass {
            crate::rate_limit::RouteClass::Listing
        }

        fn discover_index(
            &self,
            _index: crate::state::IndexDescription,
            _base: Option<&crate::discovery::BaseUrl>,
        ) -> serde_json::Value {
            serde_json::Value::Null
        }

        fn capabilities(&self) -> DriverCapabilities<'_> {
            DriverCapabilities {
                retention: Some(self),
                ..DriverCapabilities::default()
            }
        }
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

    /// A minimal index description, so a test can exercise the stub's discovery surface. Its fields do
    /// not matter: the stub ignores them.
    fn index_description() -> crate::state::IndexDescription {
        crate::state::IndexDescription {
            name: "demo".to_owned(),
            route: "demo".to_owned(),
            ecosystem: "alpha",
            kind: "hosted",
            layers: Vec::new(),
            precedence: Vec::new(),
            uploads: false,
            volatile_deletes: false,
            upload_to: None,
            upstream: None,
            hosted: None,
        }
    }

    #[test]
    fn test_stub_driver_answers_the_required_trait_surface() {
        let driver = StubDriver::default();

        assert_eq!(driver.ecosystem(), Ecosystem::new("example"));
        assert!(matches!(
            driver.classify_route("/x"),
            crate::rate_limit::RouteClass::Listing
        ));
        assert!(driver.discover_index(index_description(), None).is_null());
    }

    /// A driver that implements only the required serving surface.
    struct DefaultDriver;

    #[async_trait]
    impl EcosystemDriver for DefaultDriver {
        fn ecosystem(&self) -> Ecosystem {
            Ecosystem::new("example")
        }

        fn classify_route(&self, _path: &str) -> crate::rate_limit::RouteClass {
            crate::rate_limit::RouteClass::Listing
        }

        fn discover_index(
            &self,
            _index: crate::state::IndexDescription,
            _base: Option<&crate::discovery::BaseUrl>,
        ) -> serde_json::Value {
            serde_json::Value::Null
        }
    }

    #[test]
    fn test_driver_without_retention_exposes_no_capability() {
        let driver = DefaultDriver;
        assert_eq!(driver.ecosystem(), Ecosystem::new("example"));
        assert!(matches!(
            driver.classify_route("/x"),
            crate::rate_limit::RouteClass::Listing
        ));
        assert!(driver.discover_index(index_description(), None).is_null());

        assert!(driver.capabilities().retention.is_none());
    }

    fn decision(artifact: &str) -> RetentionDecision {
        RetentionDecision {
            project: "demo".to_owned(),
            version: Some("1.0".to_owned()),
            artifact: artifact.to_owned(),
            digest: format!("sha-{artifact}"),
            class: RetentionClass::Hosted,
            visibility: RetentionVisibility::Active,
            source: None,
            bytes: 10,
            outcome: RetentionOutcome::Remove,
            rule: Some("project-prefix"),
            retained_alternatives: Vec::new(),
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
        let driver: Arc<dyn EcosystemDriver> = Arc::new(StubDriver {
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
        let driver: Arc<dyn EcosystemDriver> = Arc::new(StubDriver {
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

        // Dropping one of two leaves the repository counted; dropping the last removes its entry.
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
        assert!(RetentionPlanError::Unsupported.to_string().contains("does not support"));
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
}
