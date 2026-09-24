use std::sync::Arc;

use crate::policy::PypiPolicy as _;
use crate::simple::absolutize;
use crate::store::CachedIndex;
use crate::store::PypiStore as _;
use crate::{CoreMetadata, ProjectDetail, parse_detail, parse_detail_html, to_json};
use peryx_driver::state::ServingState;
use peryx_events::metrics::Observation;
use peryx_index::{Index, IndexKind};
use peryx_policy::PolicyAction;
use peryx_upstream::UpstreamClient;
use url::Url;

use crate::simple_client::{CachedValidators, SimpleClientExt as _, SimpleResponse};

use super::{
    CacheError, NEGATIVE_TTL_SECS, cached_record, flight_gate, is_json, mirror_route, project_negative_key,
    release_flight, release_then, upstream_permit,
};

/// What one conditional upstream fetch did to a project's cached page.
pub(super) enum PageFetch {
    /// A `200` stored a body; `changed` when it differs from the one peryx held, or there was none.
    Stored { record: CachedIndex, changed: bool },
    /// A `304` confirmed the stored body and advanced its freshness.
    Revalidated(CachedIndex),
    /// A `404` retired the project.
    Missing,
    /// Upstream answered with a failure or not at all; `stale` is the stored page still inside its stale
    /// bound, and `status` the failing response's status when there was one.
    Failed {
        error: CacheError,
        status: Option<u16>,
        stale: Option<CachedIndex>,
    },
}

/// Fetch `project`'s page conditionally and apply the answer to the page cache. The caller holds the
/// page's flight.
///
/// # Errors
/// Returns [`CacheError`] when policy denies caching the project or the store fails; upstream failures
/// are [`PageFetch::Failed`].
pub(super) async fn fetch_page(
    state: &ServingState,
    key: &str,
    name: &str,
    project: &str,
    client: &UpstreamClient,
) -> Result<PageFetch, CacheError> {
    mirror_policy(state, name).check_resource(PolicyAction::Cached, project)?;
    let now = (state.clock)();
    let cached = cached_record(state, key)?;
    let _permit = upstream_permit(state, name).await?;
    let validators = cached_validators(cached.as_ref());
    let response = match state.upstream_routes.get(name) {
        Some(router) => router.fetch_project(project, validators).await,
        None => client.fetch_project(project, validators).await,
    };
    let servable = |cached: Option<CachedIndex>| cached.filter(|record| super::servable_stale(state, record));
    Ok(match response {
        Ok(response) if response.status == 200 => {
            let (record, changed) = cache_project_response(state, key, name, project, now, cached.as_ref(), &response)?;
            PageFetch::Stored { record, changed }
        }
        Ok(response) if response.status == 304 => {
            let mut record = revalidated(cached, response.source.as_deref())?;
            record.fetched_at_unix = now;
            record.fresh_secs = response.max_age.or(record.fresh_secs);
            state
                .meta
                .touch_index_freshness(key, record.fetched_at_unix, record.fresh_secs)?;
            state.metrics.record(Observation::Refresh {
                repository: mirror_route(state, name),
                resource: project.to_owned(),
                changed: false,
            });
            PageFetch::Revalidated(record)
        }
        Ok(response) if response.status == 404 => {
            state.meta.retire_cached_project(key, name, project)?;
            super::invalidate_project(state, name, project);
            state.remember_negative(project_negative_key(key), NEGATIVE_TTL_SECS);
            PageFetch::Missing
        }
        Ok(response) if response.status == 429 => PageFetch::Failed {
            error: CacheError::UpstreamRateLimited {
                retry_after: response.retry_after,
            },
            status: Some(429),
            stale: servable(cached),
        },
        Ok(response) => PageFetch::Failed {
            error: CacheError::Unavailable,
            status: Some(response.status),
            stale: servable(cached),
        },
        Err(err) => PageFetch::Failed {
            error: CacheError::Upstream(err),
            status: None,
            stale: servable(cached),
        },
    })
}

/// Fetch `project`'s page for a request, serving a stored page inside its stale bound when upstream
/// fails. Past `max_stale_secs` a stale page stops being an answer, so the upstream failure surfaces
/// rather than papering over an outage with data of unbounded age.
pub(super) async fn fetch_and_store(
    state: &ServingState,
    key: &str,
    name: &str,
    project: &str,
    client: &UpstreamClient,
) -> Result<Option<CachedIndex>, CacheError> {
    match fetch_page(state, key, name, project, client).await? {
        PageFetch::Stored { record, .. } | PageFetch::Revalidated(record) => Ok(Some(record)),
        PageFetch::Missing => Ok(None),
        PageFetch::Failed {
            status,
            stale: Some(record),
            ..
        } => {
            if let Some(status) = status {
                tracing::warn!(%key, status, "upstream errored; serving stale page");
            } else {
                tracing::warn!(%key, "upstream unreachable; serving stale page");
            }
            state.metrics.record(Observation::StaleServed {
                repository: mirror_route(state, name),
                resource: project.to_owned(),
            });
            Ok(Some(record))
        }
        PageFetch::Failed { error, stale: None, .. } => {
            state.metrics.record(Observation::UpstreamError {
                repository: mirror_route(state, name),
                resource: project.to_owned(),
            });
            Err(error)
        }
    }
}

/// The stored page a `304` from `answered` revalidates.
///
/// A validator belongs to the one stored response it arrived with, so a `304` attributed to any other
/// source says nothing about the page peryx holds - a routed candidate that never produced it cannot
/// declare it unchanged.
pub(super) fn revalidated(cached: Option<CachedIndex>, answered: Option<&str>) -> Result<CachedIndex, CacheError> {
    cached
        .filter(|record| record.source.as_deref() == answered)
        .ok_or(CacheError::Unavailable)
}

/// The validators a cached page may be revalidated with, bound to the source that produced it.
pub(super) fn cached_validators(cached: Option<&CachedIndex>) -> CachedValidators<'_> {
    CachedValidators {
        source: cached.and_then(|record| record.source.as_deref()),
        etag: cached.and_then(|record| record.etag.as_deref()),
        last_modified: cached.and_then(|record| record.last_modified.as_deref()),
    }
}

/// Store `response` as `project`'s page, returning the record and whether its body differs from `previous`, or
/// there was none.
fn cache_project_response(
    state: &ServingState,
    key: &str,
    name: &str,
    project: &str,
    now: i64,
    previous: Option<&CachedIndex>,
    response: &SimpleResponse,
) -> Result<(CachedIndex, bool), CacheError> {
    let record = CachedIndex {
        source: response.source.clone(),
        etag: response.etag.clone(),
        last_modified: response.last_modified.clone(),
        last_serial: response.last_serial,
        fetched_at_unix: now,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: response.max_age,
        body: canonical_raw(project, response)?,
    };
    let changed = previous.is_none_or(|previous| previous.body != record.body);
    if previous.is_some() {
        if changed {
            tracing::info!(%key, "upstream page changed");
        }
        state.metrics.record(Observation::Refresh {
            repository: mirror_route(state, name),
            resource: project.to_owned(),
            changed,
        });
    }
    persist_page_from(state, key, name, project, &record, response.source.as_deref())?;
    Ok((record, changed))
}

fn mirror_policy<'a>(state: &'a ServingState, name: &str) -> &'a peryx_policy::Policy {
    &state
        .indexes
        .iter()
        .find(|index| index.name == name)
        .expect("index policy belongs to a configured index")
        .policy
}

/// One background refresh sweep's outcome.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RefreshSummary {
    /// Stale pages revalidated against upstream.
    pub checked: usize,
    /// Pages whose upstream content differed from the cache.
    pub changed: usize,
}

/// Revalidate every cached page older than the TTL.
///
/// Upstream changes are caught within one refresh period even for pages nobody is requesting.
/// Pages run sequentially: a large cache trickles out as cheap conditional requests (`ETag` hits
/// answer 304 with no body) instead of a burst against upstream. Each revalidation is logged and
/// counted through the same events as the on-demand path.
///
/// Each page is revalidated under that project's flight, the one the request path and background
/// revalidation already take, so the sweep cannot fetch alongside them and commit an older body over
/// their result. A page a writer published while the sweep queued is left alone, and so is one a
/// writer removed: the listing is a snapshot, and refetching a row an operator purged would put the
/// project back.
///
/// # Errors
/// Returns [`CacheError`] when the hosted store fails; upstream failures do not error (a page with
/// a cached copy serves stale and is retried next sweep).
pub async fn refresh_stale_pages(state: &Arc<ServingState>) -> Result<RefreshSummary, CacheError> {
    let now = (state.clock)();
    let mut summary = RefreshSummary::default();
    for (key, fetched_at, fresh_secs) in state.meta.list_index_pages()? {
        if now - fetched_at < super::freshness_secs(state.ttl_secs, fresh_secs) {
            continue;
        }
        let Some((index, client, offline, project)) = mirror_for_key(state, &key) else {
            continue;
        };
        if offline {
            continue;
        }
        if let Err(denial) = index.policy.check_resource(PolicyAction::Cached, &project) {
            log_cache_sync(&index.route, &project, "denied", false, Some(&denial.reason));
            continue;
        }
        // Re-read under the flight: a sweep that queued behind another writer has to revalidate the
        // page that writer published, not the row it read before queueing, whose older body would
        // otherwise win the commit ordering.
        let (before, result) = {
            let gate = flight_gate(state, &key);
            let guard = gate.lock_owned().await;
            let Some(current) = cached_record(state, &key)? else {
                release_flight(state, &key, guard);
                continue;
            };
            if super::is_fresh(state, &current) {
                release_flight(state, &key, guard);
                continue;
            }
            let result = fetch_and_store(state, &key, &index.name, &project, client).await;
            release_then(state, &key, guard, || (current.body, result))
        };
        match result {
            Ok(Some(record)) => {
                let changed = before != record.body;
                if changed {
                    summary.changed += 1;
                }
                log_cache_sync(&index.route, &project, "success", changed, None);
            }
            Ok(None) => log_cache_sync(
                &index.route,
                &project,
                "noop",
                false,
                Some("project not found upstream"),
            ),
            Err(err) => {
                let reason = err.user_message();
                log_cache_sync(&index.route, &project, "failure", false, Some(&reason));
                if !is_recoverable_refresh_error(&err) {
                    return Err(err);
                }
            }
        }
        summary.checked += 1;
    }
    Ok(summary)
}

const fn is_recoverable_refresh_error(error: &CacheError) -> bool {
    matches!(
        error,
        CacheError::Upstream(_)
            | CacheError::Parse(_)
            | CacheError::Simple(_)
            | CacheError::Unavailable
            | CacheError::OfflineMissing(_)
            | CacheError::RateLimited { .. }
            | CacheError::UpstreamRateLimited { .. }
    )
}

fn log_cache_sync(index: &str, project: &str, result: &'static str, changed: bool, reason: Option<&str>) {
    peryx_events::security::Event::new("mirror_sync", result)
        .index(index)
        .resource(Some(project))
        .changed(changed)
        .count(1)
        .reason(reason)
        .emit();
}

fn mirror_for_key<'a>(state: &'a ServingState, key: &str) -> Option<(&'a Index, &'a UpstreamClient, bool, String)> {
    state
        .indexes
        .iter()
        .filter_map(|index| match &index.kind {
            IndexKind::Cached { client, offline } => {
                let project = key.strip_prefix(&index.name)?.strip_prefix('/')?;
                Some((index, client, *offline, project.to_owned()))
            }
            IndexKind::Hosted { .. } | IndexKind::Virtual { .. } => None,
        })
        .max_by_key(|(index, _, _, _)| index.name.len())
}

/// The canonical raw body to persist: file URLs resolved against the response URL and, for HTML
/// pages, converted once to PEP 691 JSON, so every later read has one format with absolute URLs.
///
/// Resolving here is what lets the read path treat a leading-`/` URL as a peryx-local record: a
/// root-relative upstream URL has already been made absolute by the time it lands in the cache.
pub(super) fn canonical_raw(project: &str, response: &SimpleResponse) -> Result<Vec<u8>, CacheError> {
    if is_json(response.content_type.as_deref()) {
        return canonical_json(&response.body, &response.url);
    }
    let parsed = parse_detail_html(project, &String::from_utf8_lossy(&response.body), &response.url)?;
    let detail = ProjectDetail {
        meta: parsed.meta,
        name: parsed.name,
        versions: parsed.versions,
        files: parsed.files,
    };
    Ok(to_json(&detail).into_bytes())
}

/// Normalize a PEP 691 JSON body into the persisted form: every file URL made absolute against
/// `base`, then reserialized. The streaming and buffered paths both persist through this, so
/// identical upstream content compares byte-equal on a later revalidation.
///
/// # Errors
/// Returns [`CacheError`] when `body` is not a valid PEP 691 project detail document.
pub(super) fn canonical_json(body: &[u8], base: &Url) -> Result<Vec<u8>, CacheError> {
    let mut parsed = parse_detail(body)?;
    for file in &mut parsed.files {
        absolutize(base, &mut file.url);
        file.provenance.retain_secure_url();
    }
    let detail = ProjectDetail {
        meta: parsed.meta,
        name: parsed.name,
        versions: parsed.versions,
        files: parsed.files,
    };
    Ok(to_json(&detail).into_bytes())
}

pub(super) fn persist_page_from(
    state: &ServingState,
    key: &str,
    name: &str,
    project: &str,
    record: &CachedIndex,
    upstream: Option<&str>,
) -> Result<(), CacheError> {
    let parsed = parse_detail(&record.body)?;
    let mut files = Vec::new();
    let mut attestations = Vec::new();
    let policy = mirror_policy(state, name);
    for file in &parsed.files {
        if policy.check_file(PolicyAction::Cached, project, file).is_err() {
            continue;
        }
        let Some(sha256) = file.hashes.get("sha256") else {
            continue;
        };
        let metadata = match file.metadata() {
            CoreMetadata::Hashes(hashes) => hashes
                .get("sha256")
                .map(|digest| (crate::stream::metadata_sibling(&file.url), digest.clone())),
            CoreMetadata::Absent | CoreMetadata::Available => None,
        };
        files.push(crate::store::PublishedFileWrite {
            sha256: sha256.clone(),
            filename: file.filename.clone(),
            url: file.url.clone(),
            size: file.size,
            metadata,
        });
        if let Some(url) = file.provenance.secure_url() {
            attestations.push((sha256.clone(), file.filename.clone(), url.to_owned()));
        }
    }
    let display = if parsed.name.is_empty() { project } else { &parsed.name };
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
            project_status: parsed.meta.project_status.as_deref(),
            project_status_reason: parsed.meta.project_status_reason.as_deref(),
            files: &files,
            attestations: &attestations,
        })
        .map_err(CacheError::from)?;
    super::invalidate_project(state, name, project);
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/cache/fetch/fence_tests.rs"]
mod fence_tests;
