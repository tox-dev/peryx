//! Browsing inside an image layer. A layer is a tar, so it drives the same neutral archive engine
//! and member model the wheel browser uses.

use std::io::Read as _;
use std::path::Path;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_storage::archive::{self, ArchiveError, MemberChunk};

/// The `member` (and its `offset`) a layer-contents request selects, or `None` to list the layer.
pub(super) fn layer_query_member(query: &str) -> Option<(String, u64)> {
    let mut member = None;
    let mut offset = 0;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "member" => member = Some(value.into_owned()),
            "offset" => offset = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    member.map(|member| (member, offset))
}

/// A synthetic filename that tells the archive engine how the layer blob is framed. The engine picks
/// its decoder by extension, and a content-addressed blob has none, so sniff the gzip magic and name
/// it accordingly.
fn layer_archive_name(path: &Path) -> &'static str {
    let mut magic = [0_u8; 2];
    let gzip = std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == [0x1f, 0x8b];
    if gzip { "layer.tar.gz" } else { "layer.tar" }
}

/// List a stored layer's members, or preview one text member, as a response the web UI's archive
/// browser consumes: a `{ "members": [...] }` document, or `text/plain` bytes with the member-size,
/// offset, and next-offset headers of the neutral archive-inspect contract.
pub(super) fn layer_contents_response(path: &Path, selected: Option<(String, u64)>) -> Response {
    let filename = layer_archive_name(path);
    match selected {
        None => match archive::list_members_path(filename, path) {
            Ok(members) => axum::Json(serde_json::json!({ "members": members })).into_response(),
            Err(err) => layer_error_response(&err),
        },
        Some((member, offset)) => {
            match archive::read_text_member_chunk_nested_path(
                filename,
                path,
                &[],
                &member,
                offset,
                archive::DEFAULT_MEMBER_CHUNK,
            ) {
                Ok(chunk) => member_chunk_response(&chunk),
                Err(err) => layer_error_response(&err),
            }
        }
    }
}

/// A previewed text member: its bytes as `text/plain`, plus the size/offset/next-offset headers the
/// browser reads to page through a large member.
fn member_chunk_response(chunk: &MemberChunk) -> Response {
    let mut response = Response::new(Body::from(chunk.bytes.clone()));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    insert_member_header(headers, "x-peryx-member-size", chunk.size);
    insert_member_header(headers, "x-peryx-member-offset", chunk.offset);
    if let Some(next) = chunk.next_offset {
        insert_member_header(headers, "x-peryx-next-offset", next);
    }
    response
}

fn insert_member_header(headers: &mut HeaderMap, name: &'static str, value: u64) {
    if let Ok(value) = HeaderValue::from_str(&value.to_string()) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

/// Map an archive engine failure onto a client status for peryx's own layer browser: a missing
/// member is a `404`, a non-text member a `415`, a bad preview range a `416`, and anything else (a
/// corrupt or unreadable layer) a `422`. This is not a distribution-spec route, so it answers with a
/// plain status and message the web UI surfaces, not a coded registry error envelope.
fn layer_error_response(err: &ArchiveError) -> Response {
    let status = match err {
        ArchiveError::MemberNotFound => StatusCode::NOT_FOUND,
        ArchiveError::BinaryMember(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ArchiveError::InvalidRange { .. } => StatusCode::RANGE_NOT_SATISFIABLE,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    (status, err.to_string()).into_response()
}
