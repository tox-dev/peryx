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
mod jobs;
mod login;
mod oidc;
mod operations;
mod placements;
mod policy_decisions;
mod pql;
mod query;
mod quota;
mod repositories;
mod retention;
mod revocations;
mod shadow;
mod status;
mod tokens;
mod trash;
mod ui;
mod usage;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_driver::state::{AppState, Index};
use peryx_identity::{Action, Denial, authorize_all};

pub use acl::{AclQuery, acl};
pub use analytics::{
    analytics_completeness, analytics_sources, analytics_timeline, analytics_top, analytics_unused, analytics_versions,
};
pub use availability::{availability_topology, availability_topology_stream};
pub use discover::{api, openapi_spec};
pub use dispatch::{dispatch_delete, dispatch_get, dispatch_post, dispatch_put, not_found};
pub use grants::{GrantsQuery, create_grant, inspect_grant, list_grants, revoke_grant};
pub use jobs::cancel_job;
pub use login::{login_callback, login_start, logout, session, session_user};
pub use oidc::{oidc_audience, oidc_mint_token};
pub use operations::operations;
pub use placements::{blob_placements, placements};
pub use policy_decisions::{PolicyDecisionsQuery, policy_decision_error_response, policy_decisions};
pub use pql::pql_query;
pub use query::{search, search_error_response, search_response, search_response_offloaded};
pub use quota::{quota_repository, quota_summary};
pub use repositories::{
    RepositoriesQuery, create_repository, disable_repository, enable_repository, inspect_repository, list_repositories,
    update_repository,
};
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

/// The configured index a `route` addresses, or `None` when no index claims it. Callers render the
/// miss as their own not-found response.
fn index_by_route<'state>(state: &'state AppState, route: &str) -> Option<&'state Index> {
    state.indexes.iter().find(|index| index.route == route)
}

/// The two outcomes a legacy per-index token authorization can be denied for, before a handler renders
/// them: `403` when a known token holds no covering grant, `401` for every other denial - an unknown
/// route, an absent credential, or an index whose ACL grants the action to no one.
enum LegacyDenied {
    Forbidden,
    Unauthorized,
}

/// Authorize a legacy `__token__` request against the ACL of the index that `route` addresses, for
/// `action`.
///
/// This is the one path a per-index upload token takes across the trash, quota, shadow, analytics, and
/// policy-decision handlers: resolve the route to its index, identify the credential against that
/// index's ACL, and ask whether the resulting principal may take `action` on any project there. Each
/// caller maps the returned index and [`LegacyDenied`] onto its own response type; the role-based path
/// for server users is decided separately.
fn authorize_legacy_route<'state>(
    state: &'state AppState,
    headers: &HeaderMap,
    route: &str,
    action: Action,
) -> Result<&'state Index, LegacyDenied> {
    let index = index_by_route(state, route).ok_or(LegacyDenied::Unauthorized)?;
    let authorization = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    let principal = index.acl.identify(authorization, (state.clock)()).principal;
    match authorize_all(&principal, &index.acl, action) {
        Ok(()) => Ok(index),
        Err(Denial::Forbidden) => Err(LegacyDenied::Forbidden),
        Err(Denial::Unavailable | Denial::Unauthenticated) => Err(LegacyDenied::Unauthorized),
    }
}
