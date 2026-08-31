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

/// The result of a page build, still inside the blocking worker's return value.
pub enum ChangePageBody {
    Encoded(Vec<u8>),
    Unsynced,
    Cancelled,
    Failed,
}

/// Reads and encodes one page, stopping between records once `cancellation` is raised.
pub fn build_change_page(
    meta: &MetaStore,
    source: &str,
    after: u64,
    limit: usize,
    cancellation: &ScanCancellation,
) -> ChangePageBody {
    let mut changes = Vec::new();
    let walked = meta.visit_journal_page(after, limit, |record| {
        if cancellation.is_cancelled() {
            return ControlFlow::Break(());
        }
        changes.push(Change {
            serial: record.serial,
            event: record.payload,
            metadata: record.mutations.into_iter().map(Into::into).collect(),
            blobs: record.blobs.into_iter().map(Into::into).collect(),
        });
        ControlFlow::Continue(())
    });
    let Ok(current_serial) = walked else {
        return ChangePageBody::Failed;
    };
    if cancellation.is_cancelled() {
        return ChangePageBody::Cancelled;
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
