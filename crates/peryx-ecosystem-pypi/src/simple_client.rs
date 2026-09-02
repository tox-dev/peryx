//! Preserves `304` and `404` so the Simple-page cache can apply its own policy.

use std::collections::VecDeque;
use std::future::Future;

use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use peryx_upstream::retry::{MAX_RETRIES, should_retry_error};
use peryx_upstream::{NamedUpstream, UpstreamClient, UpstreamError, UpstreamRouter};
use reqwest::StatusCode;
use reqwest::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, HeaderMap, HeaderName, LAST_MODIFIED, RETRY_AFTER,
};
use url::Url;

/// The `Accept` header peryx sends upstream: PEP 691 JSON first, then PEP 503 HTML.
pub const ACCEPT_SIMPLE: &str =
    "application/vnd.pypi.simple.v1+json, application/vnd.pypi.simple.v1+html;q=0.2, text/html;q=0.01";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseCachePolicy {
    pub fresh_secs: Option<i64>,
    pub must_revalidate: Option<bool>,
    pub storable: bool,
}

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
    pub last_modified: Option<String>,
    pub retry_after: Option<String>,
    pub last_serial: Option<u64>,
    /// The freshness lifetime upstream granted via `Cache-Control`; `None` when absent and zero when
    /// the response is stale or requires immediate revalidation.
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
    pub last_modified: Option<String>,
    pub retry_after: Option<String>,
    pub content_length: Option<u64>,
    pub last_serial: Option<u64>,
    /// The freshness lifetime upstream granted via `Cache-Control`; `None` when absent and zero when
    /// the response is stale or requires immediate revalidation.
    pub max_age: Option<i64>,
    response: reqwest::Response,
}

impl SimpleHead {
    /// # Errors
    /// Returns [`UpstreamError::Http`] if the transfer fails.
    pub async fn bytes(self) -> Result<Bytes, UpstreamError> {
        read_capped(Box::pin(self.response.bytes_stream()), MAX_SIMPLE_PAGE_BYTES).await
    }

    pub fn into_stream(self) -> impl Stream<Item = Result<Bytes, UpstreamError>> + Send + use<> {
        use futures_util::TryStreamExt as _;
        self.response.bytes_stream().map_err(UpstreamError::from)
    }
}

/// The stored response a conditional request may revalidate: the source that produced it and the
/// validators it arrived with.
///
/// [RFC 9111 section 4.3.1](https://www.rfc-editor.org/rfc/rfc9111.html#section-4.3.1) binds a
/// validator to the one stored response it was received with, so `source` names the routed candidate
/// that answered it. A router sends the pair only to that candidate and calls every other one
/// unconditionally. `source` is `None` for a record written before peryx tracked the answering
/// source, which no candidate can claim, so such a record revalidates nothing until it is refetched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CachedValidators<'a> {
    pub source: Option<&'a str>,
    pub etag: Option<&'a str>,
    pub last_modified: Option<&'a str>,
}

impl CachedValidators<'_> {
    /// The validators `candidate` may be asked about: the stored pair when it answered the stored
    /// response, and nothing at all otherwise.
    #[must_use]
    fn for_candidate(self, candidate: &str) -> Self {
        if self.source == Some(candidate) {
            self
        } else {
            Self::default()
        }
    }
}

/// Fetch a project's index document, then the project list, then a file's bytes - the `PyPI` Simple
/// protocol layered over an [`UpstreamClient`] as an extension trait so call sites keep method syntax.
pub trait SimpleClientExt {
    fn fetch_project(
        &self,
        project: &str,
        validators: CachedValidators<'_>,
    ) -> impl Future<Output = Result<SimpleResponse, UpstreamError>> + Send;

    fn fetch_index(&self) -> impl Future<Output = Result<SimpleResponse, UpstreamError>> + Send;

    fn head_index(
        &self,
        validators: CachedValidators<'_>,
    ) -> impl Future<Output = Result<SimpleHead, UpstreamError>> + Send;

    /// Start fetching a project's simple page, returning its headers and the open body, so callers
    /// can stream the bytes as they arrive instead of buffering the page.
    fn head_project(
        &self,
        project: &str,
        validators: CachedValidators<'_>,
    ) -> impl Future<Output = Result<SimpleHead, UpstreamError>> + Send;
}

impl SimpleClientExt for UpstreamClient {
    async fn fetch_project(
        &self,
        project: &str,
        validators: CachedValidators<'_>,
    ) -> Result<SimpleResponse, UpstreamError> {
        fetch_simple(self, simple_project_url(self, project)?, validators).await
    }

    async fn fetch_index(&self) -> Result<SimpleResponse, UpstreamError> {
        fetch_simple(self, simple_index_url(self), CachedValidators::default()).await
    }

    async fn head_index(&self, validators: CachedValidators<'_>) -> Result<SimpleHead, UpstreamError> {
        head_simple(self, simple_index_url(self), validators).await
    }

    async fn head_project(&self, project: &str, validators: CachedValidators<'_>) -> Result<SimpleHead, UpstreamError> {
        head_simple(self, simple_project_url(self, project)?, validators).await
    }
}

async fn head_simple(
    client: &UpstreamClient,
    url: Url,
    validators: CachedValidators<'_>,
) -> Result<SimpleHead, UpstreamError> {
    simple_head(
        client
            .send_validated(url, ACCEPT_SIMPLE, validators.etag, validators.last_modified)
            .await?,
    )
}

fn simple_index_url(client: &UpstreamClient) -> Url {
    client.base().clone()
}

fn simple_project_url(client: &UpstreamClient, project: &str) -> Result<Url, UpstreamError> {
    Ok(simple_index_url(client).join(&format!("{project}/"))?)
}

impl SimpleClientExt for UpstreamRouter {
    async fn fetch_project(
        &self,
        project: &str,
        validators: CachedValidators<'_>,
    ) -> Result<SimpleResponse, UpstreamError> {
        let mut candidates = NonEmptyCandidates::new(self, project);
        loop {
            let upstream = candidates.current();
            let result =
                SimpleClientExt::fetch_project(upstream.client(), project, validators.for_candidate(upstream.name()))
                    .await;
            record_health(upstream, &result);
            if fallback_result(result.as_ref().map(SimpleStatus::status)) && candidates.advance() {
                tracing::warn!(project, upstream = upstream.name(), "trying fallback");
                continue;
            }
            return attribute_source(upstream, result);
        }
    }

    async fn fetch_index(&self) -> Result<SimpleResponse, UpstreamError> {
        let mut candidates = NonEmptyCandidates::new(self, "");
        loop {
            let upstream = candidates.current();
            let result = SimpleClientExt::fetch_index(upstream.client()).await;
            record_health(upstream, &result);
            if fallback_result(result.as_ref().map(SimpleStatus::status)) && candidates.advance() {
                tracing::warn!(upstream = upstream.name(), "upstream unavailable, trying fallback");
                continue;
            }
            return attribute_source(upstream, result);
        }
    }

    async fn head_index(&self, validators: CachedValidators<'_>) -> Result<SimpleHead, UpstreamError> {
        let mut candidates = NonEmptyCandidates::new(self, "");
        loop {
            let upstream = candidates.current();
            let result = upstream
                .client()
                .head_index(validators.for_candidate(upstream.name()))
                .await;
            record_health(upstream, &result);
            if fallback_result(result.as_ref().map(SimpleStatus::status)) && candidates.advance() {
                tracing::warn!(upstream = upstream.name(), "upstream unavailable, trying fallback");
                continue;
            }
            return attribute_source(upstream, result);
        }
    }

    async fn head_project(&self, project: &str, validators: CachedValidators<'_>) -> Result<SimpleHead, UpstreamError> {
        let mut candidates = NonEmptyCandidates::new(self, project);
        loop {
            let upstream = candidates.current();
            let result = upstream
                .client()
                .head_project(project, validators.for_candidate(upstream.name()))
                .await;
            record_health(upstream, &result);
            if fallback_result(result.as_ref().map(SimpleStatus::status)) && candidates.advance() {
                tracing::warn!(project, upstream = upstream.name(), "trying fallback");
                continue;
            }
            return attribute_source(upstream, result);
        }
    }
}

struct NonEmptyCandidates<'a> {
    current: &'a NamedUpstream,
    remaining: VecDeque<&'a NamedUpstream>,
}

impl<'a> NonEmptyCandidates<'a> {
    fn new(router: &'a UpstreamRouter, key: &'a str) -> Self {
        let mut candidates = router.candidates(key);
        let current = candidates.next().expect("validated routes contain an upstream");
        Self {
            current,
            remaining: candidates.collect(),
        }
    }

    const fn current(&self) -> &'a NamedUpstream {
        self.current
    }

    fn advance(&mut self) -> bool {
        let Some(current) = self.remaining.pop_front() else {
            return false;
        };
        self.current = current;
        true
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

const fn fallback_result(result: Result<u16, &UpstreamError>) -> bool {
    match result {
        Ok(status) => matches!(status, 404 | 429 | 500..=599),
        Err(UpstreamError::Http(_) | UpstreamError::DeadlineExceeded) => true,
        Err(
            UpstreamError::Credential(_)
            | UpstreamError::Url(_)
            | UpstreamError::InvalidResponse { .. }
            | UpstreamError::ResponseTooLarge { .. }
            | UpstreamError::BlockedDestination { .. },
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

/// The ceiling for a buffered Simple page. A project page and the root index both persist under a
/// 256 MiB sync cap (`MAX_PROJECT_BYTES` and `MAX_CATALOG_BYTES`), so a live buffered read holds the
/// same ceiling: past it the upstream is broken or hostile, not serving a real page.
const MAX_SIMPLE_PAGE_BYTES: usize = 256 * 1024 * 1024;

/// Read a Simple-page body into memory under `limit`, counting the bytes as they stream so a chunked
/// or missing-`Content-Length` response cannot force an unbounded body into memory: the read fails
/// the instant a chunk would carry the running total past `limit`, before that chunk is buffered.
async fn read_capped(
    mut stream: std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    limit: usize,
) -> Result<Bytes, UpstreamError> {
    use futures_util::TryStreamExt as _;

    let mut body = BytesMut::new();
    while let Some(chunk) = stream.try_next().await? {
        if chunk.len() > limit - body.len() {
            return Err(UpstreamError::ResponseTooLarge { limit });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.freeze())
}

async fn fetch_simple(
    client: &UpstreamClient,
    url: Url,
    validators: CachedValidators<'_>,
) -> Result<SimpleResponse, UpstreamError> {
    let bounded = client.bounded_read();
    bounded
        .run(Box::pin(async {
            let mut attempt = 0;
            loop {
                let response = bounded
                    .send_validated(url.clone(), ACCEPT_SIMPLE, validators.etag, validators.last_modified)
                    .await?;
                let head = simple_head(response)?;
                match read_capped(Box::pin(head.response.bytes_stream()), MAX_SIMPLE_PAGE_BYTES).await {
                    Ok(body) => {
                        return Ok(SimpleResponse {
                            status: head.status,
                            source: head.source,
                            url: head.url,
                            content_type: head.content_type,
                            etag: head.etag,
                            last_modified: head.last_modified,
                            retry_after: head.retry_after,
                            last_serial: head.last_serial,
                            max_age: head.max_age,
                            body,
                        });
                    }
                    Err(UpstreamError::Http(err)) if should_retry_error(&err) && attempt < MAX_RETRIES => {
                        bounded.sleep_before_retry(&head.url, attempt, &err).await?;
                        attempt += 1;
                    }
                    Err(err) => return Err(err),
                }
            }
        }))
        .await
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
        last_modified: header_str(headers, &LAST_MODIFIED),
        retry_after: header_str(headers, &RETRY_AFTER),
        content_length: header_str(headers, &CONTENT_LENGTH).and_then(|value| value.parse().ok()),
        last_serial: header_str(headers, &HeaderName::from_static("x-pypi-last-serial"))
            .and_then(|value| value.parse().ok()),
        max_age: max_age(headers),
        response,
    })
}

fn validate_simple_content_type(url: &Url, content_type: Option<&str>) -> Result<(), UpstreamError> {
    let Some(content_type) = content_type else {
        return Err(UpstreamError::InvalidResponse {
            reason: format!("missing Simple API Content-Type from {url}"),
        });
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
    Err(UpstreamError::InvalidResponse {
        reason: format!("unsupported Simple API Content-Type {content_type:?} from {url}"),
    })
}

fn max_age(headers: &HeaderMap) -> Option<i64> {
    response_cache_policy(headers).fresh_secs
}

/// Parse the storage and revalidation directives a shared cache must apply to one response.
pub fn response_cache_policy(headers: &HeaderMap) -> ResponseCachePolicy {
    let mut values = headers.get_all(CACHE_CONTROL).iter();
    let Some(first) = values.next() else {
        return ResponseCachePolicy {
            fresh_secs: None,
            must_revalidate: None,
            storable: true,
        };
    };
    let mut max_age = DeltaSeconds::Absent;
    let mut s_maxage = DeltaSeconds::Absent;
    let mut invalid = false;
    let mut validate_before_reuse = false;
    let mut revalidate_when_stale = false;
    let mut storable = true;
    for value in std::iter::once(first).chain(values) {
        let Ok(value) = value.to_str() else {
            invalid = true;
            continue;
        };
        invalid |= !visit_cache_directives(value, |directive| {
            let directive = directive.trim();
            let (name, value) = directive
                .split_once('=')
                .map_or((directive, None), |(name, value)| (name, Some(value)));
            let normalized = name.trim();
            if normalized.eq_ignore_ascii_case("no-cache") {
                validate_before_reuse = true;
            } else if normalized.eq_ignore_ascii_case("must-revalidate")
                || normalized.eq_ignore_ascii_case("proxy-revalidate")
            {
                revalidate_when_stale = true;
            } else if normalized.eq_ignore_ascii_case("no-store") {
                validate_before_reuse = true;
                storable = false;
            } else if normalized.eq_ignore_ascii_case("private") {
                storable = false;
            } else if normalized.eq_ignore_ascii_case("max-age") {
                max_age.add(value, name == normalized);
            } else if normalized.eq_ignore_ascii_case("s-maxage") {
                s_maxage.add(value, name == normalized);
                revalidate_when_stale = true;
            }
        });
    }
    ResponseCachePolicy {
        fresh_secs: if validate_before_reuse || invalid || max_age.is_invalid() || s_maxage.is_invalid() {
            Some(0)
        } else {
            s_maxage.value().or_else(|| max_age.value())
        },
        must_revalidate: Some(validate_before_reuse || revalidate_when_stale),
        storable,
    }
}

enum DeltaSeconds {
    Absent,
    Value(i64),
    Invalid,
}

impl DeltaSeconds {
    fn add(&mut self, value: Option<&str>, valid_separator: bool) {
        *self = if matches!(self, Self::Absent) && valid_separator {
            value.and_then(delta_seconds).map_or(Self::Invalid, Self::Value)
        } else {
            Self::Invalid
        };
    }

    const fn value(&self) -> Option<i64> {
        match self {
            Self::Value(value) => Some(*value),
            Self::Absent | Self::Invalid => None,
        }
    }

    const fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid)
    }
}

fn visit_cache_directives(value: &str, mut visit: impl FnMut(&str)) -> bool {
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in value.bytes().enumerate() {
        if escaped {
            escaped = false;
        } else if quoted && byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            quoted = !quoted;
        } else if byte == b',' && !quoted {
            visit(&value[start..index]);
            start = index + 1;
        }
    }
    if quoted {
        return false;
    }
    visit(&value[start..]);
    true
}

fn delta_seconds(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if let Some(quoted) = bytes.strip_prefix(b"\"") {
        return quoted.strip_suffix(b"\"").and_then(quoted_delta_seconds);
    }
    decimal_delta_seconds(bytes)
}

fn quoted_delta_seconds(value: &[u8]) -> Option<i64> {
    if value.is_empty() {
        return None;
    }
    let mut seconds = 0_i64;
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'\\' {
            index += 1;
        }
        append_delta_digit(&mut seconds, value[index])?;
        index += 1;
    }
    Some(seconds)
}

fn decimal_delta_seconds(value: &[u8]) -> Option<i64> {
    if value.is_empty() {
        return None;
    }
    let mut seconds = 0_i64;
    for &byte in value {
        append_delta_digit(&mut seconds, byte)?;
    }
    Some(seconds)
}

fn append_delta_digit(seconds: &mut i64, byte: u8) -> Option<()> {
    let digit = byte.checked_sub(b'0').filter(|digit| *digit < 10)?;
    *seconds = seconds.saturating_mul(10).saturating_add(i64::from(digit));
    Some(())
}

/// The upstream fetch protocol a proxy index speaks.
///
/// A proxy revalidates and caches upstream index documents and files through this trait. Static
/// dispatch avoids allocation and virtual calls. The caller parses each returned document.
///
/// Returns are written as `impl Future + Send` rather than `async fn` so callers can spawn the futures
/// on a multi-threaded runtime without the trait dictating auto-trait bounds.
pub trait UpstreamProtocol {
    fn fetch_project(
        &self,
        project: &str,
        validators: CachedValidators<'_>,
    ) -> impl Future<Output = Result<SimpleResponse, UpstreamError>> + Send;

    fn fetch_index(&self) -> impl Future<Output = Result<SimpleResponse, UpstreamError>> + Send;

    fn fetch_bytes(&self, url: &str) -> impl Future<Output = Result<Bytes, UpstreamError>> + Send;
}

impl UpstreamProtocol for UpstreamClient {
    fn fetch_project(
        &self,
        project: &str,
        validators: CachedValidators<'_>,
    ) -> impl Future<Output = Result<SimpleResponse, UpstreamError>> + Send {
        SimpleClientExt::fetch_project(self, project, validators)
    }

    fn fetch_index(&self) -> impl Future<Output = Result<SimpleResponse, UpstreamError>> + Send {
        SimpleClientExt::fetch_index(self)
    }

    fn fetch_bytes(&self, url: &str) -> impl Future<Output = Result<Bytes, UpstreamError>> + Send {
        Self::fetch_bytes(self, url)
    }
}

#[cfg(test)]
#[path = "../tests/unit/simple_client/tests.rs"]
mod tests;
