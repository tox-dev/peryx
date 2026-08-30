//! Plans retention against a stable repository snapshot.
//!
//! A query names a repository, a compiled [`RetentionPolicy`], and a resume position. Decisions stream
//! in deterministic order. Pages retain at most `limit` decisions; exports retain one at a time. Each
//! result carries its [identity](RetentionSummary) so resumed reads reject changed inputs.
//!
//! A [cursor](encode_cursor) folds the repository, ecosystem, evaluation instant, resume offset, and
//! identity into one opaque token. Presenting it back both places the reader where it left off and,
//! through [`plan`], rejects a changed repository. The whole path only reads metadata, so an interrupted
//! plan writes nothing.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use peryx_policy::{RetentionDecision, RetentionPolicy, RetentionSummary};
use peryx_storage::meta::MetaStore;
use serde::{Deserialize, Serialize};

use crate::ScanCancellation;
use crate::serving::{RetentionDriver, RetentionScan};

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
    #[must_use]
    pub fn new(per_repository: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(GateState {
                inflight: HashMap::new(),
                per_repository,
            })),
        }
    }

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
    /// The ecosystem that owns the repository.
    pub ecosystem: &'a str,
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

/// Streams one retention plan after validating its metadata frontier.
///
/// A mismatched `expect` rejects the resume as [stale](RetentionPlanError::Stale). `start` receives the
/// snapshot identity before `emit` receives up to `limit` decisions after the skip offset. Either
/// callback may stop the read-only scan by returning an error.
///
/// # Errors
/// Returns [`RetentionPlanError`] when the repository plans no retention, the cursor is stale, a
/// callback stopped the stream, or the store could not be read.
pub fn plan(
    driver: &dyn RetentionDriver,
    meta: &MetaStore,
    query: &RetentionQuery<'_>,
    cancellation: &ScanCancellation,
    start: &mut dyn FnMut(RetentionSummary) -> Result<(), String>,
    emit: &mut dyn FnMut(&RetentionDecision) -> Result<(), String>,
) -> Result<RetentionPage, RetentionPlanError> {
    let mut summary = None;
    let mut start_stop = None;
    let started = Cell::new(false);
    let mut protocol_error = None;
    let mut seen = 0_u64;
    let mut emitted = 0_u64;
    let mut stop: Option<Stop> = None;
    let outcome = driver.plan_retention(
        &RetentionScan {
            meta,
            index: query.index,
            policy: query.policy,
            now: query.now,
            cancellation,
        },
        &mut |current| {
            if started.replace(true) {
                start_stop = Some(StartStop::Store(
                    "the retention driver opened more than one snapshot".to_owned(),
                ));
                return Err(HALT.to_owned());
            }
            if let Some(expected) = query.expect
                && expected != current
            {
                start_stop = Some(StartStop::Stale { expected, current });
                return Err(HALT.to_owned());
            }
            start(current).inspect_err(|reason| start_stop = Some(StartStop::Interrupted(reason.clone())))?;
            summary = Some(current);
            Ok(())
        },
        &mut |decision| {
            if !started.get() {
                protocol_error = Some("the retention driver emitted a decision before opening a snapshot".to_owned());
                return Err(HALT.to_owned());
            }
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
        },
    );
    if cancellation.is_cancelled() {
        return Err(RetentionPlanError::Interrupted("request cancelled".to_owned()));
    }
    match start_stop {
        Some(StartStop::Stale { expected, current }) => {
            return Err(RetentionPlanError::Stale { expected, current });
        }
        Some(StartStop::Interrupted(reason)) => return Err(RetentionPlanError::Interrupted(reason)),
        Some(StartStop::Store(reason)) => return Err(RetentionPlanError::Store(reason)),
        None => {}
    }
    if let Some(reason) = protocol_error {
        return Err(RetentionPlanError::Store(reason));
    }
    let Some(summary) = summary else {
        return Err(RetentionPlanError::Store(outcome.err().unwrap_or_else(|| {
            "the retention driver returned without opening a snapshot".to_owned()
        })));
    };
    match (outcome, stop) {
        (_, Some(Stop::Interrupted(reason))) => Err(RetentionPlanError::Interrupted(reason)),
        (Ok(()), _) => Ok(RetentionPage {
            summary,
            next_cursor: None,
            emitted,
        }),
        (Err(_), Some(Stop::Full)) => Ok(RetentionPage {
            summary,
            next_cursor: Some(encode_cursor(
                query.index,
                query.ecosystem,
                query.now,
                query.after + emitted,
                summary,
            )),
            emitted,
        }),
        (Err(reason), None) => Err(RetentionPlanError::Store(reason)),
    }
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
    pub ecosystem: String,
    pub policy: RetentionPolicy,
    pub now: Option<i64>,
    pub after: u64,
    pub expect: Option<RetentionSummary>,
}

/// # Errors
/// Returns [`RetentionPlanError`] when the sink stopped, the store could not be read, or the repository
/// plans no retention.
fn write_export(
    driver: &dyn RetentionDriver,
    meta: &MetaStore,
    query: &RetentionQuery<'_>,
    started: &mut dyn FnMut(RetentionSummary) -> Result<(), String>,
    sink: &mut dyn FnMut(Bytes) -> Result<(), ()>,
) -> Result<(), RetentionPlanError> {
    let sink = RefCell::new(sink);
    let cancellation = ScanCancellation::new();
    plan(
        driver,
        meta,
        query,
        &cancellation,
        &mut |summary| {
            started(summary)?;
            sink.borrow_mut()(line(&ExportHeader { summary })).map_err(|()| "export client gone".to_owned())
        },
        &mut |decision| sink.borrow_mut()(line(decision)).map_err(|()| "export client disconnected".to_owned()),
    )
    .map(drop)
}

fn line(value: &impl Serialize) -> Bytes {
    let mut bytes = serde_json::to_vec(value).expect("a plan record always serializes");
    bytes.push(b'\n');
    Bytes::from(bytes)
}

/// Streams retention decisions from a blocking scan through a bounded body channel.
///
/// A slow reader backpressures the scan, and a disconnected reader stops it. `permit` holds the
/// repository's concurrency slot for the stream lifetime. The validated summary appears first.
///
/// # Errors
/// Returns [`RetentionPlanError`] if the driver cannot open the snapshot or rejects its expected
/// identity.
///
/// # Panics
/// Panics if the caller bypasses retention capability resolution.
pub async fn export_body(
    driver: Arc<dyn crate::serving::RetentionDriver>,
    meta: MetaStore,
    export: RetentionExport,
    permit: RetentionPermit,
) -> Result<(RetentionSummary, Body), RetentionPlanError> {
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(8);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut started_tx = Some(started_tx);
        let query = RetentionQuery {
            index: &export.index,
            ecosystem: &export.ecosystem,
            policy: &export.policy,
            now: export.now,
            after: export.after,
            limit: None,
            expect: export.expect,
        };
        let result = write_export(
            driver.as_ref(),
            &meta,
            &query,
            &mut |summary| {
                let sender = started_tx
                    .take()
                    .ok_or_else(|| "the retention driver opened more than one snapshot".to_owned())?;
                sender.send(Ok(summary)).map_err(|_| "export request gone".to_owned())
            },
            &mut |bytes| tx.blocking_send(Ok(bytes)).map_err(drop),
        );
        if let Err(error) = result {
            if let Some(started_tx) = started_tx {
                let _ = started_tx.send(Err(error));
            } else if let RetentionPlanError::Store(reason) = error {
                let _ = tx.blocking_send(Err(std::io::Error::other(reason)));
            }
        }
    });
    let summary = started_rx
        .await
        .map_err(|_| RetentionPlanError::Interrupted("export worker stopped".to_owned()))??;
    Ok((
        summary,
        Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|chunk| (chunk, rx))
        })),
    ))
}

/// The sentinel a filled page returns from the sink so the driver's scan aborts; the outer match reads
/// [`Stop`], never this string.
const HALT: &str = "retention plan page is full";

enum Stop {
    Full,
    Interrupted(String),
}

enum StartStop {
    Stale {
        expected: RetentionSummary,
        current: RetentionSummary,
    },
    Interrupted(String),
    Store(String),
}

/// A resume offset paired with the plan identity it belongs to, decoded from a cursor.
#[derive(Debug)]
pub struct RetentionResume {
    pub repository: String,
    pub ecosystem: String,
    pub evaluated_at: Option<i64>,
    pub after: u64,
    pub expect: RetentionSummary,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    repository: String,
    ecosystem: String,
    evaluated_at: Option<i64>,
    after: u64,
    summary: RetentionSummary,
}

const CURSOR_VERSION: u8 = 1;

/// # Panics
/// Never in practice: a cursor is a small fixed structure that always serializes.
#[must_use]
pub fn encode_cursor(
    repository: &str,
    ecosystem: &str,
    evaluated_at: Option<i64>,
    after: u64,
    summary: RetentionSummary,
) -> String {
    let bytes = serde_json::to_vec(&Cursor {
        version: CURSOR_VERSION,
        repository: repository.to_owned(),
        ecosystem: ecosystem.to_owned(),
        evaluated_at,
        after,
        summary,
    })
    .expect("a plan cursor always serializes");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// # Errors
/// Returns the reason a token is not a cursor this server issued.
pub fn decode_cursor(cursor: &str) -> Result<RetentionResume, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| "invalid retention plan cursor".to_owned())?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| "invalid retention plan cursor".to_owned())?;
    if cursor.version != CURSOR_VERSION {
        return Err("invalid retention plan cursor".to_owned());
    }
    Ok(RetentionResume {
        repository: cursor.repository,
        ecosystem: cursor.ecosystem,
        evaluated_at: cursor.evaluated_at,
        after: cursor.after,
        expect: cursor.summary,
    })
}

#[cfg(test)]
#[path = "../tests/unit/retention/tests.rs"]
mod tests;
