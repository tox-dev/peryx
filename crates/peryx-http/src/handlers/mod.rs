//! axum request handlers.
//!
//! All index traffic arrives on a catch-all path that is resolved to a configured index by longest
//! route prefix, then handed to that index's ecosystem serving driver. The handlers here are
//! ecosystem-neutral: they dispatch to the driver and serve the cross-cutting endpoints (search,
//! status, stats, metrics, `OpenAPI`, discovery).

mod acl;
mod analytics;
mod availability;
mod discover;
mod dispatch;
mod grants;
mod oidc;
mod policy_decisions;
mod pql;
mod query;
mod quota;
mod retention;
mod revocations;
mod shadow;
mod status;
mod tokens;
mod trash;
mod ui;
mod usage;

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_identity::Denial;

pub use acl::{AclQuery, acl};
pub use analytics::{analytics_sources, analytics_timeline, analytics_top, analytics_unused, analytics_versions};
pub use availability::availability_topology;
pub use discover::{api, openapi_spec};
pub use dispatch::{dispatch_delete, dispatch_get, dispatch_post, dispatch_put, not_found};
pub use grants::{GrantsQuery, create_grant, inspect_grant, list_grants, revoke_grant};
pub use oidc::{oidc_audience, oidc_mint_token};
pub use policy_decisions::{PolicyDecisionsQuery, policy_decision_error_response, policy_decisions};
pub use pql::pql_query;
pub use query::{search, search_error_response, search_response, search_response_offloaded};
pub use quota::{quota_repository, quota_summary};
pub use retention::{retention_export, retention_plan};
pub use revocations::{DigestRevocationsQuery, inspect_revocation, lift_revocation, list_revocations, put_revocation};
pub use shadow::{shadow_candidates, shadow_error_response};
pub use status::{ReadinessQuery, health, readiness, status, status_authorization};
pub use tokens::{ListTokensQuery, create_token, inspect_token, list_tokens, revoke_token, rotate_token};
pub use trash::{inspect_trash, list_trash, trash_error_response};
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
