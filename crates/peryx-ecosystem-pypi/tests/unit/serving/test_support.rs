use axum::response::Response;

use super::Format;
use crate::ProjectList;
use crate::cache::CacheError;

pub fn index_response(result: Result<(ProjectList, Option<u64>), CacheError>, format: Format, index: &str) -> Response {
    super::response::index_response(result, format, index)
}
