use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::state::ServingState;
use peryx_identity::{Action, Denial, authorize_all};
use peryx_index::IndexKind;

use crate::store::{ChangelogReadError, read_changelog_page};
use crate::{dispatch_changelog_request, render_changelog_fault};

const CHANGELOG_BODY_LIMIT: usize = 64 * 1024;
const XML_CONTENT_TYPE: &str = "text/xml; charset=utf-8";

pub(super) fn is_changelog_path(path: &str, headers: &HeaderMap) -> bool {
    matches!(path.as_bytes(), b"pypi" | b"pypi/" | b"RPC2")
        && !headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("multipart/form-data"))
}

pub(super) async fn pypi_changelog(state: Arc<ServingState>, request: Request) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    if let Err(response) = authorize(&state, request.headers()) {
        return response;
    }
    let body = match to_bytes(request.into_body(), CHANGELOG_BODY_LIMIT).await {
        Ok(body) => body,
        Err(error) => {
            tracing::debug!(%error, "rejected invalid XML-RPC request body");
            return xml_response(render_changelog_fault(-32600, "server error; invalid request"));
        }
    };
    match dispatch_changelog_request(
        &body,
        || state.meta.current_serial().map_err(ChangelogReadError::from),
        |after, limit| read_changelog_page(&state.meta, after, limit),
    ) {
        Ok(body) => xml_response(body),
        Err(error) => {
            tracing::error!(%error, "failed to read the PyPI changelog");
            xml_response(render_changelog_fault(-32403, "server error; service unavailable"))
        }
    }
}

fn authorize(state: &ServingState, headers: &HeaderMap) -> Result<(), Response> {
    let authorization = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    let now = (state.clock)();
    let mut unauthenticated = false;
    let mut forbidden = false;
    for index in state
        .indexes
        .iter()
        .filter(|index| index.ecosystem == crate::ECOSYSTEM && matches!(index.kind, IndexKind::Hosted { .. }))
    {
        let identity = index.acl.identify(authorization, now);
        match authorize_all(&identity.principal, &index.acl, Action::Read) {
            Ok(()) => {}
            Err(Denial::Unauthenticated) => unauthenticated = true,
            Err(Denial::Unavailable | Denial::Forbidden) => forbidden = true,
        }
    }
    if unauthenticated {
        Err((
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx\"")],
            "unauthorized",
        )
            .into_response())
    } else if forbidden {
        Err((StatusCode::FORBIDDEN, "forbidden").into_response())
    } else {
        Ok(())
    }
}

fn xml_response(body: String) -> Response {
    ([(header::CONTENT_TYPE, XML_CONTENT_TYPE)], body).into_response()
}
