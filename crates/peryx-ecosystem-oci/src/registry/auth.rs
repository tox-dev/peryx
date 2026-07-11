//! The OCI Bearer token realm: the wire protocol peryx wraps around the neutral access model.
//!
//! `GET /v2/` challenges with `WWW-Authenticate: Bearer realm=…,service="peryx"` when any OCI index
//! restricts access, so `docker login` learns where to authenticate; `GET /v2/token` mints a JWT whose
//! grants are the intersection of the requested scope with what the caller may do; and every
//! `/v2/<name>/…` route verifies the presented token and re-challenges with the scope it lacked. The
//! scope grammar (`repository:<name>:pull,push`) lives here and nowhere else: the neutral core in
//! [`peryx_identity`] knows only a principal, an index ACL, a project, and an action.

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use peryx_core::Ecosystem;
use peryx_driver::ServingState;
use peryx_driver::discovery::BaseUrl;
use peryx_identity::{Action, Denial, Glob, Grant, Identity, IndexAcl, Principal, Signer, authorize};
use serde_json::json;
use std::collections::BTreeSet;

use crate::error::{ErrorCode, error_response};

/// The `service` a challenge and a token request name, so a client's token is bound to this realm.
const SERVICE: &str = "peryx";
/// The realm path a challenge points a client at.
const TOKEN_PATH: &str = "/v2/token";

/// Answer `GET /v2/`: `200` with the API-version header for a deployment no ACL restricts or a request
/// carrying a valid credential, otherwise `401` with the Bearer challenge that starts `docker login`.
pub(super) fn negotiate_version(state: &ServingState, headers: &HeaderMap) -> Response {
    if let Some(signer) = &state.signer
        && restricts(state)
        && !presents_valid_credential(signer, state, headers)
    {
        return challenge(headers, None, None);
    }
    super::version_ok()
}

/// Whether any OCI index restricts access, which is what turns the frictionless zero-config `200` into
/// a Bearer challenge: reads that are not anonymous, or any named credential a `docker login` validates.
fn restricts(state: &ServingState) -> bool {
    state
        .indexes
        .iter()
        .filter(|index| index.ecosystem == Ecosystem::Oci)
        .any(|index| !index.acl.anonymous_read || !index.acl.tokens.is_empty())
}

/// Whether the request already carries a credential this realm accepts: a bearer it signed, or a Basic
/// password one of its indexes issued. Takes the signer the caller resolved, so a bearer is verified
/// without re-checking that a realm exists.
fn presents_valid_credential(signer: &Signer, state: &ServingState, headers: &HeaderMap) -> bool {
    let Some(header) = authorization(headers) else {
        return false;
    };
    if let Some(token) = header.strip_prefix("Bearer ") {
        return signer.verify(token).is_ok();
    }
    if header.starts_with("Basic ") {
        return named_principal(state, header).is_some();
    }
    false
}

/// Answer `GET /v2/token`: always a `200` JWT whose grants are the intersection of the requested scope
/// with what the caller may do, except that a Basic credential matching no live token is a real login
/// failure and gets `401`.
pub(super) fn issue_token(state: &ServingState, headers: &HeaderMap, query: &str) -> Response {
    let Some(signer) = &state.signer else {
        return error_response(ErrorCode::Unsupported, "token authentication is not enabled");
    };
    let principal = match resolve_principal(state, authorization(headers)) {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let grants = approved_grants(state, &principal, &parse_scopes(query));
    let now = (state.clock)();
    let token = signer.mint(&principal, &grants, now, state.token_ttl_secs);
    let body = json!({
        "token": token,
        "access_token": token,
        "expires_in": state.token_ttl_secs,
    })
    .to_string();
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// The principal a token request speaks as: anonymous with no credential, the named subject a Basic
/// password authenticates, or a `401` when a Basic password authenticates nowhere — the check that
/// makes `docker login` reject a wrong password instead of silently issuing an anonymous token.
fn resolve_principal(state: &ServingState, header: Option<&str>) -> Result<Principal, Response> {
    match header {
        Some(header) if header.starts_with("Basic ") => {
            named_principal(state, header).ok_or_else(|| error_response(ErrorCode::Unauthorized, "invalid credentials"))
        }
        _ => Ok(Principal::Anonymous),
    }
}

/// The named subject a Basic password authenticates on any OCI index, so a token issued for one index's
/// credential is validated wherever that credential is configured.
fn named_principal(state: &ServingState, header: &str) -> Option<Principal> {
    let now = (state.clock)();
    state
        .indexes
        .iter()
        .filter(|index| index.ecosystem == Ecosystem::Oci)
        .find_map(|index| match index.acl.identify(Some(header), now).principal {
            Principal::Named { subject } => Some(Principal::Named { subject }),
            Principal::Anonymous => None,
        })
}

/// The grants a token carries: for each requested scope, the actions the principal may take on the
/// repository it names, resolved through the same [`super::resolve`] the resource routes use. A scope
/// that resolves to nothing, or grants nothing, contributes no grant — an empty result is a valid token,
/// not an error.
fn approved_grants(state: &ServingState, principal: &Principal, scopes: &[RequestedScope]) -> Vec<Grant> {
    let mut grants = Vec::new();
    for scope in scopes {
        let Some((index, repo)) = super::resolve(&state.indexes, &scope.name) else {
            continue;
        };
        let actions: BTreeSet<Action> = scope
            .actions
            .iter()
            .copied()
            .filter(|&action| authorize(principal, &index.acl, Some(repo), action).is_ok())
            .collect();
        if !actions.is_empty() {
            grants.push(Grant {
                projects: vec![Glob::new(scope.name.clone())],
                actions,
            });
        }
    }
    grants
}

/// One requested access scope: the repository `<name>` and the neutral actions its OCI verbs map to.
struct RequestedScope {
    name: String,
    actions: BTreeSet<Action>,
}

/// The scopes a token request asks for. A client sends one `scope` per repository, or several
/// space-separated in one parameter; both spellings are accepted.
fn parse_scopes(query: &str) -> Vec<RequestedScope> {
    url::form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _)| key == "scope")
        .flat_map(|(_, value)| value.split(' ').filter_map(parse_scope).collect::<Vec<_>>())
        .collect()
}

/// One `repository:<name>:<actions>` scope into its repository and the neutral actions it requests; any
/// other resource type, or an empty name, is not a repository scope and yields nothing.
fn parse_scope(scope: &str) -> Option<RequestedScope> {
    let fields: Vec<&str> = scope.splitn(3, ':').collect();
    let [kind, name, actions] = fields[..] else {
        return None;
    };
    if kind != "repository" || name.is_empty() {
        return None;
    }
    let actions: BTreeSet<Action> = actions
        .split(',')
        .flat_map(|verb| scope_actions(verb).iter().copied())
        .collect();
    Some(RequestedScope {
        name: name.to_owned(),
        actions,
    })
}

/// The neutral actions one OCI scope verb requests: `pull` reads, `push` writes, `delete` deletes, and
/// `*` all three; an unknown verb requests nothing.
fn scope_actions(verb: &str) -> &'static [Action] {
    match verb {
        "pull" => &[Action::Read],
        "push" => &[Action::Write],
        "delete" => &[Action::Delete],
        "*" => &[Action::Read, Action::Write, Action::Delete],
        _ => &[],
    }
}

/// Authorize a read of `<name>` before its handler runs, resolving the index it addresses and checking
/// the presented credential against the index ACL. On refusal it returns the scoped challenge to send.
/// A name that resolves to no index passes through so the handler answers name-unknown itself: there is
/// no ACL to check and no artifact to protect.
pub(super) fn authorize_read(state: &ServingState, headers: &HeaderMap, name: &str) -> Result<(), Response> {
    let Some((index, repo)) = super::resolve(&state.indexes, name) else {
        return Ok(());
    };
    let (identity, bad_token) = identify(state, &index.acl, headers);
    authorize(&identity.principal, &index.acl, Some(repo), Action::Read)
        .map_err(|denial| resource_challenge(state, headers, name, Action::Read, denial, bad_token))
}

/// The principal a resource request presents, and whether it presented a bearer this realm could not
/// verify. A verifiable bearer names its subject; a Basic credential is matched against the index's
/// tokens; anything else, including a bearer that fails verification, leaves the request anonymous.
pub(super) fn identify(state: &ServingState, acl: &IndexAcl, headers: &HeaderMap) -> (Identity, bool) {
    let header = authorization(headers);
    if let Some(token) = header.and_then(|header| header.strip_prefix("Bearer "))
        && let Some(signer) = &state.signer
    {
        return match signer.verify(token) {
            Ok((principal, _)) => (Identity { principal, user: None }, false),
            Err(_) => (
                Identity {
                    principal: Principal::Anonymous,
                    user: None,
                },
                true,
            ),
        };
    }
    (acl.identify(header, (state.clock)()), false)
}

/// The response for a refused resource request: with a realm configured, a `401` Bearer challenge
/// carrying the scope the request needed and an `error` a client acts on — `invalid_token` retries with
/// fresh credentials, `insufficient_scope` does not. Without a realm the registry keeps the Basic answers
/// a pushing client already handles, so an existing `docker login -u _ -p <token>` flow is untouched.
pub(super) fn resource_challenge(
    state: &ServingState,
    headers: &HeaderMap,
    name: &str,
    action: Action,
    denial: Denial,
    bad_token: bool,
) -> Response {
    if state.signer.is_none() {
        return if matches!(denial, Denial::Forbidden) {
            error_response(ErrorCode::Denied, "token does not grant this action")
        } else {
            basic_challenge()
        };
    }
    let error = if bad_token {
        Some("invalid_token")
    } else if matches!(denial, Denial::Forbidden) {
        Some("insufficient_scope")
    } else {
        None
    };
    challenge(headers, Some(&resource_scope(name, action)), error)
}

/// The `repository:<name>:<verbs>` scope a challenge advertises for an action, so a client knows the
/// token to request: a pull for a read, push for a write, delete for a removal (each with pull, the
/// prerequisite every registry client assumes).
fn resource_scope(name: &str, action: Action) -> String {
    let verbs = match action {
        Action::Read => "pull",
        Action::Write => "pull,push",
        Action::Delete => "pull,delete",
    };
    format!("repository:{name}:{verbs}")
}

/// A `401` carrying the Bearer challenge: the realm to authenticate at, the service the token binds to,
/// and optionally the scope needed and the `error` explaining the refusal.
fn challenge(headers: &HeaderMap, scope: Option<&str>, error: Option<&str>) -> Response {
    use std::fmt::Write as _;
    let mut value = format!("Bearer realm=\"{}\",service=\"{SERVICE}\"", realm(headers));
    if let Some(scope) = scope {
        let _ = write!(value, ",scope=\"{scope}\"");
    }
    if let Some(error) = error {
        let _ = write!(value, ",error=\"{error}\"");
    }
    unauthorized(&value)
}

/// The Basic challenge a realm-less registry falls back to, the answer an existing `docker login`
/// push flow already expects.
fn basic_challenge() -> Response {
    unauthorized("Basic realm=\"peryx\"")
}

/// A `401` with the distribution-spec error body and the given `WWW-Authenticate` value.
fn unauthorized(www_authenticate: &str) -> Response {
    let body = json!({"errors": [{"code": "UNAUTHORIZED", "message": "authentication required"}]}).to_string();
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, www_authenticate)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("unauthorized response builds from validated parts")
}

/// The absolute realm URL a challenge points at, derived from the request's forwarded origin; a request
/// that carries no host falls back to the relative path, still enough for a client on the same origin.
fn realm(headers: &HeaderMap) -> String {
    let placeholder = Uri::from_static("/");
    BaseUrl::from_request(headers, &placeholder).map_or_else(|| TOKEN_PATH.to_owned(), |base| base.join(TOKEN_PATH))
}

fn authorization(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::{Action, RequestedScope, parse_scopes, resource_scope, scope_actions};
    use std::collections::BTreeSet;

    fn scope_named<'a>(scopes: &'a [RequestedScope], name: &str) -> &'a BTreeSet<Action> {
        &scopes
            .iter()
            .find(|scope| scope.name == name)
            .expect("scope present")
            .actions
    }

    #[test]
    fn test_parse_scopes_maps_verbs_and_drops_unusable_scopes() {
        // `+` is a query-encoded space, so one parameter can carry several scopes; a second parameter
        // adds another. A non-repository resource, an empty name, and a two-field scope are all dropped.
        let scopes = parse_scopes(
            "scope=repository:team/app:pull,push+repository:lib/x:delete,bogus&\
             scope=repository:all/y:*+registry:catalog:*+repository::pull+repository:onlytwo",
        );
        assert_eq!(scopes.len(), 3);
        assert_eq!(
            scope_named(&scopes, "team/app"),
            &BTreeSet::from([Action::Read, Action::Write])
        );
        assert_eq!(scope_named(&scopes, "lib/x"), &BTreeSet::from([Action::Delete]));
        assert_eq!(
            scope_named(&scopes, "all/y"),
            &BTreeSet::from([Action::Read, Action::Write, Action::Delete])
        );
    }

    #[test]
    fn test_scope_actions_maps_each_verb() {
        assert_eq!(scope_actions("pull"), &[Action::Read]);
        assert_eq!(scope_actions("push"), &[Action::Write]);
        assert_eq!(scope_actions("delete"), &[Action::Delete]);
        assert_eq!(scope_actions("*"), &[Action::Read, Action::Write, Action::Delete]);
        assert!(scope_actions("mystery").is_empty());
    }

    #[test]
    fn test_resource_scope_advertises_the_verbs_for_each_action() {
        assert_eq!(resource_scope("team/app", Action::Read), "repository:team/app:pull");
        assert_eq!(
            resource_scope("team/app", Action::Write),
            "repository:team/app:pull,push"
        );
        assert_eq!(
            resource_scope("team/app", Action::Delete),
            "repository:team/app:pull,delete"
        );
    }
}
