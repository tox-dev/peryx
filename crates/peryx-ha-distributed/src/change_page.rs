//! Builds replication change pages off the async request workers, under a concurrency bound.
//!
//! A page is a redb read plus a decode and a JSON encode of every record in it, none of which yields.
//! Run on the request task it holds a Tokio worker for the whole page, so a handful of peers polling
//! the feed can stall unrelated routes. The work therefore moves to a blocking worker, and because a
//! started blocking task cannot be aborted, admission is refused rather than queued and the page walk
//! checks a cancellation flag between records instead of relying on the request future being dropped.

use std::ops::ControlFlow;

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::ScanCancellation;
use peryx_storage::meta::MetaStore;
use tokio::task::JoinError;

use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION};

/// Caps the change-page builds that may occupy blocking workers at once.
///
/// A page holds its decoded records and their encoded form in memory together, so the bound is set by
/// what a node should spend on peers that are catching up, not by how many peers there are.
pub const DEFAULT_MAX_CONCURRENT_CHANGE_PAGES: usize = 4;

/// Caps the encoded bytes of one change page.
///
/// Every client of the change feed reads the reply under this bound and rejects a larger one whole,
/// so a page built above it stops replication at that serial rather than costing a retry. The count
/// limit alone does not bound it: one journal record carries every driver row its transaction
/// touched.
pub const MAX_CHANGE_PAGE_BYTES: u64 = 4 * 1024 * 1024;

/// Headroom for the page fields and JSON framing outside the change array.
const CHANGE_PAGE_ENVELOPE_BYTES: u64 = 4 * 1024;

/// The result of a page build, still inside the blocking worker's return value.
pub enum ChangePageBody {
    Encoded(Vec<u8>),
    /// The record after the cursor fills a page on its own, so no request size makes progress.
    RecordTooLarge {
        serial: u64,
        bytes: u64,
    },
    /// The records after the reader's cursor are no longer held, so no page can carry it forward and a
    /// checkpoint is the only way on.
    ///
    /// It names no serial. A reader that resumed from one named here would resume from what the writer
    /// held when it answered rather than from the checkpoint it went on to install, and a floor that
    /// moved in between would leave its cursor and its state at different serials.
    BelowFloor,
    Unsynced,
    Cancelled,
    Failed,
}

/// Whether the records after `after` are gone.
///
/// A journal that starts above the next record the reader needs has lost the range between them. An
/// empty journal is the same answer wherever its serial is not zero: a node that installed a checkpoint
/// stands at that serial holding no records below it.
const fn below_floor(after: u64, current_serial: u64, floor: Option<u64>) -> bool {
    match floor {
        Some(floor) => after.saturating_add(1) < floor,
        None => after < current_serial,
    }
}

/// Reads and encodes one page, stopping between records once `cancellation` is raised or the next
/// record would carry the page past [`MAX_CHANGE_PAGE_BYTES`].
///
/// A page truncated by the byte budget still reports the snapshot's head serial, so it stays a prefix
/// the reader resumes from instead of a short page that reads as caught up.
pub fn build_change_page(
    meta: &MetaStore,
    source: &str,
    after: u64,
    limit: usize,
    cancellation: &ScanCancellation,
) -> ChangePageBody {
    let mut changes: Vec<Change> = Vec::new();
    let mut used = CHANGE_PAGE_ENVELOPE_BYTES;
    let mut oversized = None;
    let mut encoded = Vec::new();
    let walked = meta.visit_journal_page(after, limit, |record| {
        if cancellation.is_cancelled() {
            return ControlFlow::Break(());
        }
        let change = Change {
            serial: record.serial,
            event: record.payload,
            metadata: record.mutations.into_iter().map(Into::into).collect(),
            blobs: record.blobs.into_iter().map(Into::into).collect(),
        };
        encoded.clear();
        serde_json::to_writer(&mut encoded, &change).expect("a stored journal record serializes");
        let bytes = encoded.len() as u64;
        // One more byte for the separator this change needs once it joins the array.
        if used + bytes + 1 > MAX_CHANGE_PAGE_BYTES {
            if changes.is_empty() {
                oversized = Some((change.serial, bytes));
            }
            return ControlFlow::Break(());
        }
        used += bytes + 1;
        changes.push(change);
        ControlFlow::Continue(())
    });
    // The floor read joins the walk's result, so one arm answers a store this node cannot read at all
    // rather than two that differ only in which read noticed.
    let Ok((current_serial, floor)) = walked.and_then(|serial| meta.journal_floor().map(|floor| (serial, floor)))
    else {
        return ChangePageBody::Failed;
    };
    if below_floor(after, current_serial, floor) {
        return ChangePageBody::BelowFloor;
    }
    if cancellation.is_cancelled() {
        return ChangePageBody::Cancelled;
    }
    if let Some((serial, bytes)) = oversized {
        return ChangePageBody::RecordTooLarge { serial, bytes };
    }
    let page = ChangePage {
        version: PROTOCOL_VERSION,
        source: source.to_owned(),
        after,
        current_serial,
        changes,
    };
    ChangePageBody::Encoded(serde_json::to_vec(&page).expect("a page of stored journal records serializes"))
}

pub fn change_page_response(built: Result<ChangePageBody, JoinError>) -> Response {
    match built {
        Ok(ChangePageBody::Encoded(body)) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Ok(ChangePageBody::RecordTooLarge { serial, bytes }) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("journal record {serial} encodes to {bytes} bytes; a change page holds {MAX_CHANGE_PAGE_BYTES}"),
        )
            .into_response(),
        Ok(ChangePageBody::BelowFloor) => (
            StatusCode::GONE,
            "the records after this cursor are no longer retained; install a checkpoint",
        )
            .into_response(),
        Ok(ChangePageBody::Unsynced) => {
            (StatusCode::SERVICE_UNAVAILABLE, "replica has not synced a source yet").into_response()
        }
        Ok(ChangePageBody::Cancelled) => retry_later("change page build stopped early"),
        Ok(ChangePageBody::Failed) | Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub fn change_pages_at_capacity() -> Response {
    retry_later("peer change page capacity reached")
}

fn retry_later(message: &'static str) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "1")], message).into_response()
}

#[cfg(test)]
#[path = "../tests/unit/change_page_tests.rs"]
mod tests;
