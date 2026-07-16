//! The live upstream tee: forward a cold `PyPI` page to the client as it arrives while persisting
//! it, so a serial resolver waits on the network once, not twice.

use super::{
    Arc, Bytes, CacheError, CachedIndex, Event, JSON_META_PREFLIGHT_BYTES, PageOutcome, PageSummary, PageTransformer,
    ServingState, UpstreamPermit, VecDeque, mirror_route, persist_streamed, release_flight, spawn_metadata_backfill,
    transform_error,
};
use crate::cache::fetch::canonical_json;

pub(super) struct FreshJsonStream {
    pub(super) state: Arc<ServingState>,
    pub(super) key: String,
    pub(super) hot_key: String,
    pub(super) route: String,
    pub(super) cached_name: String,
    pub(super) project: String,
    pub(super) now: i64,
    pub(super) context: crate::stream::PageContext,
    pub(super) cached_present: bool,
    pub(super) guard: tokio::sync::OwnedMutexGuard<()>,
    pub(super) head: crate::simple_client::SimpleHead,
    pub(super) permit: UpstreamPermit,
}

impl FreshJsonStream {
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the flight guard deliberately lives until it moves into the stream or is released"
    )]
    pub(super) async fn stream(self) -> Result<PageOutcome, CacheError> {
        use futures_util::StreamExt as _;
        if self.cached_present {
            tracing::info!(key = %self.key, "upstream page changed");
            self.state.metrics.record(Event::Refresh {
                route: mirror_route(&self.state, &self.cached_name),
                project: self.project.clone(),
                changed: true,
            });
        }

        let etag = self.head.etag.clone();
        let last_serial = self.head.last_serial;
        let max_age = self.head.max_age;
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
        // `files` streamed ahead of `meta`, so the project status is not yet known: buffer the rest
        // of the page and transform it whole with the status seeded, then finish it on the buffered
        // path. Otherwise a quarantined project's files would stream out before `meta` was read.
        let preflight = match preflight {
            JsonPreflight::Streaming {
                body, transformer, raw, ..
            } if transformer.files_precede_meta() => match buffer_whole_page(body, raw, buffered_context).await {
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
                        flight: FlightGuard {
                            state: self.state,
                            key: self.key,
                            guard: Some(self.guard),
                        },
                        hot_key: self.hot_key,
                        route: self.route,
                        cached: self.cached_name,
                        project: self.project,
                        etag,
                        last_serial,
                        fetched_at: self.now,
                        fresh_secs: max_age,
                        base,
                        _permit: self.permit,
                    },
                    raw,
                    served,
                    pending,
                ),
                last_serial,
            )),
            JsonPreflight::Complete { raw, served, summary } => {
                let record = build_record(raw, &base, etag, last_serial, max_age, self.now);
                let expires_at =
                    record.fetched_at_unix + crate::cache::freshness_secs(self.state.ttl_secs, record.fresh_secs);
                #[rustfmt::skip]
                persist_streamed(&self.state, &self.key, &self.cached_name, &self.project, &record, &summary)?;
                spawn_metadata_backfill(self.state.clone(), self.route.clone(), &summary.registrations);
                let bytes = Bytes::from(served);
                self.state
                    .cache
                    .store_hot_versioned(self.hot_key, bytes.clone(), expires_at, last_serial);
                release_flight(&self.state, &self.key, self.guard);
                Ok(PageOutcome::Ready(bytes, last_serial))
            }
        }
    }
}

fn build_record(
    raw: Vec<u8>,
    base: &url::Url,
    etag: Option<String>,
    last_serial: Option<u64>,
    fresh_secs: Option<i64>,
    now: i64,
) -> CachedIndex {
    CachedIndex {
        etag,
        last_serial,
        fetched_at_unix: now,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs,
        body: canonical_json(&raw, base).unwrap_or(raw),
    }
}

/// Drain the rest of a page whose `files` preceded `meta`, then transform it whole with the project
/// status seeded so a quarantined project withholds its files regardless of key order.
async fn buffer_whole_page(
    mut body: futures_util::stream::BoxStream<'static, Result<Bytes, peryx_upstream::UpstreamError>>,
    mut raw: Vec<u8>,
    context: crate::stream::PageContext,
) -> Result<(Vec<u8>, Vec<u8>, PageSummary), CacheError> {
    use futures_util::StreamExt as _;
    while let Some(chunk) = body.next().await {
        raw.extend_from_slice(&chunk?);
    }
    let status = crate::parse_detail(&raw)
        .map_err(CacheError::Simple)?
        .meta
        .project_status;
    let mut transformer = PageTransformer::new(context);
    transformer.seed_project_status(status);
    let served = transformer.push(&raw).map_err(transform_error)?;
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
    let mut served = Vec::new();
    let mut pending = VecDeque::new();
    loop {
        let Some(chunk) = body.next().await else {
            let summary = transformer.finish().map_err(transform_error)?;
            return Ok(JsonPreflight::Complete { raw, served, summary });
        };
        let chunk = chunk?;
        for position in 0..chunk.len() {
            raw.push(chunk[position]);
            let out = transformer.push(&chunk[position..=position]).map_err(transform_error)?;
            if !out.is_empty() {
                served.extend_from_slice(&out);
                pending.push_back(Bytes::from(out));
            }
            if transformer.meta_preflight_done() || raw.len() >= JSON_META_PREFLIGHT_BYTES {
                body = prepend_chunk(body, chunk.slice(position + 1..));
                return Ok(JsonPreflight::Streaming {
                    body,
                    transformer: Box::new(transformer),
                    raw,
                    served,
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

/// Releases the single-flight hold however the live stream ends. Completion, a transform or upstream
/// error, and a mid-page client disconnect all drop the owning `LiveStream`, so `Drop` is the one
/// place that covers every terminal path; without it the error and disconnect paths would leak one
/// map entry per distinct failing project. Unlock before forgetting, matching `release_flight`.
struct FlightGuard {
    state: Arc<ServingState>,
    key: String,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        drop(self.guard.take());
        self.state.cache.forget_flight(&self.key);
    }
}

/// Everything a live streaming fetch carries between polls.
struct LiveStream {
    body: futures_util::stream::BoxStream<'static, Result<Bytes, peryx_upstream::UpstreamError>>,
    transformer: PageTransformer,
    flight: FlightGuard,
    hot_key: String,
    route: String,
    cached: String,
    project: String,
    etag: Option<String>,
    last_serial: Option<u64>,
    fetched_at: i64,
    fresh_secs: Option<i64>,
    base: url::Url,
    _permit: UpstreamPermit,
}

/// Stream the upstream body through the transformer to the client, teeing the raw bytes for the
/// page cache and the transformed bytes for the hot cache; both persist when the stream completes.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the flight guard inside LiveStream deliberately lives until the stream ends"
)]
fn live_stream(
    state: Arc<ServingState>,
    live: LiveStream,
    raw: Vec<u8>,
    served: Vec<u8>,
    pending: VecDeque<Bytes>,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>> {
    use futures_util::StreamExt as _;
    let started = std::time::Instant::now();
    futures_util::stream::unfold(
        (state, Some(live), raw, served, pending),
        move |(state, live, mut raw, mut served, mut pending)| async move {
            let mut live = live?;
            if let Some(out) = pending.pop_front() {
                return Some((Ok(out), (state, Some(live), raw, served, pending)));
            }
            match live.body.next().await {
                Some(Ok(chunk)) => {
                    raw.extend_from_slice(&chunk);
                    match live.transformer.push(&chunk) {
                        Ok(out) => {
                            served.extend_from_slice(&out);
                            Some((Ok(Bytes::from(out)), (state, Some(live), raw, served, pending)))
                        }
                        Err(err) => Some((
                            Err(std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string())),
                            (state, None, raw, served, pending),
                        )),
                    }
                }
                None => {
                    let summary = match live.transformer.finish() {
                        Ok(summary) => summary,
                        Err(err) => {
                            return Some((
                                Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, err.to_string())),
                                (state, None, raw, served, pending),
                            ));
                        }
                    };
                    let raw = std::mem::take(&mut raw);
                    let body = canonical_json(&raw, &live.base).unwrap_or(raw);
                    let record = CachedIndex {
                        etag: live.etag.clone(),
                        last_serial: live.last_serial,
                        fetched_at_unix: live.fetched_at,
                        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                        fresh_secs: live.fresh_secs,
                        body,
                    };
                    let expires_at = record.fetched_at_unix + crate::cache::freshness_secs(state.ttl_secs, record.fresh_secs);
                    // One batched transaction, awaited before the body closes: every byte has been
                    // sent already, and a client cannot act on the page before EOF, so downloads
                    // always find their file registrations.
                    let raw_len = record.body.len();
                    let registrations = summary.registrations.clone();
                    let persist_state = state.clone();
                    let (key, cached, project) = (live.flight.key.clone(), live.cached.clone(), live.project.clone());
                    #[rustfmt::skip]
                    tokio::task::spawn_blocking(move || {
                        if let Err(err) = persist_streamed(&persist_state, &key, &cached, &project, &record, &summary) { tracing::error!(error = ?err, %key, "page persist failed"); }
                    })
                    .await
                    .expect("page persist task never panics");
                    spawn_metadata_backfill(state.clone(), live.route.clone(), &registrations);
                    // The hot page goes live only after the persist lands: a concurrent client that
                    // serves this page from the hot cache and immediately requests a file must find
                    // that file's registration.
                    state.cache.store_hot_versioned(
                        live.hot_key.clone(),
                        Bytes::from(std::mem::take(&mut served)),
                        expires_at,
                        live.last_serial,
                    );
                    let elapsed_ms = started.elapsed().as_millis();
                    tracing::debug!(key = %live.flight.key, bytes = raw_len, elapsed_ms, "page streamed from upstream");
                    None
                }
                Some(Err(err)) => Some((
                    Err(std::io::Error::other(err.to_string())),
                    (state, None, raw, served, pending),
                )),
            }
        },
    )
    .boxed()
}
