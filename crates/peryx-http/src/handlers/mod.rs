//! axum request handlers.
//!
//! All index traffic arrives on a catch-all path that is resolved to a configured index by longest
//! route prefix, then handed to that index's ecosystem serving driver. The handlers here are
//! ecosystem-neutral: they dispatch to the driver and serve the cross-cutting endpoints (search,
//! status, stats, metrics, `OpenAPI`, discovery).

mod acl;
mod discover;
mod dispatch;
mod query;
mod status;
mod ui;
mod usage;

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_identity::Denial;

pub use acl::{AclQuery, acl};
pub use discover::{api, openapi_spec};
pub use dispatch::{dispatch_delete, dispatch_get, dispatch_post, dispatch_put, not_found};
pub use query::{search, search_error_response, search_response, search_response_offloaded};
pub use status::{ReadinessQuery, StatusQuery, health, readiness, status};
pub use ui::{ui_manifest, ui_member, ui_members, ui_project, ui_projects};
pub use usage::{StatsQuery, ecosystem_summaries, family_descriptors, metrics, stats};

/// Map an authorization [`Denial`] to its HTTP answer: `403` when the credential is valid but holds no
/// covering grant, `401` with a Basic challenge when the request could authenticate and did not.
fn denied(denial: Denial) -> Response {
    if denial == Denial::Forbidden {
        return StatusCode::FORBIDDEN.into_response();
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx\"")],
    )
        .into_response()
}
