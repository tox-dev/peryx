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
use peryx_identity::{
    Action, Denial, Glob, Grant, Identity, IndexAcl, Principal, Signer, authorize, authorize_all,
    authorize_exact_grants, authorize_grants, strip_auth_scheme,
};
use serde_json::json;
use std::collections::{BTreeSet, HashMap};

use crate::error::{ErrorCode, error_response};

/// The realm path a challenge points a client at.
const TOKEN_PATH: &str = "/v2/token";
const CATALOG_SCOPE: &str = "registry:catalog:*";
const CATALOG_GRANT: &str = "\0registry\0catalog";

/// Answer `GET /v2/`: `200` with the API-version header for a deployment no ACL restricts or a request
/// carrying a valid credential, otherwise `401` with the Bearer challenge that starts `docker login`.
pub(super) fn negotiate_version(state: &ServingState, headers: &HeaderMap) -> Response {
    if let Some(signer) = &state.signer
        && restricts(state)
        && !presents_valid_credential(signer, state, headers)
    {
        return challenge(signer.audience(), headers, None, None);
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
    if let Some(token) = strip_auth_scheme(header, "Bearer") {
        return signer.verify(token).is_ok();
    }
    if strip_auth_scheme(header, "Basic").is_some() {
        return named_requester(state, header).is_some();
    }
    false
}

/// Answer `GET /v2/token`: a request for this realm's service gets a JWT whose grants are the
/// intersection of the requested scope with what the caller may do. A missing or different service is
/// denied, and a Basic credential matching no live token is a login failure.
pub(super) fn issue_token(state: &ServingState, headers: &HeaderMap, query: &str) -> Response {
    let Some(signer) = &state.signer else {
        return error_response(ErrorCode::Unsupported, "token authentication is not enabled");
    };
    let Some(scopes) = parse_token_request(query, signer.audience()) else {
        return error_response(ErrorCode::Denied, "requested service is not available");
    };
    let requester = match resolve_requester(state, authorization(headers)) {
        Ok(requester) => requester,
        Err(response) => return response,
    };
    let grants = approved_grants(state, &requester, &scopes);
    let now = (state.clock)();
    let token = signer.mint(&requester.principal, &grants, now, state.token_ttl_secs);
    let body = json!({
        "token": token,
        "access_token": token,
        "expires_in": state.token_ttl_secs,
    })
    .to_string();
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// The identity and source credential a token request speaks as: anonymous with no credential, the
/// named subject a Basic password authenticates, or a `401` when Basic authenticates nowhere.
fn resolve_requester<'a>(state: &'a ServingState, header: Option<&'a str>) -> Result<TokenRequester<'a>, Response> {
    match header {
        Some(header) if strip_auth_scheme(header, "Basic").is_some() => {
            named_requester(state, header).ok_or_else(|| error_response(ErrorCode::Unauthorized, "invalid credentials"))
        }
        _ => Ok(TokenRequester {
            principal: Principal::Anonymous,
            basic: None,
        }),
    }
}

/// The named subject and first OCI index a Basic password authenticates against.
fn named_requester<'a>(state: &'a ServingState, header: &'a str) -> Option<TokenRequester<'a>> {
    let now = (state.clock)();
    state
        .indexes
        .iter()
        .filter(|index| index.ecosystem == Ecosystem::Oci)
        .find_map(|index| match index.acl.identify(Some(header), now).principal {
            Principal::Named { subject } => Some(TokenRequester {
                principal: Principal::Named { subject },
                basic: Some(BasicAuthentication {
                    header,
                    index: &index.name,
                    now,
                }),
            }),
            Principal::Anonymous => None,
        })
}

struct TokenRequester<'a> {
    principal: Principal,
    basic: Option<BasicAuthentication<'a>>,
}

#[derive(Clone, Copy)]
struct BasicAuthentication<'a> {
    header: &'a str,
    index: &'a str,
    now: i64,
}

/// The grants a token carries: for each requested scope, the actions the principal may take on the
/// repository it names, resolved through the same [`super::resolve`] the resource routes use. A named
/// requester must authenticate as the same subject on every scoped index. A scope that resolves to
/// nothing, authenticates as another subject, or grants nothing contributes no grant.
fn approved_grants(state: &ServingState, requester: &TokenRequester<'_>, scopes: &[RequestedScope]) -> Vec<Grant> {
    let mut grants = Vec::new();
    let mut authenticated_indexes = requester
        .basic
        .map(|basic| (basic, HashMap::from([(basic.index, true)])));
    for scope in scopes {
        match &scope.resource {
            ScopeResource::Repository(name) => {
                let Some((index, repo)) = super::resolve(&state.indexes, name) else {
                    continue;
                };
                if let Some((basic, indexes)) = &mut authenticated_indexes
                    && !*indexes.entry(index.name.as_str()).or_insert_with(|| {
                        index.acl.identify(Some(basic.header), basic.now).principal == requester.principal
                    })
                {
                    continue;
                }
                let actions: BTreeSet<Action> = scope
                    .actions
                    .iter()
                    .copied()
                    .filter(|&action| authorize(&requester.principal, &index.acl, Some(repo), action).is_ok())
                    .collect();
                if !actions.is_empty() {
                    grants.push(Grant {
                        projects: vec![Glob::new(name.clone())],
                        actions,
                    });
                }
            }
            ScopeResource::Catalog if authorize_catalog_requester(state, requester).is_ok() => grants.push(Grant {
                projects: vec![Glob::new(CATALOG_GRANT)],
                actions: BTreeSet::from([Action::Read]),
            }),
            ScopeResource::Catalog => {}
        }
    }
    grants
}

enum ScopeResource {
    Repository(String),
    Catalog,
}

struct RequestedScope {
    resource: ScopeResource,
    actions: BTreeSet<Action>,
}

/// Validate the request's one service and return its scopes. A client sends one `scope` per
/// repository, or several space-separated in one parameter; both spellings are accepted.
fn parse_token_request(query: &str, audience: &str) -> Option<Vec<RequestedScope>> {
    let mut requested_service = None;
    let mut scopes = Vec::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "service" if requested_service.replace(value == audience).is_some() => return None,
            "scope" => scopes.extend(value.split(' ').filter_map(parse_scope)),
            _ => {}
        }
    }
    requested_service?.then_some(scopes)
}

fn parse_scope(scope: &str) -> Option<RequestedScope> {
    let fields: Vec<&str> = scope.splitn(3, ':').collect();
    let [kind, name, actions] = fields[..] else {
        return None;
    };
    match kind {
        "repository" if !name.is_empty() => Some(RequestedScope {
            resource: ScopeResource::Repository(name.to_owned()),
            actions: actions
                .split(',')
                .flat_map(|verb| scope_actions(verb).iter().copied())
                .collect(),
        }),
        "registry" if name == "catalog" && actions.split(',').any(|action| action == "*") => Some(RequestedScope {
            resource: ScopeResource::Catalog,
            actions: BTreeSet::from([Action::Read]),
        }),
        _ => None,
    }
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
    let presented = identify(state, &index.acl, headers);
    presented
        .authorize(&index.acl, repo, name, Action::Read)
        .map_err(|denial| resource_challenge(state, headers, name, Action::Read, denial, presented.bad_token()))
}

pub(super) fn authorize_catalog(state: &ServingState, headers: &HeaderMap) -> Result<(), Response> {
    if let Some(token) = authorization(headers).and_then(|header| strip_auth_scheme(header, "Bearer"))
        && let Some(signer) = &state.signer
    {
        return match signer.verify(token) {
            Ok((_, grants)) => authorize_exact_grants(&grants, CATALOG_GRANT, Action::Read)
                .map_err(|denial| access_challenge(state, headers, CATALOG_SCOPE, denial, false)),
            Err(_) => Err(access_challenge(state, headers, CATALOG_SCOPE, Denial::Forbidden, true)),
        };
    }
    let requester = authorization(headers)
        .filter(|header| strip_auth_scheme(header, "Basic").is_some())
        .and_then(|header| named_requester(state, header))
        .unwrap_or(TokenRequester {
            principal: Principal::Anonymous,
            basic: None,
        });
    authorize_catalog_requester(state, &requester)
        .map_err(|denial| access_challenge(state, headers, CATALOG_SCOPE, denial, false))
}

fn authorize_catalog_requester(state: &ServingState, requester: &TokenRequester<'_>) -> Result<(), Denial> {
    for index in state.indexes.iter().filter(|index| index.ecosystem == Ecosystem::Oci) {
        if index.acl.anonymous_read {
            continue;
        }
        let principal = if let Some(basic) = requester.basic {
            let principal = index.acl.identify(Some(basic.header), basic.now).principal;
            if principal != requester.principal {
                return Err(Denial::Forbidden);
            }
            principal
        } else {
            Principal::Anonymous
        };
        authorize_all(&principal, &index.acl, Action::Read)?;
    }
    Ok(())
}

/// Resolve a resource credential, retaining a verified bearer's embedded grants for authorization.
pub(super) fn identify(state: &ServingState, acl: &IndexAcl, headers: &HeaderMap) -> PresentedIdentity {
    let header = authorization(headers);
    if let Some(token) = header.and_then(|header| strip_auth_scheme(header, "Bearer"))
        && let Some(signer) = &state.signer
    {
        return match signer.verify(token) {
            Ok((principal, grants)) => PresentedIdentity {
                identity: Identity { principal, user: None },
                authorization: PresentedAuthorization::Bearer(grants),
            },
            Err(_) => PresentedIdentity {
                identity: Identity {
                    principal: Principal::Anonymous,
                    user: None,
                },
                authorization: PresentedAuthorization::InvalidBearer,
            },
        };
    }
    PresentedIdentity {
        identity: acl.identify(header, (state.clock)()),
        authorization: PresentedAuthorization::Acl,
    }
}

pub(super) struct PresentedIdentity {
    identity: Identity,
    authorization: PresentedAuthorization,
}

enum PresentedAuthorization {
    Acl,
    Bearer(Vec<Grant>),
    InvalidBearer,
}

impl PresentedIdentity {
    pub(super) fn authorize(
        &self,
        acl: &IndexAcl,
        repository: &str,
        resource: &str,
        action: Action,
    ) -> Result<(), Denial> {
        match &self.authorization {
            PresentedAuthorization::Bearer(grants) => authorize_grants(grants, Some(resource), action),
            PresentedAuthorization::Acl | PresentedAuthorization::InvalidBearer => {
                authorize(&self.identity.principal, acl, Some(repository), action)
            }
        }
    }

    pub(super) fn into_identity(self) -> Identity {
        self.identity
    }

    pub(super) const fn bad_token(&self) -> bool {
        matches!(self.authorization, PresentedAuthorization::InvalidBearer)
    }
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
    access_challenge(state, headers, &resource_scope(name, action), denial, bad_token)
}

fn access_challenge(
    state: &ServingState,
    headers: &HeaderMap,
    scope: &str,
    denial: Denial,
    bad_token: bool,
) -> Response {
    let Some(signer) = &state.signer else {
        return if matches!(denial, Denial::Forbidden) {
            error_response(ErrorCode::Denied, "token does not grant this action")
        } else {
            basic_challenge()
        };
    };
    let error = if bad_token {
        Some("invalid_token")
    } else if matches!(denial, Denial::Forbidden) {
        Some("insufficient_scope")
    } else {
        None
    };
    challenge(signer.audience(), headers, Some(scope), error)
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
fn challenge(service: &str, headers: &HeaderMap, scope: Option<&str>, error: Option<&str>) -> Response {
    use std::fmt::Write as _;
    let mut value = format!("Bearer realm=\"{}\",service=\"{service}\"", realm(headers));
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
    use super::{Action, resource_scope, scope_actions};

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
