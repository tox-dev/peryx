//! The `PyPI` Simple-repository protocol over the neutral upstream transport.
//!
//! [`UpstreamClient`] is ecosystem-neutral HTTP: it sends conditional `GET`s and streams files, and
//! knows nothing about PEP 503/691. This module is the seam where those neutral sends become
//! Simple-API document fetches — the `Accept` negotiation, the content-type validation, and the
//! `x-pypi-last-serial`/`Cache-Control` parsing that only the `PyPI` ecosystem cares about. Status is
//! kept agnostic: `304` and `404` come back to the caller rather than raised, so the cache layer
//! decides what to do.

use std::future::Future;

use bytes::Bytes;
use futures_util::Stream;
use peryx_upstream::retry::{MAX_RETRIES, should_retry_error, sleep_before_retry};
use peryx_upstream::{NamedUpstream, UpstreamClient, UpstreamError, UpstreamRouter};
use reqwest::StatusCode;
use reqwest::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, HeaderMap, HeaderName};
use url::Url;

/// The `Accept` header peryx sends upstream: PEP 691 JSON first, then PEP 503 HTML.
pub const ACCEPT_SIMPLE: &str =
    "application/vnd.pypi.simple.v1+json, application/vnd.pypi.simple.v1+html;q=0.2, text/html;q=0.01";

/// A response to an upstream simple-page fetch. Kept status-agnostic: `304` and `404` are returned
/// to the caller rather than raised, so the cache layer decides what to do.
#[derive(Debug, Clone)]
pub struct SimpleResponse {
    pub status: u16,
    /// The configured source that answered a routed request; absent for a legacy single upstream.
    pub source: Option<String>,
    /// The final URL fetched (after redirects), used as the base for resolving relative HTML links.
    pub url: Url,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_serial: Option<u64>,
    /// The freshness lifetime upstream granted via `Cache-Control`; `None` when the response
    /// carried no positive lifetime (absent header, `no-cache`, `no-store`, or zero).
    pub max_age: Option<i64>,
    pub body: Bytes,
}

/// The headers of a simple-page fetch with the body still open, for streaming.
#[derive(Debug)]
pub struct SimpleHead {
    pub status: u16,
    /// The configured source that answered a routed request; absent for a legacy single upstream.
    pub source: Option<String>,
    /// The final URL fetched (after redirects), the base for resolving relative HTML links.
    pub url: Url,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_serial: Option<u64>,
    /// The freshness lifetime upstream granted via `Cache-Control`; `None` when the response
    /// carried no positive lifetime (absent header, `no-cache`, `no-store`, or zero).
    pub max_age: Option<i64>,
    response: reqwest::Response,
}

impl SimpleHead {
    /// Read the whole body.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Http`] if the transfer fails.
    pub async fn bytes(self) -> Result<Bytes, UpstreamError> {
        Ok(self.response.bytes().await?)
    }

    /// Consume the body as a stream of chunks.
    pub fn into_stream(self) -> impl Stream<Item = Result<Bytes, UpstreamError>> + Send + use<> {
        use futures_util::TryStreamExt as _;
        self.response.bytes_stream().map_err(UpstreamError::from)
    }
}

/// Fetch a project's index document, then the project list, then a file's bytes — the `PyPI` Simple
/// protocol layered over an [`UpstreamClient`] as an extension trait so call sites keep method syntax.
pub trait SimpleClientExt {
    /// Fetch a project's simple page, optionally revalidating with `If-None-Match`.
    fn fetch_project(
        &self,
        project: &str,
        etag: Option<&str>,
    ) -> impl Future<Output = Result<SimpleResponse, UpstreamError>> + Send;

    /// Fetch the upstream root project list.
    fn fetch_index(&self) -> impl Future<Output = Result<SimpleResponse, UpstreamError>> + Send;

    /// Start fetching a project's simple page, returning its headers and the open body, so callers
    /// can stream the bytes as they arrive instead of buffering the page.
    fn head_project(
        &self,
        project: &str,
        etag: Option<&str>,
    ) -> impl Future<Output = Result<SimpleHead, UpstreamError>> + Send;
}

impl SimpleClientExt for UpstreamClient {
    async fn fetch_project(&self, project: &str, etag: Option<&str>) -> Result<SimpleResponse, UpstreamError> {
        let url = self.base().join(&format!("{project}/"))?;
        fetch_simple(self, url, etag).await
    }

    async fn fetch_index(&self) -> Result<SimpleResponse, UpstreamError> {
        fetch_simple(self, self.base().clone(), None).await
    }

    async fn head_project(&self, project: &str, etag: Option<&str>) -> Result<SimpleHead, UpstreamError> {
        let url = self.base().join(&format!("{project}/"))?;
        simple_head(self.send_conditional(url, ACCEPT_SIMPLE, etag).await?)
    }
}

impl SimpleClientExt for UpstreamRouter {
    async fn fetch_project(&self, project: &str, _etag: Option<&str>) -> Result<SimpleResponse, UpstreamError> {
        let mut candidates = self.candidates(project).peekable();
        loop {
            let upstream = candidates.next().expect("an upstream route always has a candidate");
            let result = SimpleClientExt::fetch_project(upstream.client(), project, None).await;
            record_health(upstream, &result);
            if fallback_result(&result) && candidates.peek().is_some() {
                tracing::warn!(project, upstream = upstream.name(), "trying fallback");
                continue;
            }
            return attribute_source(upstream, result);
        }
    }

    async fn fetch_index(&self) -> Result<SimpleResponse, UpstreamError> {
        let mut candidates = self.candidates("").peekable();
        loop {
            let upstream = candidates.next().expect("an upstream route always has a candidate");
            let result = SimpleClientExt::fetch_index(upstream.client()).await;
            record_health(upstream, &result);
            if fallback_result(&result) && candidates.peek().is_some() {
                tracing::warn!(upstream = upstream.name(), "upstream unavailable, trying fallback");
                continue;
            }
            return attribute_source(upstream, result);
        }
    }

    async fn head_project(&self, project: &str, _etag: Option<&str>) -> Result<SimpleHead, UpstreamError> {
        let mut candidates = self.candidates(project).peekable();
        loop {
            let upstream = candidates.next().expect("an upstream route always has a candidate");
            let result = upstream.client().head_project(project, None).await;
            record_health(upstream, &result);
            if fallback_result(&result) && candidates.peek().is_some() {
                tracing::warn!(project, upstream = upstream.name(), "trying fallback");
                continue;
            }
            return attribute_source(upstream, result);
        }
    }
}

fn attribute_source<T: SimpleStatus>(
    upstream: &NamedUpstream,
    result: Result<T, UpstreamError>,
) -> Result<T, UpstreamError> {
    result.map(|mut response| {
        let upstream = upstream.name().to_owned();
        let status = response.status();
        tracing::debug!(upstream, status, "upstream source answered");
        response.set_source(upstream);
        response
    })
}

fn record_health<T: SimpleStatus>(upstream: &NamedUpstream, result: &Result<T, UpstreamError>) {
    if matches!(result, Ok(response) if matches!(response.status(), 200 | 304 | 404)) {
        upstream.mark_healthy();
    } else {
        upstream.mark_unhealthy();
    }
}

fn fallback_result<T: SimpleStatus>(result: &Result<T, UpstreamError>) -> bool {
    match result {
        Ok(response) => matches!(response.status(), 404 | 429 | 500..=599),
        Err(UpstreamError::Http(_)) => true,
        Err(
            UpstreamError::Url(_)
            | UpstreamError::MissingContentType { .. }
            | UpstreamError::UnsupportedContentType { .. }
            | UpstreamError::ResponseTooLarge { .. },
        ) => false,
    }
}

trait SimpleStatus {
    fn status(&self) -> u16;
    fn set_source(&mut self, source: String);
}

impl SimpleStatus for SimpleResponse {
    fn status(&self) -> u16 {
        self.status
    }

    fn set_source(&mut self, source: String) {
        self.source = Some(source);
    }
}

impl SimpleStatus for SimpleHead {
    fn status(&self) -> u16 {
        self.status
    }

    fn set_source(&mut self, source: String) {
        self.source = Some(source);
    }
}

async fn fetch_simple(client: &UpstreamClient, url: Url, etag: Option<&str>) -> Result<SimpleResponse, UpstreamError> {
    let mut attempt = 0;
    loop {
        let response = client.send_conditional(url.clone(), ACCEPT_SIMPLE, etag).await?;
        let head = simple_head(response)?;
        match head.response.bytes().await {
            Ok(body) => {
                return Ok(SimpleResponse {
                    status: head.status,
                    source: head.source,
                    url: head.url,
                    content_type: head.content_type,
                    etag: head.etag,
                    last_serial: head.last_serial,
                    max_age: head.max_age,
                    body,
                });
            }
            Err(err) if should_retry_error(&err) && attempt < MAX_RETRIES => {
                sleep_before_retry(&head.url, attempt, &err).await;
                attempt += 1;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn header_str(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

fn simple_head(response: reqwest::Response) -> Result<SimpleHead, UpstreamError> {
    let headers = response.headers();
    let content_type = header_str(headers, &CONTENT_TYPE);
    if response.status() == StatusCode::OK {
        validate_simple_content_type(response.url(), content_type.as_deref())?;
    }
    Ok(SimpleHead {
        status: response.status().as_u16(),
        source: None,
        url: response.url().clone(),
        content_type,
        etag: header_str(headers, &ETAG),
        last_serial: header_str(headers, &HeaderName::from_static("x-pypi-last-serial"))
            .and_then(|value| value.parse().ok()),
        max_age: max_age(headers),
        response,
    })
}

fn validate_simple_content_type(url: &Url, content_type: Option<&str>) -> Result<(), UpstreamError> {
    let Some(content_type) = content_type else {
        return Err(UpstreamError::MissingContentType { url: url.clone() });
    };
    let media_type = content_type
        .split_once(';')
        .map_or(content_type, |(media_type, _)| media_type)
        .trim()
        .to_ascii_lowercase();
    if matches!(
        media_type.as_str(),
        "application/vnd.pypi.simple.v1+json" | "application/vnd.pypi.simple.v1+html" | "text/html"
    ) {
        return Ok(());
    }
    Err(UpstreamError::UnsupportedContentType {
        url: url.clone(),
        content_type: content_type.to_owned(),
    })
}

/// The freshness lifetime a `Cache-Control` header grants a shared cache: `s-maxage` beats
/// `max-age`, `no-cache`/`no-store` disable caching, and a non-positive lifetime counts as none.
fn max_age(headers: &HeaderMap) -> Option<i64> {
    let value = headers.get(CACHE_CONTROL)?.to_str().ok()?;
    let mut max_age = None;
    let mut s_maxage = None;
    for directive in value.split(',') {
        let directive = directive.trim().to_ascii_lowercase();
        if directive == "no-cache" || directive == "no-store" {
            return None;
        }
        if let Some(secs) = directive.strip_prefix("max-age=").and_then(|v| v.parse().ok()) {
            max_age = Some(secs);
        }
        if let Some(secs) = directive.strip_prefix("s-maxage=").and_then(|v| v.parse().ok()) {
            s_maxage = Some(secs);
        }
    }
    s_maxage.or(max_age).filter(|&secs| secs > 0)
}

/// The upstream fetch protocol a proxy index speaks.
///
/// A proxy revalidates and caches an upstream's index documents and files. This trait is the seam
/// that logic plugs into: [`UpstreamClient`] speaks the `PyPI` PEP 503/691 simple API here, and an OCI
/// registry (`/v2/`) or an npm registry are sibling ecosystems. It is dispatched **statically**: one
/// concrete client today, an enum per proxy once a second protocol dispatches through it, never a
/// boxed object, so proxying costs nothing over calling the client directly. Parsing the returned
/// document is the ecosystem driver's job; this trait only fetches.
///
/// Returns are written as `impl Future + Send` rather than `async fn` so callers can spawn the futures
/// on a multi-threaded runtime without the trait dictating auto-trait bounds.
pub trait UpstreamProtocol {
    /// Fetch a project's index document, conditional on `etag`.
    fn fetch_project(
        &self,
        project: &str,
        etag: Option<&str>,
    ) -> impl Future<Output = Result<SimpleResponse, UpstreamError>> + Send;

    /// Fetch the full project list.
    fn fetch_index(&self) -> impl Future<Output = Result<SimpleResponse, UpstreamError>> + Send;

    /// Fetch a file's bytes by URL.
    fn fetch_bytes(&self, url: &str) -> impl Future<Output = Result<Bytes, UpstreamError>> + Send;
}

impl UpstreamProtocol for UpstreamClient {
    fn fetch_project(
        &self,
        project: &str,
        etag: Option<&str>,
    ) -> impl Future<Output = Result<SimpleResponse, UpstreamError>> + Send {
        SimpleClientExt::fetch_project(self, project, etag)
    }

    fn fetch_index(&self) -> impl Future<Output = Result<SimpleResponse, UpstreamError>> + Send {
        SimpleClientExt::fetch_index(self)
    }

    fn fetch_bytes(&self, url: &str) -> impl Future<Output = Result<Bytes, UpstreamError>> + Send {
        Self::fetch_bytes(self, url)
    }
}
