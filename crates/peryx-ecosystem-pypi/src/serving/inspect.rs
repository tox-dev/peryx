use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_core::path::{self};
use peryx_driver::not_found;
use peryx_driver::state::ServingState;
use peryx_storage::blob::BlobLease;

use crate::cache::{self};

use super::response::{CacheContext, cache_error_response};
use super::{HttpResult, path_error_response, safe_filename};

const MEMBER_SIZE_HEADER: &str = "x-peryx-member-size";

const MEMBER_OFFSET_HEADER: &str = "x-peryx-member-offset";

const MEMBER_NEXT_OFFSET_HEADER: &str = "x-peryx-next-offset";

pub(super) async fn inspect_route(
    state: Arc<ServingState>,
    position: usize,
    target: &str,
    query: Option<&str>,
) -> Response {
    let index = state.index_at(position);
    let route = index.route.clone();
    let Some((sha256, rest)) = target.split_once('/') else {
        return not_found();
    };
    let digest = match super::parse_digest(sha256) {
        Ok(digest) => digest,
        Err(err) => return path_error_response(&err),
    };
    let archive_query = match archive_query(query) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let (raw_filename, member) = match archive_query.member {
        Some(member) => (rest, Some(member)),
        None if archive_query.containers.is_empty() => match rest.split_once('/') {
            Some((filename, member)) => match path::decode_path(member) {
                Ok(member) => (filename, Some(member.into_owned())),
                Err(err) => return path_error_response(&err),
            },
            None => (rest, None),
        },
        None => (rest, None),
    };
    let filename = match safe_filename(raw_filename) {
        Ok(filename) => filename,
        Err(err) => return path_error_response(&err),
    };
    match super::get::download_refusal(&state, index, &filename, &digest).await {
        Ok(Some(refusal)) => return refusal.into_response(),
        Ok(None) => {}
        Err(err) => return cache_error_response(&err, CacheContext::file(&route, sha256, &filename)),
    }
    let owner = index.name.clone();
    let path = match cache::file_path(state, owner, digest, route.clone(), filename.clone()).await {
        Ok(path) => path,
        Err(err) => {
            return cache_error_response(&err, CacheContext::file(&route, sha256, &filename));
        }
    };
    match member {
        Some(member) => {
            archive_member(
                &filename,
                path,
                archive_query.containers,
                &member,
                archive_query.offset,
                archive_query.limit,
            )
            .await
        }
        None => archive_listing(&filename, path, archive_query.containers).await,
    }
}

struct ArchiveQuery {
    member: Option<String>,
    containers: Vec<String>,
    offset: u64,
    limit: u64,
}

fn archive_query(query: Option<&str>) -> HttpResult<ArchiveQuery> {
    let mut parsed = ArchiveQuery {
        member: None,
        containers: Vec::new(),
        offset: 0,
        limit: crate::archive::DEFAULT_MEMBER_CHUNK,
    };
    let Some(query) = query else {
        return Ok(parsed);
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "member" => parsed.member = Some(value.into_owned()),
            "container" => parsed.containers.push(value.into_owned()),
            "offset" => {
                parsed.offset = value
                    .parse::<u64>()
                    .map_err(|_| (StatusCode::BAD_REQUEST, "offset must be a non-negative integer").into_response())?;
            }
            "limit" => {
                let limit = value.parse::<u64>().map_err(|_| {
                    (
                        StatusCode::BAD_REQUEST,
                        "limit must be an integer between 1 and 1048576",
                    )
                        .into_response()
                })?;
                if !(1..=crate::archive::MAX_MEMBER_CHUNK).contains(&limit) {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        format!("limit must be between 1 and {} bytes", crate::archive::MAX_MEMBER_CHUNK),
                    )
                        .into_response()
                        .into());
                }
                parsed.limit = limit;
            }
            _ => {}
        }
    }
    Ok(parsed)
}

async fn archive_listing(filename: &str, lease: BlobLease, containers: Vec<String>) -> Response {
    let filename = filename.to_owned();
    let task = tokio::task::spawn_blocking({
        let filename = filename.clone();
        move || crate::archive::list_members_nested_path(&filename, lease.path(), &containers)
    });
    inspect_response(task, &filename, None, |result| match result {
        Ok(members) => axum::Json(serde_json::json!({ "filename": &filename, "members": members })).into_response(),
        Err(err) => archive_error(&err, &filename, None),
    })
    .await
}

async fn archive_member(
    filename: &str,
    lease: BlobLease,
    containers: Vec<String>,
    member: &str,
    offset: u64,
    limit: u64,
) -> Response {
    let filename = filename.to_owned();
    let member = member.to_owned();
    let task = tokio::task::spawn_blocking({
        let filename = filename.clone();
        let member = member.clone();
        move || {
            crate::archive::read_text_member_chunk_nested_path(
                &filename,
                lease.path(),
                &containers,
                &member,
                offset,
                limit,
            )
        }
    });
    inspect_response(task, &filename, Some(&member), |result| match result {
        Ok(chunk) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            insert_header(&mut headers, MEMBER_SIZE_HEADER, chunk.size);
            insert_header(&mut headers, MEMBER_OFFSET_HEADER, chunk.offset);
            if let Some(next) = chunk.next_offset {
                insert_header(&mut headers, MEMBER_NEXT_OFFSET_HEADER, next);
            }
            (headers, chunk.bytes).into_response()
        }
        Err(err) => archive_error(&err, &filename, Some(&member)),
    })
    .await
}

/// Await a spawned archive inspection and shape its outcome into a response, mapping a worker-thread
/// panic to a `500` rather than letting the join failure abort the request. The archive engine runs
/// on `spawn_blocking`, so without this a panic there would drop the connection instead of returning
/// a structured error.
async fn inspect_response<T: Send + 'static>(
    task: tokio::task::JoinHandle<T>,
    filename: &str,
    member: Option<&str>,
    on_ready: impl FnOnce(T) -> Response,
) -> Response {
    match task.await {
        Ok(ready) => on_ready(ready),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{}: inspection failed: {err}", archive_target(filename, member)),
        )
            .into_response(),
    }
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: u64) {
    if let Ok(value) = HeaderValue::from_str(&value.to_string()) {
        headers.insert(name, value);
    }
}

fn archive_error(err: &crate::archive::ArchiveError, filename: &str, member: Option<&str>) -> Response {
    use crate::archive::ArchiveError;
    let status = match err {
        ArchiveError::Unsupported | ArchiveError::UnsupportedNestedArchive(_) | ArchiveError::BinaryMember(_) => {
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        }
        ArchiveError::MemberNotFound => StatusCode::NOT_FOUND,
        ArchiveError::InvalidRange { .. } => StatusCode::RANGE_NOT_SATISFIABLE,
        ArchiveError::UnsafeMember(_)
        | ArchiveError::TruncatedMember { .. }
        | ArchiveError::Invalid(_)
        | ArchiveError::Read(_) => StatusCode::UNPROCESSABLE_ENTITY,
        ArchiveError::NestingTooDeep { .. } => StatusCode::BAD_REQUEST,
        ArchiveError::InspectionLimitExceeded { .. }
        | ArchiveError::NestedArchiveTooLarge { .. }
        | ArchiveError::TooManyEntries(_) => StatusCode::PAYLOAD_TOO_LARGE,
    };
    (status, format!("{}: {err}", archive_target(filename, member))).into_response()
}

fn archive_target(filename: &str, member: Option<&str>) -> String {
    member.map_or_else(
        || format!("archive {filename:?}"),
        |member| format!("member {member:?} in archive {filename:?}"),
    )
}

#[cfg(test)]
#[path = "../../tests/unit/serving/inspect/tests.rs"]
mod tests;
