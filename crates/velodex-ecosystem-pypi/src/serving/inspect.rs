//! Archive inspection: list a cached distribution's members or read one text member chunk.
#![allow(
    clippy::result_large_err,
    reason = "handler helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use velodex_http::handlers::not_found;
use velodex_http::path_safety::{self};
use velodex_http::state::AppState;

use crate::cache::{self};

use super::response::{CacheContext, file_response};
use super::{path_error_response, safe_filename};

const MEMBER_SIZE_HEADER: &str = "x-velodex-member-size";

const MEMBER_OFFSET_HEADER: &str = "x-velodex-member-offset";

const MEMBER_NEXT_OFFSET_HEADER: &str = "x-velodex-next-offset";

/// `GET /{route}/inspect/{sha256}/{filename}` lists a cached archive's members, or reads one text
/// member inline. Repeated `container` query parameters select nested archives.
pub(super) async fn inspect_route(state: Arc<AppState>, route: String, target: &str, query: Option<&str>) -> Response {
    let Some((sha256, rest)) = target.split_once('/') else {
        return not_found();
    };
    let digest = match path_safety::parse_digest(sha256) {
        Ok(digest) => digest,
        Err(err) => return path_error_response(&err),
    };
    let archive_query = match archive_query(query) {
        Ok(query) => query,
        Err(response) => return response,
    };
    let (raw_filename, member) = match archive_query.member {
        Some(member) => (rest, Some(member)),
        None if archive_query.containers.is_empty() => match rest.split_once('/') {
            Some((filename, member)) => match path_safety::decode_path(member) {
                Ok(member) => (filename, Some(member)),
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
    let path = match cache::file_path(state, digest, route.clone(), filename.clone()).await {
        Ok(path) => path,
        Err(err) => {
            return file_response(Err(err), CacheContext::file(&route, sha256, &filename));
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

fn archive_query(query: Option<&str>) -> Result<ArchiveQuery, Response> {
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
                        .into_response());
                }
                parsed.limit = limit;
            }
            _ => {}
        }
    }
    Ok(parsed)
}

/// Render an archive's member list as JSON.
async fn archive_listing(filename: &str, path: std::path::PathBuf, containers: Vec<String>) -> Response {
    let filename = filename.to_owned();
    match tokio::task::spawn_blocking({
        let filename = filename.clone();
        move || crate::archive::list_members_nested_path(&filename, &path, &containers)
    })
    .await
    .expect("archive listing task panicked")
    {
        Ok(members) => axum::Json(serde_json::json!({ "filename": filename, "members": members })).into_response(),
        Err(err) => archive_error(&err, &filename, None),
    }
}

/// Serve one text archive member chunk.
async fn archive_member(
    filename: &str,
    path: std::path::PathBuf,
    containers: Vec<String>,
    member: &str,
    offset: u64,
    limit: u64,
) -> Response {
    let filename = filename.to_owned();
    let member = member.to_owned();
    match tokio::task::spawn_blocking({
        let filename = filename.clone();
        let member = member.clone();
        move || {
            crate::archive::read_text_member_chunk_nested_path(&filename, &path, &containers, &member, offset, limit)
        }
    })
    .await
    .expect("archive member task panicked")
    {
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
        ArchiveError::UnsafeMember(_) | ArchiveError::Invalid(_) | ArchiveError::Read(_) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        ArchiveError::NestingTooDeep { .. } => StatusCode::BAD_REQUEST,
        ArchiveError::NestedArchiveTooLarge { .. } | ArchiveError::TooManyEntries(_) => StatusCode::PAYLOAD_TOO_LARGE,
    };
    let target = member.map_or_else(
        || format!("archive {filename:?}"),
        |member| format!("member {member:?} in archive {filename:?}"),
    );
    (status, format!("{target}: {err}")).into_response()
}
