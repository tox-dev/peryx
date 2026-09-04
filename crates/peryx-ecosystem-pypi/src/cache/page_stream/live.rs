//! The live upstream tee: forward a cold `PyPI` page to the client as it arrives while persisting
//! it, so a serial resolver waits on the network once, not twice.

use super::{
    Arc, Bytes, CacheError, CachedIndex, JSON_META_PREFLIGHT_BYTES, Observation, PageOutcome, PageSummary,
    PageTransformer, ServingState, UpstreamPermit, VecDeque, mirror_route, persist_streamed, release_flight,
    spawn_metadata_backfill, transform_error,
};
use crate::cache::fetch::canonical_json;
use crate::stream::{MAX_PAGE_BYTES, TransformError};

#[cfg(test)]
#[path = "../../../tests/unit/cache_coverage/live.rs"]
mod cache_coverage_tests;

pub(super) struct FreshJsonStream {
    pub(super) state: Arc<ServingState>,
    pub(super) key: String,
    pub(super) representation_key: String,
    pub(super) route: String,
    pub(super) cached_name: String,
    pub(super) project: String,
    pub(super) now: i64,
    pub(super) context: crate::stream::PageContext,
    pub(super) cached_present: bool,
    pub(super) guard: peryx_index::serving::FlightGuard,
    pub(super) head: crate::simple_client::SimpleHead,
    pub(super) permit: UpstreamPermit,
}

impl FreshJsonStream {
    pub(super) async fn stream(self) -> Result<PageOutcome, CacheError> {
        use futures_util::StreamExt as _;
        if self.cached_present {
            tracing::info!(key = %self.key, "upstream page changed");
            self.state.metrics.record(Observation::Refresh {
                repository: mirror_route(&self.state, &self.cached_name),
                resource: self.project.clone(),
                changed: true,
            });
        }

        let meta = PageMeta {
            source: self.head.source.clone(),
            etag: self.head.etag.clone(),
            last_modified: self.head.last_modified.clone(),
            last_serial: self.head.last_serial,
            fresh_secs: self.head.max_age,
            fetched_at_unix: self.now,
        };
        let last_serial = meta.last_serial;
        let base = self.head.url.clone();
        let mut context = self.context;
        context.base = Some(base.clone());
        let buffered_context = context.clone();
        let preflight =
            match preflight_json_stream(self.head.into_stream().boxed(), PageTransformer::new(context)).await {
                Ok(preflight) => preflight,
                Err(err) => {
                    release_flight(&self.state, &self.key, self.guard);
                    return Err(err);
                }
            };
        // Buffer until project status is known; streaming files first could expose quarantined files.
        let preflight = match preflight {
            JsonPreflight::Streaming {
                body, transformer, raw, ..
            } if !transformer.headers_known() => match buffer_whole_page(body, raw, buffered_context).await {
                Ok((raw, served, summary)) => JsonPreflight::Complete { raw, served, summary },
                Err(err) => {
                    release_flight(&self.state, &self.key, self.guard);
                    return Err(err);
                }
            },
            preflight => preflight,
        };
        match preflight {
            JsonPreflight::Streaming {
                body,
                transformer,
                raw,
                served,
                pending,
            } => Ok(PageOutcome::Streaming(
                live_stream(
                    self.state.clone(),
                    LiveStream {
                        body,
                        transformer: *transformer,
                        representation_key: self.representation_key,
                        route: self.route,
                        cached: self.cached_name,
                        project: self.project,
                        meta,
                        base,
                    },
                    FlightGuard {
                        key: self.key,
                        _guard: self.guard,
                    },
                    self.permit,
                    raw,
                    served,
                    pending,
                ),
                last_serial,
            )),
            JsonPreflight::Complete { raw, served, summary } => {
                let upstream = meta.source.clone();
                let record = build_record(raw, &base, meta);
                let expires_at =
                    record.fetched_at_unix + crate::cache::freshness_secs(self.state.ttl_secs, record.fresh_secs);
                persist_streamed(
                    &self.state,
                    &self.key,
                    &self.cached_name,
                    &self.project,
                    &record,
                    &summary,
                    upstream.as_deref(),
                )?;
                spawn_metadata_backfill(
                    &self.state,
                    self.cached_name.clone(),
                    self.route.clone(),
                    &summary.registrations,
                );
                let bytes = Bytes::from(served);
                self.state
                    .cache
                    .store_hot_versioned(self.representation_key, bytes.clone(), expires_at, last_serial);
                release_flight(&self.state, &self.key, self.guard);
                Ok(PageOutcome::Ready(bytes, last_serial))
            }
        }
    }
}

/// The provenance and revalidation metadata a cached page keeps from the response that produced it.
struct PageMeta {
    source: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
    last_serial: Option<u64>,
    fresh_secs: Option<i64>,
    fetched_at_unix: i64,
}

fn build_record(raw: Vec<u8>, base: &url::Url, meta: PageMeta) -> CachedIndex {
    CachedIndex {
        source: meta.source,
        etag: meta.etag,
        last_modified: meta.last_modified,
        last_serial: meta.last_serial,
        fetched_at_unix: meta.fetched_at_unix,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: meta.fresh_secs,
        body: canonical_json(&raw, base).unwrap_or(raw),
    }
}

/// Drain the rest of a page whose `files` preceded `project-status`, then transform it whole with the project
/// status seeded so a quarantined project withholds its files regardless of key order.
async fn buffer_whole_page(
    mut body: futures_util::stream::BoxStream<'static, Result<Bytes, peryx_upstream::UpstreamError>>,
    mut raw: Vec<u8>,
    context: crate::stream::PageContext,
) -> Result<(Vec<u8>, Vec<u8>, PageSummary), CacheError> {
    use futures_util::StreamExt as _;
    // Bound the buffer as the transformer does: a `files`-before-status page whose whole body would
    // pass `MAX_PAGE_BYTES` must fail here, before `raw` grows unbounded, rather than after the second
    // transformer pass allocates a matching output vector from the oversized input.
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        if raw.len().saturating_add(chunk.len()) > MAX_PAGE_BYTES {
            return Err(transform_error(TransformError::TooLarge));
        }
        raw.extend_from_slice(&chunk);
    }
    let mut transformer = PageTransformer::new(context);
    let parsed = crate::parse_detail(&raw).map_err(CacheError::Simple)?;
    transformer.seed_project_status(parsed.meta.project_status, parsed.meta.project_status_reason);
    let mut served = Vec::with_capacity(raw.len());
    transformer.push_into(&raw, &mut served).map_err(transform_error)?;
    let summary = transformer.finish().map_err(transform_error)?;
    Ok((raw, served, summary))
}

enum JsonPreflight {
    Streaming {
        body: futures_util::stream::BoxStream<'static, Result<Bytes, peryx_upstream::UpstreamError>>,
        transformer: Box<PageTransformer>,
        raw: Vec<u8>,
        served: Vec<u8>,
        pending: VecDeque<Bytes>,
    },
    Complete {
        raw: Vec<u8>,
        served: Vec<u8>,
        summary: PageSummary,
    },
}

/// An HTML-only upstream cannot stream through the JSON transformer: buffer it, canonicalize to
/// JSON once, and persist.
async fn preflight_json_stream(
    mut body: futures_util::stream::BoxStream<'static, Result<Bytes, peryx_upstream::UpstreamError>>,
    mut transformer: PageTransformer,
) -> Result<JsonPreflight, CacheError> {
    use futures_util::StreamExt as _;
    let mut raw = Vec::new();
    let mut pending = VecDeque::new();
    let mut outgoing = Vec::new();
    loop {
        let Some(chunk) = body.next().await else {
            let summary = transformer.finish().map_err(transform_error)?;
            return Ok(JsonPreflight::Complete {
                raw,
                served: outgoing,
                summary,
            });
        };
        let chunk = chunk?;
        for position in 0..chunk.len() {
            raw.push(chunk[position]);
            transformer
                .push_into(&chunk[position..=position], &mut outgoing)
                .map_err(transform_error)?;
            if transformer.header_preflight_done() || raw.len() >= JSON_META_PREFLIGHT_BYTES {
                pending.push_back(Bytes::copy_from_slice(&outgoing));
                body = prepend_chunk(body, chunk.slice(position + 1..));
                return Ok(JsonPreflight::Streaming {
                    body,
                    transformer: Box::new(transformer),
                    raw,
                    served: outgoing,
                    pending,
                });
            }
        }
    }
}

fn prepend_chunk(
    body: futures_util::stream::BoxStream<'static, Result<Bytes, peryx_upstream::UpstreamError>>,
    chunk: Bytes,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, peryx_upstream::UpstreamError>> {
    use futures_util::StreamExt as _;
    if chunk.is_empty() {
        body
    } else {
        futures_util::stream::once(async move { Ok(chunk) }).chain(body).boxed()
    }
}

/// Retains the single-flight hold until the live stream ends.
struct FlightGuard {
    key: String,
    _guard: peryx_index::serving::FlightGuard,
}

/// Everything a live streaming fetch carries between polls.
struct LiveStream {
    body: futures_util::stream::BoxStream<'static, Result<Bytes, peryx_upstream::UpstreamError>>,
    transformer: PageTransformer,
    representation_key: String,
    route: String,
    cached: String,
    project: String,
    meta: PageMeta,
    base: url::Url,
}

fn live_stream(
    state: Arc<ServingState>,
    live: LiveStream,
    flight: FlightGuard,
    permit: UpstreamPermit,
    raw: Vec<u8>,
    served: Vec<u8>,
    pending: VecDeque<Bytes>,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>> {
    use futures_util::StreamExt as _;
    let started = std::time::Instant::now();
    futures_util::stream::unfold(
        (state, Some(live), Some(flight), Some(permit), raw, served, pending),
        move |(state, live, flight, permit, mut raw, mut served, mut pending)| async move {
            let mut live = live?;
            let flight = flight?;
            let permit = permit?;
            if let Some(out) = pending.pop_front() {
                return Some((
                    Ok(out),
                    (state, Some(live), Some(flight), Some(permit), raw, served, pending),
                ));
            }
            match live.body.next().await {
                Some(Ok(chunk)) => {
                    raw.extend_from_slice(&chunk);
                    match live.transformer.push(&chunk) {
                        Ok(out) => {
                            served.extend_from_slice(&out);
                            Some((
                                Ok(Bytes::from(out)),
                                (state, Some(live), Some(flight), Some(permit), raw, served, pending),
                            ))
                        }
                        Err(err) => Some((
                            Err(std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string())),
                            (state, None, None, None, raw, served, pending),
                        )),
                    }
                }
                None => match complete_live(state.clone(), live, flight, permit, raw, served, started).await {
                    Ok(()) => None,
                    Err(error) => Some((Err(error), (state, None, None, None, Vec::new(), Vec::new(), pending))),
                },
                Some(Err(err)) => Some((
                    Err(std::io::Error::other(err.to_string())),
                    (state, None, None, None, raw, served, pending),
                )),
            }
        },
    )
    .boxed()
}

async fn complete_live(
    state: Arc<ServingState>,
    live: LiveStream,
    flight: FlightGuard,
    permit: UpstreamPermit,
    raw: Vec<u8>,
    mut served: Vec<u8>,
    started: std::time::Instant,
) -> Result<(), std::io::Error> {
    let LiveStream {
        body: _,
        transformer,
        representation_key,
        route,
        cached,
        project,
        meta,
        base,
    } = live;
    let summary = transformer
        .finish()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, error.to_string()))?;
    let last_serial = meta.last_serial;
    let upstream = meta.source.clone();
    let record = build_record(raw, &base, meta);
    let expires_at = record.fetched_at_unix + crate::cache::freshness_secs(state.ttl_secs, record.fresh_secs);
    let raw_len = record.body.len();
    let registrations = summary.registrations.clone();
    let persist_state = state.clone();
    let key = flight.key.clone();
    let persist_key = key.clone();
    let persist_cached = cached.clone();
    let persist_project = project.clone();
    let persist_upstream = upstream.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(err) = persist_streamed(
            &persist_state,
            &persist_key,
            &persist_cached,
            &persist_project,
            &record,
            &summary,
            persist_upstream.as_deref(),
        ) {
            tracing::error!(error = ?err, key = %persist_key, "page persist failed");
        }
    })
    .await
    .expect("page persist task never panics");
    spawn_metadata_backfill(&state, cached.clone(), route, &registrations);
    state.cache.store_hot_versioned(
        representation_key,
        Bytes::from(std::mem::take(&mut served)),
        expires_at,
        last_serial,
    );
    let elapsed_ms = started.elapsed().as_millis();
    let upstream = upstream.unwrap_or(cached);
    drop(permit);
    drop(flight);
    tracing::debug!(%key, upstream, bytes = raw_len, elapsed_ms, "page streamed from upstream");
    Ok(())
}
