use std::io::{Read, Seek as _, Write};
use std::sync::Arc;

use crate::catalog::redact_url;
use crate::policy::PypiPolicy as _;
use crate::simple::{DetailSink, File, absolutize, stream_detail_json};
use crate::store::PypiStore as _;
use crate::store::{
    CachedIndex, ProjectGeneration, abort_project_generation, active_project_generation, begin_project_generation,
    publish_project_generation, put_project_files, recover_project_generations, refresh_project_generation,
};
use crate::{CoreMetadata, ProjectDetail, parse_detail, parse_detail_html, to_json};
use peryx_driver::state::ServingState;
use peryx_events::metrics::Observation;
use peryx_index::serving::Inflight;
use peryx_index::{Index, IndexKind};
use peryx_policy::{Policy, PolicyAction};
use peryx_storage::meta::{MetaError, MetaStore};
use peryx_upstream::UpstreamClient;
use peryx_upstream::UpstreamError;
use time::OffsetDateTime;
use url::Url;

use crate::simple_client::{SimpleClientExt as _, SimpleHead, SimpleResponse};

use super::{
    CacheError, NEGATIVE_TTL_SECS, cached_record, flight_gate, is_json, mirror_route, project_negative_key,
    release_flight, release_then, upstream_permit,
};

pub(super) async fn fetch_and_store(
    state: &ServingState,
    key: &str,
    name: &str,
    project: &str,
    client: &UpstreamClient,
) -> Result<Option<CachedIndex>, CacheError> {
    mirror_policy(state, name).check_resource(PolicyAction::Cached, project)?;
    let now = (state.clock)();
    let cached = cached_record(state, key)?;
    let etag = cached.as_ref().and_then(|record| record.etag.clone());
    let route = mirror_route(state, name);
    let event_project = project.to_owned();
    let _permit = upstream_permit(state, name).await?;
    let response = match state.upstream_routes.get(name) {
        Some(router) => router.fetch_project(project, etag.as_deref()).await,
        None => client.fetch_project(project, etag.as_deref()).await,
    };
    match response {
        Ok(response) if response.status == 200 => {
            cache_project_response(state, key, name, project, now, cached.as_ref(), &response).map(Some)
        }
        Ok(response) if response.status == 304 => {
            let mut record = cached.ok_or(CacheError::Unavailable)?;
            record.fetched_at_unix = now;
            record.fresh_secs = response.max_age.or(record.fresh_secs);
            state
                .meta
                .touch_index_freshness(key, record.fetched_at_unix, record.fresh_secs)?;
            state.metrics.record(Observation::Refresh {
                repository: route,
                resource: event_project,
                changed: false,
            });
            Ok(Some(record))
        }
        Ok(response) if response.status == 404 => state
            .meta
            .retire_cached_project(key, name, project)
            .map_err(CacheError::from)
            .map(|()| {
                super::invalidate_project(state, name, project);
                state.remember_negative(project_negative_key(key), NEGATIVE_TTL_SECS);
                None
            }),
        Ok(response)
            if response.status == 429
                && cached
                    .as_ref()
                    .is_none_or(|record| !super::servable_stale(state, record)) =>
        {
            state.metrics.record(Observation::UpstreamError {
                repository: route,
                resource: event_project,
            });
            Err(CacheError::UpstreamRateLimited {
                retry_after: response.retry_after,
            })
        }
        // Past `max_stale_secs` a stale page stops being an answer, so drop it and let the upstream
        // failure surface rather than papering over an outage with data of unbounded age.
        Ok(response) => cached
            .filter(|record| super::servable_stale(state, record))
            .map_or_else(
                || {
                    state.metrics.record(Observation::UpstreamError {
                        repository: route.clone(),
                        resource: event_project.clone(),
                    });
                    Err(CacheError::Unavailable)
                },
                |record| {
                    tracing::warn!(%key, status = response.status, "upstream errored; serving stale page");
                    state.metrics.record(Observation::StaleServed {
                        repository: route.clone(),
                        resource: event_project.clone(),
                    });
                    Ok(Some(record))
                },
            ),
        Err(err) => cached
            .filter(|record| super::servable_stale(state, record))
            .map_or_else(
                || {
                    state.metrics.record(Observation::UpstreamError {
                        repository: route.clone(),
                        resource: event_project.clone(),
                    });
                    Err(CacheError::Upstream(err))
                },
                |record| {
                    tracing::warn!(%key, "upstream unreachable; serving stale page");
                    state.metrics.record(Observation::StaleServed {
                        repository: route.clone(),
                        resource: event_project.clone(),
                    });
                    Ok(Some(record))
                },
            ),
    }
}

fn cache_project_response(
    state: &ServingState,
    key: &str,
    name: &str,
    project: &str,
    now: i64,
    previous: Option<&CachedIndex>,
    response: &SimpleResponse,
) -> Result<CachedIndex, CacheError> {
    let record = CachedIndex {
        etag: response.etag.clone(),
        last_serial: response.last_serial,
        fetched_at_unix: now,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: response.max_age,
        body: canonical_raw(project, response)?,
    };
    if let Some(previous) = previous {
        let changed = previous.body != record.body;
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
    Ok(record)
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
        summary.checked += 1;
        match &result {
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
            }
        }
        result?;
    }
    Ok(summary)
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

/// The largest project detail response peryx accepts.
///
/// A very large generated project's JSON stays well under it; the cap only stops an upstream or
/// decompressor from writing unbounded bytes into local storage.
pub const MAX_PROJECT_BYTES: u64 = 256 * 1024 * 1024;
/// The most files one project generation admits, bounding both the parse and the row count a
/// million-file generated project produces.
pub const MAX_PROJECT_FILES: u64 = 2_000_000;
/// Files committed per staging transaction, bounding one commit for a project with a huge file list.
const PROJECT_FILE_BATCH: usize = 10_000;

/// The result of synchronizing one project's remote file metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectSyncOutcome {
    /// A `200` parsed into a freshly published generation holding `files` admitted files.
    Published { files: u64 },
    /// A `304` reused the active generation, whose `files` rows are untouched.
    NotModified { files: u64 },
    /// The project does not exist upstream; any prior generation is left in place.
    Missing,
}

/// A remote project detail could not be fetched, parsed, or published.
#[derive(Debug, thiserror::Error)]
pub enum ProjectSyncError {
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Simple(#[from] crate::SimpleError),
    #[error("upstream project detail returned {0}")]
    Status(u16),
    #[error("upstream project detail exceeds the {MAX_PROJECT_BYTES}-byte limit")]
    TooLarge,
    #[error("upstream project detail exceeds the {MAX_PROJECT_FILES}-file limit")]
    TooManyFiles,
}

/// Fetch and atomically publish one project's remote file-metadata generation on `index`.
///
/// The detail page is fetched conditionally: a `304` refreshes the active generation's validators in
/// place, a `404` leaves any prior generation serviceable, and a `200` streams the body into a
/// bounded temporary file, parses it into a staging generation of policy-admitted files, and swaps
/// the active pointer only once the whole document parsed. No metadata transaction is held during the
/// upstream request, and a failed parse or publication never disturbs the previously active generation.
///
/// # Errors
/// Returns [`ProjectSyncError`] without changing the active generation when the fetch, transfer,
/// parse, or publication fails.
pub async fn sync_project_files<C: crate::SimpleClientExt + Sync>(
    client: &C,
    inflight: &Inflight,
    meta: &MetaStore,
    index: &str,
    policy: &Policy,
    project: &str,
    fallback_source: &str,
) -> Result<ProjectSyncOutcome, ProjectSyncError> {
    let (_guard, waited) = crate::sync_lock::acquire(inflight, &format!("pypi\0project\0{index}\0{project}")).await;
    if waited && let Some(active) = active_project_generation(meta, index, project)? {
        return Ok(ProjectSyncOutcome::NotModified { files: active.files });
    }
    recover_project_generations(meta, index, project)?;
    let previous = active_project_generation(meta, index, project)?;
    let head = client
        .head_project(project, previous.as_ref().and_then(|active| active.etag.as_deref()))
        .await?;
    let fetched_at_unix = OffsetDateTime::now_utc().unix_timestamp();
    match head.status {
        304 => {
            let previous = previous.ok_or(MetaError::DriverPrecondition(
                "upstream returned 304 without an active project generation".to_owned(),
            ))?;
            let refreshed = refresh_project_generation(
                meta,
                index,
                project,
                previous.generation,
                head.etag,
                head.last_modified,
                fetched_at_unix,
            );
            refreshed?;
            Ok(ProjectSyncOutcome::NotModified { files: previous.files })
        }
        404 => Ok(ProjectSyncOutcome::Missing),
        _ => publish_project_response(meta, index, policy, project, fallback_source, head, fetched_at_unix).await,
    }
}

async fn publish_project_response(
    meta: &MetaStore,
    index: &str,
    policy: &Policy,
    project: &str,
    fallback_source: &str,
    mut head: SimpleHead,
    fetched_at_unix: i64,
) -> Result<ProjectSyncOutcome, ProjectSyncError> {
    match head.status {
        200 if head.content_length.is_some_and(|bytes| bytes > MAX_PROJECT_BYTES) => {
            return Err(ProjectSyncError::TooLarge);
        }
        200 => {}
        status => return Err(ProjectSyncError::Status(status)),
    }
    let upstream = head.source.take();
    let base = head.url.clone();
    let final_url = redact_url(head.url.as_str());
    let format = if is_json(head.content_type.as_deref()) {
        "json"
    } else {
        "html"
    };
    let etag = head.etag.clone();
    let last_modified = head.last_modified.clone();
    let last_serial = head.last_serial;
    let mut file = tempfile::tempfile()?;
    let bytes = write_project_stream(head.into_stream(), &mut file, MAX_PROJECT_BYTES).await?;
    file.flush()?;
    file.rewind()?;

    let (generation, expected_active) = begin_project_generation(meta, index, project)?;
    let parsed = parse_project(
        &mut file,
        ParseProject {
            format,
            base: &base,
            meta,
            index,
            policy,
            project,
            generation,
            upstream: upstream.as_deref(),
            max_files: MAX_PROJECT_FILES,
        },
    );
    let (files, detail) = match parsed {
        Ok(result) => result,
        Err(err) => {
            abort_project_generation(meta, index, project, generation)?;
            return Err(err);
        }
    };
    let source = upstream.unwrap_or_else(|| redact_url(fallback_source));
    let generation_record = ProjectGeneration {
        generation,
        source,
        url: final_url,
        format: format.to_owned(),
        etag,
        last_modified,
        last_serial,
        fetched_at_unix,
        bytes,
        files,
        versions: detail.versions,
        project_status: detail.project_status,
        project_status_reason: detail.project_status_reason,
    };
    publish_project_generation(meta, index, project, expected_active, generation_record)?;
    recover_project_generations(meta, index, project)?;
    Ok(ProjectSyncOutcome::Published { files })
}

async fn write_project_stream<S>(mut stream: S, writer: &mut impl Write, limit: u64) -> Result<u64, ProjectSyncError>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, UpstreamError>> + Unpin,
{
    use futures_util::TryStreamExt as _;
    let mut bytes = 0_u64;
    while let Some(chunk) = stream.try_next().await? {
        write_project_chunk(writer, &chunk, &mut bytes, limit)?;
    }
    Ok(bytes)
}

fn write_project_chunk(
    writer: &mut impl Write,
    chunk: &[u8],
    bytes: &mut u64,
    limit: u64,
) -> Result<(), ProjectSyncError> {
    *bytes = bytes
        .checked_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX))
        .filter(|bytes| *bytes <= limit)
        .ok_or(ProjectSyncError::TooLarge)?;
    writer.write_all(chunk)?;
    Ok(())
}

/// The detail header fields a generation records once its files drain.
#[derive(Debug)]
struct ParsedDetailHeader {
    versions: Vec<String>,
    project_status: Option<String>,
    project_status_reason: Option<String>,
}

#[derive(Clone, Copy)]
struct ParseProject<'a> {
    format: &'a str,
    base: &'a Url,
    meta: &'a MetaStore,
    index: &'a str,
    policy: &'a Policy,
    project: &'a str,
    generation: u64,
    upstream: Option<&'a str>,
    max_files: u64,
}

fn parse_project(
    reader: &mut impl Read,
    input: ParseProject<'_>,
) -> Result<(u64, ParsedDetailHeader), ProjectSyncError> {
    let ParseProject {
        format,
        base,
        meta,
        index,
        policy,
        project,
        generation,
        upstream,
        max_files,
    } = input;
    let mut batcher = FileBatcher::new(meta, index, project, policy, generation, upstream, max_files);
    let header = if format == "json" {
        let detail = stream_detail_json(reader, base, &mut batcher)?;
        ParsedDetailHeader {
            versions: detail.versions,
            project_status: detail.meta.project_status,
            project_status_reason: detail.meta.project_status_reason,
        }
    } else {
        let mut body = String::new();
        reader.read_to_string(&mut body)?;
        let detail = parse_detail_html(project, &body, base)?;
        for mut parsed in detail.files {
            absolutize(base, &mut parsed.url);
            batcher.file(parsed)?;
        }
        ParsedDetailHeader {
            versions: detail.versions,
            project_status: detail.meta.project_status,
            project_status_reason: detail.meta.project_status_reason,
        }
    };
    Ok((batcher.finish()?, header))
}

/// Collects policy-admitted files into bounded batches and commits each into the staging generation.
struct FileBatcher<'a> {
    meta: &'a MetaStore,
    index: &'a str,
    project: &'a str,
    policy: &'a Policy,
    generation: u64,
    upstream: Option<&'a str>,
    max_files: u64,
    batch: Vec<File>,
    admitted: u64,
    seen: u64,
}

impl<'a> FileBatcher<'a> {
    fn new(
        meta: &'a MetaStore,
        index: &'a str,
        project: &'a str,
        policy: &'a Policy,
        generation: u64,
        upstream: Option<&'a str>,
        max_files: u64,
    ) -> Self {
        Self {
            meta,
            index,
            project,
            policy,
            generation,
            upstream,
            max_files,
            batch: Vec::with_capacity(PROJECT_FILE_BATCH),
            admitted: 0,
            seen: 0,
        }
    }

    fn flush(&mut self) -> Result<(), ProjectSyncError> {
        let written = put_project_files(
            self.meta,
            self.index,
            self.project,
            self.generation,
            self.index,
            self.upstream,
            &self.batch,
        );
        self.admitted += written?;
        self.batch.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<u64, ProjectSyncError> {
        self.flush()?;
        Ok(self.admitted)
    }
}

impl DetailSink for FileBatcher<'_> {
    type Error = ProjectSyncError;

    fn file(&mut self, file: File) -> Result<(), ProjectSyncError> {
        self.seen += 1;
        if self.seen > self.max_files {
            return Err(ProjectSyncError::TooManyFiles);
        }
        // A file peryx cannot content-address or the policy denies is left out of the generation, so
        // only a servable file is ever exposed to an installer.
        if file.sha256().is_none()
            || self
                .policy
                .check_file(PolicyAction::Cached, self.project, &file)
                .is_err()
        {
            return Ok(());
        }
        self.batch.push(file);
        if self.batch.len() == PROJECT_FILE_BATCH {
            self.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/cache/fetch/fence_tests.rs"]
mod fence_tests;

#[cfg(test)]
#[path = "../../tests/unit/cache/fetch/sync_tests.rs"]
mod sync_tests;
