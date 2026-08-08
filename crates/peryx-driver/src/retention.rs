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
    let generation = match meta.policy_input_generation(index) {
        Ok(generation) => generation,
        Err(error) => return Err(error.to_string()),
    };
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
///
/// # Panics
/// Panics if the caller bypasses retention capability resolution.
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
        let retention = driver
            .capabilities()
            .retention
            .expect("export preparation requires retention support");
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
#[path = "../tests/unit/retention/tests.rs"]
mod tests;
