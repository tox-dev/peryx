//! Streaming simple-page serving: hot cache, warm transform, and live upstream tee.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::store::CachedIndex;
use crate::store::PypiStore as _;
use crate::stream::{PageSummary, PageTransformer};
use crate::{ProjectDetail, ProjectStatus, parse_detail};
use bytes::Bytes;
use peryx_driver::rate_limit::UpstreamPermit;
use peryx_driver::state::ServingState;
use peryx_events::metrics::Event;
use peryx_index::{Index, IndexKind};
use peryx_policy::PolicyAction;
use peryx_upstream::UpstreamClient;

use crate::simple_client::{SimpleClientExt as _, SimpleHead, SimpleResponse};

use super::fetch::canonical_raw;
use super::metadata::spawn_metadata_backfill;
use super::resolve::{known_metadata, local_detail, resolve_detail, rewrite_urls};
mod live;
use live::FreshJsonStream;

use super::{
    CacheError, NEGATIVE_TTL_SECS, cached_record, flight_gate, fresh_cached, freshness, is_json, mirror_route,
    persist_page, project_negative_key, release_flight, upstream_permit,
};

/// Persist a streamed page from what the transformer already extracted: no re-parse of the raw
/// body sits on the serving path, which a serial client feels on every large cold page.
fn persist_streamed(
    state: &ServingState,
    key: &str,
    name: &str,
    project: &str,
    record: &CachedIndex,
    summary: &PageSummary,
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
    let files: Vec<(String, String, Option<u64>)> = registrations
        .iter()
        .map(|registration| (registration.sha256.clone(), registration.url.clone(), registration.size))
        .collect();
    let metadata: Vec<(String, String, String)> = registrations
        .iter()
        .filter_map(|registration| {
            let (url, digest) = registration.metadata.as_ref()?;
            Some((registration.sha256.clone(), url.clone(), digest.clone()))
        })
        .collect();
    let display = summary.name.as_deref().unwrap_or(project);
    state
        .meta
        .put_cached_page(
            key,
            record,
            name,
            project,
            display,
            name,
            summary.project_status.as_deref(),
            summary.project_status_reason.as_deref(),
            &files,
            &metadata,
        )
        .map_err(CacheError::from)?;
    state.invalidate_project(project);
    Ok(())
}

/// How a simple-page request gets its bytes.
pub enum PageOutcome {
    /// The full transformed document, from the hot cache or a warm raw page.
    Ready(Bytes),
    /// A live upstream fetch, transformed chunk by chunk as it arrives. The raw body tees into the
    /// page cache and the transformed body into the hot cache when the stream completes.
    Streaming(futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>>),
    /// The project does not exist upstream.
    NotFound,
    /// Not streamable here (several cached layers, or no cached); the buffered path serves it.
    Fallback,
}

const JSON_META_PREFLIGHT_BYTES: usize = 64 * 1024;

/// Serve a simple page with maximum overlap: hot cache first, then a warm raw page transformed in
/// memory, then a streaming upstream fetch.
///
/// # Errors
/// Returns [`CacheError`] on a store failure; upstream failures degrade to [`PageOutcome::Fallback`]
/// so the buffered path can serve stale data.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the flight guard deliberately lives until it moves into the stream or is released"
)]
pub async fn stream_detail(
    state: Arc<ServingState>,
    position: usize,
    project: String,
) -> Result<PageOutcome, CacheError> {
    let index = state.index_at(position);
    index.policy.check_project(PolicyAction::Serve, &project)?;
    if index.policy.active() {
        return Ok(PageOutcome::Fallback);
    }
    let route = index.route.clone();
    let hot_key = state.hot_key(&route, &project, super::SIMPLE_JSON);
    // A hot hit is a lookup and a memcpy; take it before the per-request work in `streaming_parts`
    // (upstream client build, upload/override scans, page context). Only a page that already streamed
    // through the transform path can be hot, so this never shadows a Fallback the miss path would pick.
    if let Some(bytes) = state.hot_fresh(&hot_key) {
        return Ok(PageOutcome::Ready(bytes));
    }

    let Some((cached_name, client, offline, context)) = streaming_parts(&state, index, &project)? else {
        return Ok(PageOutcome::Fallback);
    };

    let key = format!("{cached_name}/{project}");
    if offline {
        return offline_page(&state, &key, &hot_key, context);
    }
    if let Some(record) = fresh_cached(&state, &key)? {
        return Ok(PageOutcome::Ready(transform_whole(&state, &hot_key, &record, context)?));
    }
    if state.negative_fresh(&project_negative_key(&key)) {
        return Ok(missing_upstream_outcome(&context));
    }
    // Serve stale before taking the flight gate so concurrent hits do not queue; the spawned refresh
    // coalesces itself.
    if let Some(record) = super::stale_servable(&state, &key)? {
        let bytes = transform_whole(&state, &hot_key, &record, context)?;
        let _ = spawn_revalidation(state.clone(), key, cached_name, project, client);
        return Ok(PageOutcome::Ready(bytes));
    }

    let gate = flight_gate(&state, &key);
    let guard = gate.lock_owned().await;
    if let Some(bytes) = state.hot_fresh(&state.hot_key(&route, &project, super::SIMPLE_JSON)) {
        return Ok(PageOutcome::Ready(bytes));
    }
    if let Some(record) = fresh_cached(&state, &key)? {
        return Ok(PageOutcome::Ready(transform_whole(&state, &hot_key, &record, context)?));
    }
    if state.negative_fresh(&project_negative_key(&key)) {
        release_flight(&state, &key, guard);
        return Ok(missing_upstream_outcome(&context));
    }

    let now = (state.clock)();
    let cached = cached_record(&state, &key)?;
    let etag = cached.as_ref().and_then(|record| record.etag.clone());
    let permit = upstream_permit(&state, &cached_name).await?;
    let Ok(head) = client.head_project(&project, etag.as_deref()).await else {
        release_flight(&state, &key, guard);
        return Ok(PageOutcome::Fallback);
    };
    match head.status {
        200 if is_json(head.content_type.as_deref()) => {
            FreshJsonStream {
                state,
                key,
                hot_key,
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
            state.metrics.record(Event::Refresh {
                route: mirror_route(&state, &cached_name),
                project: project.clone(),
                changed: false,
            });
            release_flight(&state, &key, guard);
            Ok(PageOutcome::Ready(transform_whole(&state, &hot_key, &record, context)?))
        }
        404 => {
            state.remember_negative(project_negative_key(&key), NEGATIVE_TTL_SECS);
            release_flight(&state, &key, guard);
            Ok(missing_upstream_outcome(&context))
        }
        200 => {
            let record = buffer_html_page(&state, &key, &cached_name, &project, now, head).await?;
            release_flight(&state, &key, guard);
            Ok(PageOutcome::Ready(transform_whole(&state, &hot_key, &record, context)?))
        }
        _ => {
            release_flight(&state, &key, guard);
            Ok(PageOutcome::Fallback)
        }
    }
}

fn offline_page(
    state: &ServingState,
    key: &str,
    hot_key: &str,
    context: crate::stream::PageContext,
) -> Result<PageOutcome, CacheError> {
    match state.meta.get_index(key)? {
        Some(record) => Ok(PageOutcome::Ready(transform_whole(state, hot_key, &record, context)?)),
        None => Ok(PageOutcome::Fallback),
    }
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
        PageOutcome::Ready(_) | PageOutcome::Fallback => {}
        PageOutcome::NotFound => return Ok(None),
        PageOutcome::Streaming(mut stream) => {
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
    let max_age = head.max_age;
    let body = head.bytes().await?;
    let response = SimpleResponse {
        status: 200,
        url,
        content_type,
        etag,
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
    persist_page(state, key, cached_name, project, &record)?;
    Ok(record)
}

/// The streaming ingredients for an index: its single cached layer with its client, plus the hosted
/// virtual-index context. `None` when the index has no cached or more than one (the buffered path
/// handles those).
fn streaming_parts(
    state: &ServingState,
    index: &Index,
    project: &str,
) -> Result<Option<(String, UpstreamClient, bool, crate::stream::PageContext)>, CacheError> {
    match &index.kind {
        _ if index.policy.has_project_size_limit() => Ok(None),
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
        IndexKind::Virtual { layers, upload } => {
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
            let overrides: std::collections::BTreeMap<String, String> = match upload {
                Some(pos) => state
                    .meta
                    .list_overrides(&state.index_at(*pos).name, project)?
                    .into_iter()
                    .collect(),
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

/// Transform a warm raw page in one pass and remember the result in the hot cache.
fn transform_whole(
    state: &ServingState,
    hot_key: &str,
    record: &CachedIndex,
    mut context: crate::stream::PageContext,
) -> Result<Bytes, CacheError> {
    let detail = parse_detail(&record.body)?;
    context.known_metadata = known_metadata(state, &detail.files)?;
    let mut transformer = PageTransformer::new(context);
    // Seed the status so a quarantined page withholds its files whether `meta` precedes or follows
    // `files`; the whole-page pass otherwise learns the status only once it reaches `meta`.
    transformer.seed_project_status(detail.meta.project_status);
    let mut out = transformer.push(&record.body).map_err(transform_error)?;
    transformer.finish().map_err(transform_error)?;
    out.shrink_to_fit();
    let bytes = Bytes::from(out);
    let expires_at = record.fetched_at_unix + freshness(state, record);
    state.cache.store_hot(hot_key.to_owned(), bytes.clone(), expires_at);
    Ok(bytes)
}

/// Refresh a stale-but-served page against upstream in the background, coalesced by the same
/// single-flight gate the on-demand fetch uses.
///
/// The first hit to find a page stale takes the gate and revalidates it; concurrent hits that also
/// served it stale find the gate held, so a burst of requests triggers one upstream check, not a
/// herd. The returned handle lets a test await the refresh; the serving path drops it, having already
/// answered from the stale bytes.
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
    guard: tokio::sync::OwnedMutexGuard<()>,
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
        | crate::stream::TransformError::TooLarge => CacheError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use peryx_storage::blob::BlobStore;
    use peryx_storage::meta::MetaStore;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn test_transform_error_maps_parse_and_truncated_errors() {
        let err = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert!(matches!(transform_error(err.into()), CacheError::Parse(_)));
        assert!(matches!(
            transform_error(crate::stream::TransformError::Truncated),
            CacheError::Unavailable
        ));
        assert!(matches!(
            transform_error(crate::stream::TransformError::TooLarge),
            CacheError::Unavailable
        ));
    }

    fn flask_body(versions: &[&str]) -> Vec<u8> {
        crate::to_json(&crate::ProjectDetail {
            meta: crate::Meta::default(),
            name: "flask".to_owned(),
            versions: versions.iter().map(|version| (*version).to_owned()).collect(),
            files: vec![],
        })
        .into_bytes()
    }

    /// A wired state whose `pypi` mirror holds a `flask` page stale since `fetched_at`, over a mock
    /// upstream at `upstream`.
    fn stale_flask_state(
        dir: &tempfile::TempDir,
        upstream: &str,
        fetched_at: i64,
    ) -> (Arc<ServingState>, UpstreamClient) {
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        let client = UpstreamClient::new(upstream).unwrap();
        let indexes = vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: client.clone(),
                offline: false,
            },
            policy: peryx_policy::Policy::default(),
            acl: peryx_identity::IndexAcl::default(),
        }];
        let mut app = peryx_driver::state::AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 2000));
        crate::install(&mut app);
        let state = app.serving.clone();
        state
            .meta
            .put_index(
                "pypi/flask",
                &CachedIndex {
                    etag: None,
                    last_serial: None,
                    fetched_at_unix: fetched_at,
                    content_type: None,
                    fresh_secs: None,
                    body: flask_body(&["1.0"]),
                },
            )
            .unwrap();
        (state, client)
    }

    #[tokio::test]
    async fn test_spawn_revalidation_refreshes_the_cached_page() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let (state, client) = stale_flask_state(&dir, &format!("{}/simple/", server.uri()), 1000);
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(flask_body(&["1.0", "2.0"]), "application/vnd.pypi.simple.v1+json"),
            )
            .mount(&server)
            .await;

        spawn_revalidation(
            state.clone(),
            "pypi/flask".to_owned(),
            "pypi".to_owned(),
            "flask".to_owned(),
            client,
        )
        .expect("the free gate lets the refresh run")
        .await
        .unwrap();

        let body = state.meta.get_index("pypi/flask").unwrap().unwrap().body;
        assert!(String::from_utf8(body).unwrap().contains("2.0"));
        assert!(state.cache.inflight.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_spawn_revalidation_skips_when_a_refresh_is_already_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let (state, client) = stale_flask_state(&dir, "https://example.invalid/simple/", 1000);
        let held = flight_gate(&state, "pypi/flask").lock_owned().await;

        let outcome = spawn_revalidation(
            state.clone(),
            "pypi/flask".to_owned(),
            "pypi".to_owned(),
            "flask".to_owned(),
            client,
        );

        assert!(outcome.is_none());
        drop(held);
    }

    #[tokio::test]
    async fn test_revalidation_keeps_the_stale_page_when_upstream_is_unparseable() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let (state, client) = stale_flask_state(&dir, &format!("{}/simple/", server.uri()), 1000);
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"not json".to_vec(), "application/vnd.pypi.simple.v1+json"),
            )
            .mount(&server)
            .await;

        spawn_revalidation(
            state.clone(),
            "pypi/flask".to_owned(),
            "pypi".to_owned(),
            "flask".to_owned(),
            client,
        )
        .expect("the free gate lets the refresh run")
        .await
        .unwrap();

        // The unparseable upstream response is rejected, so the stale page stays and the hold is freed.
        let body = state.meta.get_index("pypi/flask").unwrap().unwrap().body;
        assert!(String::from_utf8(body).unwrap().contains("1.0"));
        assert!(state.cache.inflight.lock().unwrap().is_empty());
    }
}
