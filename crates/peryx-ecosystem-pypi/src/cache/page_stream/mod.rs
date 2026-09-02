use std::collections::VecDeque;
use std::sync::Arc;

use crate::store::CachedIndex;
use crate::store::PypiStore as _;
use crate::stream::{PageSummary, PageTransformer};
use crate::{ProjectDetail, ProjectStatus, parse_detail};
use bytes::Bytes;
use peryx_driver::rate_limit::UpstreamPermit;
use peryx_driver::state::ServingState;
use peryx_events::metrics::Observation;
use peryx_index::{Index, IndexKind};
use peryx_policy::PolicyAction;
use peryx_upstream::UpstreamClient;

use crate::simple_client::{SimpleClientExt as _, SimpleHead, SimpleResponse};

use super::fetch::{canonical_raw, persist_page_from};
use super::metadata::spawn_metadata_backfill;
use super::resolve::{known_metadata, local_detail, resolve_detail, rewrite_urls};
mod live;
use live::FreshJsonStream;

use super::{
    CacheError, NEGATIVE_TTL_SECS, cached_record, flight_gate, fresh_cached, freshness, is_json, mirror_route,
    project_negative_key, release_flight, release_then, upstream_permit,
};

fn persist_streamed(
    state: &ServingState,
    key: &str,
    name: &str,
    project: &str,
    record: &CachedIndex,
    summary: &PageSummary,
    upstream: Option<&str>,
) -> Result<(), CacheError> {
    let registrations = if summary
        .project_status
        .as_deref()
        .and_then(ProjectStatus::from_marker)
        .is_some_and(|status| !status.offers_downloads())
    {
        &[]
    } else {
        summary.registrations.as_slice()
    };
    let files: Vec<crate::store::PublishedFileWrite> = registrations
        .iter()
        .map(|registration| crate::store::PublishedFileWrite {
            sha256: registration.sha256.clone(),
            filename: registration.filename.clone(),
            url: registration.url.clone(),
            size: registration.size,
            metadata: registration.metadata.clone(),
        })
        .collect();
    let attestations: Vec<(String, String, String)> = registrations
        .iter()
        .filter_map(|registration| {
            let url = registration.provenance.clone()?;
            Some((registration.sha256.clone(), registration.filename.clone(), url))
        })
        .collect();
    let display = summary.name.as_deref().unwrap_or(project);
    state
        .meta
        .put_cached_page(crate::store::CachedPageWrite {
            key,
            record,
            index: name,
            normalized: project,
            display,
            source: name,
            upstream,
            project_status: summary.project_status.as_deref(),
            project_status_reason: summary.project_status_reason.as_deref(),
            files: &files,
            attestations: &attestations,
        })
        .map_err(CacheError::from)?;
    super::invalidate_project(state, name, project);
    Ok(())
}

async fn fetch_project_head(
    state: &ServingState,
    name: &str,
    client: &UpstreamClient,
    project: &str,
    etag: Option<&str>,
) -> Result<SimpleHead, peryx_upstream::UpstreamError> {
    match state.upstream_routes.get(name) {
        Some(router) => router.head_project(project, etag).await,
        None => client.head_project(project, etag).await,
    }
}

pub enum PageOutcome {
    /// The full transformed document, from the hot cache or a warm raw page.
    Ready(Bytes, Option<u64>),
    /// A live upstream fetch, transformed chunk by chunk as it arrives. The raw body tees into the
    /// page cache and the transformed body into the hot cache when the stream completes.
    Streaming(
        futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>>,
        Option<u64>,
    ),
    /// The project does not exist upstream.
    NotFound,
    /// Not streamable here (several cached layers, or no cached); the buffered path serves it.
    Fallback,
}

const JSON_META_PREFLIGHT_BYTES: usize = 64 * 1024;

/// # Errors
/// Returns [`CacheError`] on a store failure; upstream failures degrade to [`PageOutcome::Fallback`]
/// so the buffered path can serve stale data.
pub async fn stream_detail(
    state: Arc<ServingState>,
    position: usize,
    project: String,
) -> Result<PageOutcome, CacheError> {
    let index = state.index_at(position);
    index.policy.check_resource(PolicyAction::Serve, &project)?;
    if index.policy.active() || super::has_active_revocations(&state)? {
        return Ok(PageOutcome::Fallback);
    }
    let route = index.route.clone();
    let representation_key = state.representation_key(&route, &project, super::SIMPLE_JSON);
    // A hot hit is a lookup and a memcpy; take it before the per-request work in `streaming_parts`
    // (upstream client build, upload/override scans, page context). Only a page that already streamed
    // through the transform path can be hot, so this never shadows a Fallback the miss path would pick.
    if let Some((bytes, last_serial)) = state.hot_fresh_versioned(&representation_key) {
        return Ok(PageOutcome::Ready(bytes, last_serial));
    }

    let Some((cached_name, client, offline, context)) = streaming_parts(&state, index, &project)? else {
        return Ok(PageOutcome::Fallback);
    };
    let key = format!("{cached_name}/{project}");
    if offline {
        return offline_page(&state, &key, &representation_key, context);
    }
    if let Some(record) = fresh_cached(&state, &key)? {
        return transform_whole(&state, &representation_key, &record, context);
    }
    if state.negative_fresh(&project_negative_key(&key)) {
        return Ok(missing_upstream_outcome(&context));
    }
    // Serve stale before taking the flight gate so concurrent hits do not queue; the spawned refresh
    // coalesces itself.
    if let Some(record) = super::stale_servable(&state, &key)? {
        drop(spawn_revalidation(state.clone(), key, cached_name, project, client));
        return transform_whole(&state, &representation_key, &record, context);
    }

    let gate = flight_gate(&state, &key);
    let guard = gate.lock_owned().await;
    if let Some((bytes, last_serial)) =
        state.hot_fresh_versioned(&state.representation_key(&route, &project, super::SIMPLE_JSON))
    {
        return Ok(PageOutcome::Ready(bytes, last_serial));
    }
    if let Some(record) = fresh_cached(&state, &key)? {
        return transform_whole(&state, &representation_key, &record, context);
    }
    if state.negative_fresh(&project_negative_key(&key)) {
        return release_then(&state, &key, guard, || Ok(missing_upstream_outcome(&context)));
    }

    let now = (state.clock)();
    let cached = cached_record(&state, &key)?;
    let etag = cached.as_ref().and_then(|record| record.etag.clone());
    let permit = upstream_permit(&state, &cached_name).await?;
    let Ok(head) = fetch_project_head(&state, &cached_name, &client, &project, etag.as_deref()).await else {
        return release_then(&state, &key, guard, || Ok(PageOutcome::Fallback));
    };
    match head.status {
        200 if is_json(head.content_type.as_deref()) => {
            FreshJsonStream {
                state,
                key,
                representation_key,
                route,
                cached_name,
                project,
                now,
                context,
                cached_present: cached.is_some(),
                guard,
                head,
                permit,
            }
            .stream()
            .await
        }
        304 => {
            let mut record = cached.ok_or(CacheError::Unavailable)?;
            record.fetched_at_unix = now;
            record.fresh_secs = head.max_age.or(record.fresh_secs);
            state
                .meta
                .touch_index_freshness(&key, record.fetched_at_unix, record.fresh_secs)?;
            state.metrics.record(Observation::Refresh {
                repository: mirror_route(&state, &cached_name),
                resource: project.clone(),
                changed: false,
            });
            release_then(&state, &key, guard, || {
                transform_whole(&state, &representation_key, &record, context)
            })
        }
        404 => retire_missing_project(&state, &key, &cached_name, &project, guard, &context),
        200 => {
            let record = buffer_html_page(&state, &key, &cached_name, &project, now, head).await?;
            release_then(&state, &key, guard, || {
                transform_whole(&state, &representation_key, &record, context)
            })
        }
        _ => release_then(&state, &key, guard, || Ok(PageOutcome::Fallback)),
    }
}

fn retire_missing_project(
    state: &ServingState,
    key: &str,
    index: &str,
    project: &str,
    guard: peryx_index::serving::FlightGuard,
    context: &crate::stream::PageContext,
) -> Result<PageOutcome, CacheError> {
    let retired = state.meta.retire_cached_project(key, index, project);
    release_flight(state, key, guard);
    retired.map_err(CacheError::from).map(|()| {
        super::invalidate_project(state, index, project);
        state.remember_negative(project_negative_key(key), NEGATIVE_TTL_SECS);
        missing_upstream_outcome(context)
    })
}

fn offline_page(
    state: &ServingState,
    key: &str,
    representation_key: &str,
    context: crate::stream::PageContext,
) -> Result<PageOutcome, CacheError> {
    state.meta.get_index(key)?.map_or_else(
        || Ok(PageOutcome::Fallback),
        |record| transform_whole(state, representation_key, &record, context),
    )
}

/// Fetch and persist the project page for `position`, then return the served detail model.
///
/// `peryx prefetch sync` uses this instead of a separate downloader so CLI prefetching and HTTP
/// requests share cache registration, single-flight, and streaming behavior.
///
/// # Errors
/// Returns [`CacheError`] on store, parse, upstream, or stream failures.
pub async fn materialize_detail(
    state: Arc<ServingState>,
    position: usize,
    project: String,
) -> Result<Option<ProjectDetail>, CacheError> {
    match stream_detail(state.clone(), position, project.clone()).await? {
        PageOutcome::Ready(_, _) | PageOutcome::Fallback => {}
        PageOutcome::NotFound => return Ok(None),
        PageOutcome::Streaming(mut stream, _) => {
            use futures_util::StreamExt as _;
            while let Some(chunk) = stream.next().await {
                chunk.map_err(|err| CacheError::Stream(err.to_string()))?;
            }
        }
    }
    let index = state.index_at(position);
    resolve_detail(&state, index, &project, &index.route).await
}

const fn missing_upstream_outcome(context: &crate::stream::PageContext) -> PageOutcome {
    if context.local_files.is_empty() && context.local_versions.is_empty() {
        PageOutcome::NotFound
    } else {
        PageOutcome::Fallback
    }
}

async fn buffer_html_page(
    state: &ServingState,
    key: &str,
    cached_name: &str,
    project: &str,
    now: i64,
    head: SimpleHead,
) -> Result<CachedIndex, CacheError> {
    let url = head.url.clone();
    let content_type = head.content_type.clone();
    let (etag, last_serial) = (head.etag.clone(), head.last_serial);
    let last_modified = head.last_modified.clone();
    let max_age = head.max_age;
    let source = head.source.clone();
    let body = head.bytes().await?;
    let response = SimpleResponse {
        status: 200,
        source,
        url,
        content_type,
        etag,
        last_modified,
        retry_after: None,
        last_serial,
        max_age,
        body,
    };
    let record = CachedIndex {
        etag: response.etag.clone(),
        last_serial: response.last_serial,
        fetched_at_unix: now,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: response.max_age,
        body: canonical_raw(project, &response)?,
    };
    persist_page_from(state, key, cached_name, project, &record, response.source.as_deref())?;
    Ok(record)
}

fn streaming_parts(
    state: &ServingState,
    index: &Index,
    project: &str,
) -> Result<Option<(String, UpstreamClient, bool, crate::stream::PageContext)>, CacheError> {
    match &index.kind {
        _ if index.policy.has_resource_size_limit() => Ok(None),
        IndexKind::Cached { client, offline } => Ok(Some((
            index.name.clone(),
            client.clone(),
            *offline,
            crate::stream::page_context(
                &index.route,
                project,
                index.policy.clone(),
                Vec::new(),
                Vec::new(),
                &std::collections::BTreeMap::new(),
            ),
        ))),
        IndexKind::Hosted { .. } => Ok(None),
        IndexKind::Virtual { layers, write_target } => {
            let mut cached = None;
            let mut local_files = Vec::new();
            let mut local_versions = Vec::new();
            for &pos in layers {
                let layer = state.index_at(pos);
                match &layer.kind {
                    IndexKind::Cached { client, offline } => {
                        if layer.policy.active() {
                            return Ok(None);
                        }
                        if cached.replace((layer.name.clone(), client.clone(), *offline)).is_some() {
                            return Ok(None);
                        }
                    }
                    IndexKind::Hosted { .. } => {
                        if layer.policy.active() {
                            return Ok(None);
                        }
                        if let Some(mut detail) = local_detail(state, &layer.name, project)? {
                            rewrite_urls(&mut detail, &index.route);
                            local_versions.extend(detail.versions);
                            local_files.extend(detail.files);
                        }
                    }
                    IndexKind::Virtual { .. } => return Ok(None),
                }
            }
            let Some((cached, client, offline)) = cached else {
                return Ok(None);
            };
            let overrides = match write_target {
                Some(pos) => state.meta.list_overrides(&state.index_at(*pos).name, project)?,
                None => std::collections::BTreeMap::new(),
            };
            Ok(Some((
                cached,
                client,
                offline,
                crate::stream::page_context(
                    &index.route,
                    project,
                    index.policy.clone(),
                    local_files,
                    local_versions,
                    &overrides,
                ),
            )))
        }
    }
}

fn transform_whole(
    state: &ServingState,
    representation_key: &str,
    record: &CachedIndex,
    mut context: crate::stream::PageContext,
) -> Result<PageOutcome, CacheError> {
    let detail = parse_detail(&record.body)?;
    context.known_metadata = known_metadata(state, &detail.files)?;
    let mut transformer = PageTransformer::new(context);
    // Seed the status so a quarantined page withholds its files whether `meta` precedes or follows
    // `files`; the whole-page pass otherwise learns the status only once it reaches `meta`.
    transformer.seed_project_status(detail.meta.project_status, detail.meta.project_status_reason);
    let mut out = Vec::with_capacity(record.body.len());
    transformer.push_into(&record.body, &mut out).map_err(transform_error)?;
    transformer.finish().map_err(transform_error)?;
    out.shrink_to_fit();
    let bytes = Bytes::from(out);
    let expires_at = record.fetched_at_unix + freshness(state, record);
    state.cache.store_hot_versioned(
        representation_key.to_owned(),
        bytes.clone(),
        expires_at,
        record.last_serial,
    );
    Ok(PageOutcome::Ready(bytes, record.last_serial))
}

/// Refresh a stale-but-served page against upstream in the background, coalesced by the same
/// single-flight gate the on-demand fetch uses.
///
/// The first hit to find a page stale takes the gate and revalidates it; concurrent hits that also
/// served it stale find the gate held, so a burst of requests triggers one upstream check, not a
/// herd. The serving path drops the handle because it already answered from the stale bytes.
fn spawn_revalidation(
    state: Arc<ServingState>,
    key: String,
    name: String,
    project: String,
    client: UpstreamClient,
) -> Option<tokio::task::JoinHandle<()>> {
    let guard = flight_gate(&state, &key).try_lock_owned().ok()?;
    Some(tokio::spawn(revalidate(state, key, name, project, client, guard)))
}

/// Revalidate one page and release the single-flight hold however it ends. The request that spawned
/// this already holds the stale bytes, so a failed refresh only logs and leaves the stale page in
/// place for the next request to retry.
async fn revalidate(
    state: Arc<ServingState>,
    key: String,
    name: String,
    project: String,
    client: UpstreamClient,
    guard: peryx_index::serving::FlightGuard,
) {
    if let Err(err) = super::fetch::fetch_and_store(&state, &key, &name, &project, &client).await {
        tracing::debug!(?err, %key, "background revalidation failed");
    }
    release_flight(&state, &key, guard);
}

fn transform_error(err: crate::stream::TransformError) -> CacheError {
    match err {
        crate::stream::TransformError::Parse(err) => CacheError::Parse(err),
        crate::stream::TransformError::Simple(err) => CacheError::Simple(err),
        crate::stream::TransformError::Truncated
        | crate::stream::TransformError::Trailing
        | crate::stream::TransformError::Malformed
        | crate::stream::TransformError::TooLarge => CacheError::Unavailable,
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/cache/page_stream/tests.rs"]
mod tests;
