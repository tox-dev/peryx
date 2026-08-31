//! Keep protected fields out of serialization, caller-specific responses out of shared caches, and a
//! browser's own defences on every response the process emits.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use peryx_driver::authz::ScopedDecision;
use peryx_driver::state::AppState;
use pin_project_lite::pin_project;
use serde_json::{Map, Value};
use tower::{Layer, Service};

use crate::handlers::discover::trusts_proxy;

/// The least authority a response field requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldClassification {
    Public,
    Repository,
    Operator,
    Administrator,
}

impl FieldClassification {
    const fn visible_to(self, audience: Self) -> bool {
        matches!(
            (self, audience),
            (Self::Public, _)
                | (Self::Repository, Self::Repository | Self::Administrator)
                | (Self::Operator, Self::Operator | Self::Administrator)
                | (Self::Administrator, Self::Administrator)
        )
    }
}

/// Public access, an allowed repository token, or a role decision bound to the checked scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseAuthorization {
    Public,
    /// The caller passed a repository's token ACL before response construction.
    Repository,
    Scoped(ScopedDecision),
}

impl ResponseAuthorization {
    #[must_use]
    pub const fn field_class(self) -> Option<FieldClassification> {
        match self.classification() {
            Ok(class) => Some(class),
            Err(ResponseDenied) => None,
        }
    }

    const fn classification(self) -> Result<FieldClassification, ResponseDenied> {
        let authorization = match self {
            Self::Public => return Ok(FieldClassification::Public),
            Self::Repository => return Ok(FieldClassification::Repository),
            Self::Scoped(authorization) => authorization,
        };
        if !authorization.decision().is_allowed() {
            return Err(ResponseDenied);
        }
        match authorization.scope() {
            peryx_identity::Scope::OperatorRead | peryx_identity::Scope::AnalyticsRead => {
                Ok(FieldClassification::Operator)
            }
            peryx_identity::Scope::AdministrationRead | peryx_identity::Scope::AdministrationWrite => {
                Ok(FieldClassification::Administrator)
            }
            peryx_identity::Scope::RepositoryRead
            | peryx_identity::Scope::RepositoryWrite
            | peryx_identity::Scope::RepositoryDelete => Ok(FieldClassification::Repository),
        }
    }
}

/// Couples a field with its classification so routes cannot add an unclassified value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedField {
    name: &'static str,
    classification: FieldClassification,
    value: Value,
}

impl ClassifiedField {
    #[must_use]
    pub const fn new(name: &'static str, classification: FieldClassification, value: Value) -> Self {
        Self {
            name,
            classification,
            value,
        }
    }
}

/// A generic denial that carries no resource, path, or query data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseDenied;

impl fmt::Display for ResponseDenied {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("response access denied")
    }
}

impl std::error::Error for ResponseDenied {}

/// A route must classify every top-level field. Classify a nested object at the highest level any
/// value inside it requires, or filter that object before adding it.
///
/// # Errors
/// Returns [`ResponseDenied`] when authorization failed. The error contains no request data.
pub fn filter_fields(
    authorization: ResponseAuthorization,
    fields: impl IntoIterator<Item = ClassifiedField>,
) -> Result<Map<String, Value>, ResponseDenied> {
    let audience = authorization.classification()?;
    Ok(fields
        .into_iter()
        .filter(|field| field.classification.visible_to(audience))
        .map(|field| (field.name.to_owned(), field.value))
        .collect())
}

/// Prevent authenticated output from entering a shared cache or any cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectedCachePolicy {
    Private,
    NoStore,
}

impl ProtectedCachePolicy {
    pub fn apply(self, headers: &mut HeaderMap) {
        headers.insert(
            header::CACHE_CONTROL,
            match self {
                Self::Private => HeaderValue::from_static("private, no-cache"),
                Self::NoStore => HeaderValue::from_static("no-store"),
            },
        );
    }
}

/// Rejects framing and plugin content and pins the document's base URI. Script sources stay
/// unconstrained: the rendered shell carries the hydration bootstrap inline, so a `script-src`
/// without `'unsafe-inline'` would blank every page.
const HTML_SECURITY_POLICY: HeaderValue =
    HeaderValue::from_static("frame-ancestors 'none'; base-uri 'none'; object-src 'none'");

/// One year, without `includeSubDomains`: peryx answers for the host it was dialled on and cannot
/// know which sibling names the operator also owns, let alone whether those speak TLS.
const TRANSPORT_SECURITY: HeaderValue = HeaderValue::from_static("max-age=31536000");

/// Wrap the whole request surface in the browser defences its handlers did not choose for themselves.
///
/// These sit outside every other layer, so they also reach the framework's own 404 and 405 replies,
/// the rate limiter's rejections, and the static assets that deliberately stay outside request
/// accounting. Each header is a default: a handler or a trusted proxy that set a stricter value
/// keeps it, and no cache header is touched.
///
/// Every response the process emits pays for this layer and the serving path is cache-bound rather
/// than instruction-bound, so the whole policy is one `Router::layer` call: one boxed service, one
/// future, and one pass over the header map. The transport verdict is settled here, when the router
/// is built, leaving the per-response work to writing.
pub(crate) fn secure_responses(router: Router, state: &Arc<AppState>) -> Router {
    router.layer(BrowserDefaults {
        transport: if state.serving.tls_terminated {
            Transport::Terminated
        } else if state.serving.rate_limits.trusts_any_proxy() {
            Transport::Forwarded(Arc::clone(state))
        } else {
            Transport::Cleartext
        },
    })
}

/// Only a scheme forwarded by a proxy has to be read off the request, and it can only ever arrive
/// where a proxy is trusted, so that is the one arm a response pays a request read for.
#[derive(Clone)]
enum Transport {
    Cleartext,
    Terminated,
    Forwarded(Arc<AppState>),
}

impl Transport {
    fn pins(&self, request: &Request) -> bool {
        match self {
            Self::Cleartext => false,
            Self::Terminated => true,
            Self::Forwarded(state) => forwarded_over_tls(state, request),
        }
    }
}

/// A forwarded scheme is evidence only from a configured trusted proxy. Anyone else claiming
/// `https` would otherwise pin a host that peryx serves over cleartext, locking clients out of it.
fn forwarded_over_tls(state: &AppState, request: &Request) -> bool {
    trusts_proxy(state, request)
        && request
            .headers()
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .is_some_and(|scheme| scheme.trim().eq_ignore_ascii_case("https"))
}

#[derive(Clone)]
struct BrowserDefaults {
    transport: Transport,
}

impl<S> Layer<S> for BrowserDefaults {
    type Service = SecuredResponses<S>;

    fn layer(&self, inner: S) -> Self::Service {
        SecuredResponses {
            inner,
            transport: self.transport.clone(),
        }
    }
}

#[derive(Clone)]
struct SecuredResponses<S> {
    inner: S,
    transport: Transport,
}

impl<S> Service<Request> for SecuredResponses<S>
where
    S: Service<Request, Response = Response>,
{
    type Response = Response;
    type Error = S::Error;
    type Future = SecuredResponse<S::Future>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let pins_transport = self.transport.pins(&request);
        SecuredResponse {
            inner: self.inner.call(request),
            pins_transport,
        }
    }
}

pin_project! {
    struct SecuredResponse<F> {
        #[pin]
        inner: F,
        pins_transport: bool,
    }
}

impl<F, E> Future for SecuredResponse<F>
where
    F: Future<Output = Result<Response, E>>,
{
    type Output = Result<Response, E>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut response = ready!(this.inner.poll(context))?;
        apply_defaults(response.headers_mut(), *this.pins_transport);
        Poll::Ready(Ok(response))
    }
}

/// Each header is a default, so an entry a handler already filled stays as it is. Framing, plugin
/// and referrer rules protect rendered documents, so they follow the response's media type rather
/// than the route.
fn apply_defaults(headers: &mut HeaderMap, pins_transport: bool) {
    headers
        .entry(header::X_CONTENT_TYPE_OPTIONS)
        .or_insert(HeaderValue::from_static("nosniff"));
    if is_html(headers) {
        headers
            .entry(header::CONTENT_SECURITY_POLICY)
            .or_insert(HTML_SECURITY_POLICY);
        headers
            .entry(header::X_FRAME_OPTIONS)
            .or_insert(HeaderValue::from_static("DENY"));
        headers
            .entry(header::REFERRER_POLICY)
            .or_insert(HeaderValue::from_static("no-referrer"));
    }
    if pins_transport {
        headers
            .entry(header::STRICT_TRANSPORT_SECURITY)
            .or_insert(TRANSPORT_SECURITY);
    }
}

/// `text/html; charset=utf-8` is as much a document as bare `text/html`, so the essence decides.
/// Comparing the raw bytes keeps a full media-type parse off every response the process emits.
fn is_html(headers: &HeaderMap) -> bool {
    const HTML: &[u8] = b"text/html";

    headers.get(header::CONTENT_TYPE).is_some_and(|content_type| {
        let Some((essence, rest)) = content_type.as_bytes().split_at_checked(HTML.len()) else {
            return false;
        };
        essence.eq_ignore_ascii_case(HTML) && rest.first().is_none_or(|byte| matches!(byte, b';' | b' ' | b'\t'))
    })
}
