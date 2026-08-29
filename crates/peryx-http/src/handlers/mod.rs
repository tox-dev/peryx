//! Longest-prefix routing dispatches repository traffic without encoding ecosystem paths here.

use std::collections::BTreeSet;

mod acl;
mod analytics;
mod discover;
mod dispatch;
mod grants;
mod jobs;
mod login;
mod policy_decisions;
mod pql;
mod query;
mod quota;
mod repositories;
mod retention;
mod revocations;
mod status;
mod tokens;
mod trash;
mod usage;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mediatype::{MediaType, names};
use peryx_driver::http_services::VersionPrecondition;
use peryx_driver::state::{AppState, Index};
use peryx_identity::{Action, Denial};

pub use acl::{AclQuery, acl};
pub use analytics::{analytics_groups, analytics_sources, analytics_timeline, analytics_top, analytics_unused};
pub use discover::{api, openapi_spec};
pub use dispatch::{dispatch_delete, dispatch_get, dispatch_post, dispatch_put, not_found};
pub use grants::{GrantsQuery, create_grant, inspect_grant, list_grants, revoke_grant};
pub use jobs::cancel_job;
pub use login::{login_callback, login_start, logout, session, session_user};
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
pub use status::{ReadinessQuery, health, readiness, status, status_authorization};
pub use tokens::{ListTokensQuery, create_token, inspect_token, list_tokens, revoke_token, rotate_token};
pub use trash::{inspect_trash, list_trash, trash_error_response};
pub use usage::{StatsQuery, ecosystem_summaries, family_descriptors, metrics, stats};

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

fn index_by_route<'state>(state: &'state AppState, route: &str) -> Option<&'state Index> {
    state.serving.indexes.iter().find(|index| index.route == route)
}

fn is_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    value
        .to_str()
        .ok()
        .and_then(|value| MediaType::parse(value).ok())
        .is_some_and(|media_type| media_type.ty == names::APPLICATION && media_type.subty == names::JSON)
}

#[derive(Clone, Copy)]
enum IfMatchError {
    Missing,
    Malformed,
}

fn if_match(headers: &HeaderMap) -> Result<VersionPrecondition, IfMatchError> {
    let fields = headers.get_all(header::IF_MATCH);
    let mut values = fields.iter();
    let first = values.next().ok_or(IfMatchError::Missing)?;
    let mut wildcard_count = 0;
    let mut saw_tag = false;
    let mut versions = BTreeSet::new();
    for value in std::iter::once(first).chain(values) {
        parse_if_match_field(value.as_bytes(), &mut wildcard_count, &mut saw_tag, &mut versions)?;
    }
    if wildcard_count > 1 || wildcard_count == 1 && saw_tag {
        return Err(IfMatchError::Malformed);
    }
    Ok(if wildcard_count == 1 {
        VersionPrecondition::Exists
    } else {
        VersionPrecondition::Versions(versions)
    })
}

// Commas are valid inside opaque tags: https://www.rfc-editor.org/rfc/rfc9110.html#section-8.8.3
fn parse_if_match_field(
    field: &[u8],
    wildcard_count: &mut usize,
    saw_tag: &mut bool,
    versions: &mut BTreeSet<u64>,
) -> Result<(), IfMatchError> {
    let mut index = 0;
    while index < field.len() {
        while field.get(index).is_some_and(|byte| matches!(byte, b' ' | b'\t')) {
            index += 1;
        }
        if index == field.len() {
            break;
        }
        if field[index] == b',' {
            index += 1;
            continue;
        }
        if field[index] == b'*' {
            *wildcard_count += 1;
        } else {
            let weak = field[index..].starts_with(b"W/");
            if weak {
                index += 2;
            }
            if field.get(index) != Some(&b'"') {
                return Err(IfMatchError::Malformed);
            }
            index += 1;
            let start = index;
            while field.get(index).is_some_and(|byte| *byte != b'"') {
                if !matches!(field[index], b'!' | b'#'..=b'~' | b'\x80'..=b'\xff') {
                    return Err(IfMatchError::Malformed);
                }
                index += 1;
            }
            if field.get(index) != Some(&b'"') {
                return Err(IfMatchError::Malformed);
            }
            *saw_tag = true;
            if !weak
                && let Ok(value) = std::str::from_utf8(&field[start..index])
                && let Ok(version) = value.parse::<u64>()
                && version.to_string() == value
            {
                versions.insert(version);
            }
        }
        index += 1;
        while field.get(index).is_some_and(|byte| matches!(byte, b' ' | b'\t')) {
            index += 1;
        }
        if index == field.len() {
            break;
        }
        if field[index] != b',' {
            return Err(IfMatchError::Malformed);
        }
        index += 1;
    }
    Ok(())
}

/// HTTP handlers distinguish an authenticated denial from a missing or invalid credential.
enum EcosystemCredentialDenied {
    Forbidden,
    Unauthorized,
}

fn authorize_ecosystem_credential<'state>(
    state: &'state AppState,
    headers: &HeaderMap,
    route: &str,
    action: Action,
) -> Result<&'state Index, EcosystemCredentialDenied> {
    let index = index_by_route(state, route).ok_or(EcosystemCredentialDenied::Unauthorized)?;
    let authorization = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    match state.authorize_index_credential(index, authorization, action) {
        Ok(()) => Ok(index),
        Err(Denial::Forbidden) => Err(EcosystemCredentialDenied::Forbidden),
        Err(Denial::Unavailable | Denial::Unauthenticated) => Err(EcosystemCredentialDenied::Unauthorized),
    }
}
