//! Upstream page fetch, revalidation, and persistence for cached indexes.

use std::sync::Arc;

use crate::policy::PypiPolicy as _;
use crate::{CoreMetadata, ProjectDetail, parse_detail, parse_detail_html, to_json};
use peryx_http::metrics::Event;
use peryx_http::state::{AppState, Index, IndexKind};
use peryx_policy::PolicyAction;
use peryx_storage::meta::CachedIndex;
use peryx_upstream::{SimpleResponse, UpstreamClient};

use super::{CacheError, NEGATIVE_TTL_SECS, is_json, mirror_route, project_negative_key, upstream_permit};

/// Fetch a page (buffered) and persist the raw body plus all file registrations in one transaction.
/// Used by the non-streaming path: HTML upstreams, HTML clients, and internal consumers.
///
/// Every outcome that a log line describes also lands in the metrics tree: revalidations (and
/// whether upstream actually changed), stale fallbacks, and hard upstream failures.
pub(super) async fn fetch_and_store(
    state: &AppState,
    key: &str,
    name: &str,
    project: &str,
    client: &UpstreamClient,
) -> Result<Option<CachedIndex>, CacheError> {
    mirror_policy(state, name).check_project(PolicyAction::Cached, project)?;
    let now = (state.clock)();
    let cached = state.meta.get_index(key)?;
    let etag = cached.as_ref().and_then(|record| record.etag.clone());
    let route = mirror_route(state, name);
    let event_project = project.to_owned();
    let _permit = upstream_permit(state, name).await?;
    match client.fetch_project(project, etag.as_deref()).await {
        Ok(response) if response.status == 200 => {
            let record = CachedIndex {
                etag: response.etag.clone(),
                last_serial: response.last_serial,
                fetched_at_unix: now,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: response.max_age,
                body: canonical_raw(project, &response)?,
            };
            if let Some(previous) = &cached {
                let changed = previous.body != record.body;
                if changed {
                    tracing::info!(%key, "upstream page changed");
                }
                let event = Event::Refresh {
                    route,
                    project: event_project,
                    changed,
                };
                state.metrics.record(event);
            }
            persist_page(state, key, name, project, &record)?;
            Ok(Some(record))
        }
        Ok(response) if response.status == 304 => {
            let mut record = cached.ok_or(CacheError::Unavailable)?;
            record.fetched_at_unix = now;
            record.fresh_secs = response.max_age.or(record.fresh_secs);
            state.meta.put_index(key, &record)?;
            state.metrics.record(Event::Refresh {
                route,
                project: event_project,
                changed: false,
            });
            Ok(Some(record))
        }
        Ok(response) if response.status == 404 => {
            state.remember_negative(project_negative_key(key), NEGATIVE_TTL_SECS);
            Ok(None)
        }
        Ok(response) => cached.map_or_else(
            || {
                state.metrics.record(Event::UpstreamError {
                    route: route.clone(),
                    project: event_project.clone(),
                });
                Err(CacheError::Unavailable)
            },
            |record| {
                tracing::warn!(%key, status = response.status, "upstream errored; serving stale page");
                state.metrics.record(Event::StaleServed {
                    route: route.clone(),
                    project: event_project.clone(),
                });
                Ok(Some(record))
            },
        ),
        Err(err) => cached.map_or_else(
            || {
                state.metrics.record(Event::UpstreamError {
                    route: route.clone(),
                    project: event_project.clone(),
                });
                Err(CacheError::Upstream(err))
            },
            |record| {
                tracing::warn!(%key, "upstream unreachable; serving stale page");
                state.metrics.record(Event::StaleServed {
                    route: route.clone(),
                    project: event_project.clone(),
                });
                Ok(Some(record))
            },
        ),
    }
}

fn mirror_policy<'a>(state: &'a AppState, name: &str) -> &'a peryx_policy::Policy {
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
/// # Errors
/// Returns [`CacheError`] when the hosted store fails; upstream failures do not error (a page with
/// a cached copy serves stale and is retried next sweep).
pub async fn refresh_stale_pages(state: &Arc<AppState>) -> Result<RefreshSummary, CacheError> {
    let now = (state.clock)();
    let mut summary = RefreshSummary::default();
    for (key, fetched_at, fresh_secs) in state.meta.list_index_pages()? {
        if now - fetched_at < fresh_secs.unwrap_or(state.ttl_secs) {
            continue;
        }
        let Some((index, client, offline, project)) = mirror_for_key(state, &key) else {
            continue;
        };
        if offline {
            continue;
        }
        if let Err(denial) = index.policy.check_project(PolicyAction::Cached, &project) {
            log_cache_sync(&index.route, &project, "denied", false, Some(&denial.reason));
            continue;
        }
        summary.checked += 1;
        let before = state.meta.get_index(&key)?.map(|record| record.body);
        let result = fetch_and_store(state, &key, &index.name, &project, client).await;
        match &result {
            Ok(Some(record)) => {
                let changed = before.as_ref() != Some(&record.body);
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
            }
        }
        result?;
    }
    Ok(summary)
}

fn log_cache_sync(index: &str, project: &str, result: &'static str, changed: bool, reason: Option<&str>) {
    peryx_http::security::Event::new("mirror_sync", result)
        .index(index)
        .project(Some(project))
        .changed(changed)
        .count(1)
        .reason(reason)
        .emit();
}

/// Map a cache key (`{cached index name}/{project}`) back to its cached index and client; the longest matching
/// name wins when one cached's name prefixes another's.
fn mirror_for_key<'a>(state: &'a AppState, key: &str) -> Option<(&'a Index, &'a UpstreamClient, bool, String)> {
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

/// The canonical raw body to persist: JSON pages verbatim, HTML pages converted once to PEP 691
/// JSON (with upstream URLs intact), so every later read has one format to deal with.
pub(super) fn canonical_raw(project: &str, response: &SimpleResponse) -> Result<Vec<u8>, CacheError> {
    if is_json(response.content_type.as_deref()) {
        return Ok(response.body.to_vec());
    }
    let parsed = parse_detail_html(project, &String::from_utf8_lossy(&response.body), &response.url);
    let parsed = parsed?;
    let detail = ProjectDetail {
        meta: parsed.meta,
        name: parsed.name,
        versions: parsed.versions,
        files: parsed.files,
    };
    Ok(to_json(&detail).into_bytes())
}

pub fn persist_page(
    state: &AppState,
    key: &str,
    name: &str,
    project: &str,
    record: &CachedIndex,
) -> Result<(), CacheError> {
    let parsed = parse_detail(&record.body)?;
    let mut files = Vec::new();
    let mut metadata = Vec::new();
    let policy = mirror_policy(state, name);
    for file in &parsed.files {
        if policy.check_file(PolicyAction::Cached, project, file).is_err() {
            continue;
        }
        let Some(sha256) = file.hashes.get("sha256") else {
            continue;
        };
        if file.url.starts_with('/') {
            continue; // a legacy record with peryx-route URLs has nothing to register
        }
        files.push((sha256.clone(), file.url.clone(), file.size));
        if let CoreMetadata::Hashes(hashes) = file.metadata()
            && let Some(digest) = hashes.get("sha256")
        {
            metadata.push((sha256.clone(), format!("{}.metadata", file.url), digest.clone()));
        }
    }
    let display = if parsed.name.is_empty() { project } else { &parsed.name };
    state
        .meta
        .put_cached_page(
            key,
            record,
            name,
            project,
            display,
            name,
            parsed.meta.project_status.as_deref(),
            parsed.meta.project_status_reason.as_deref(),
            &files,
            &metadata,
        )
        .map_err(CacheError::from)?;
    state.bump_epoch();
    Ok(())
}
