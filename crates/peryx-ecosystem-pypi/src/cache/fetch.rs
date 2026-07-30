//! Upstream page fetch, revalidation, and persistence for cached indexes.

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
use peryx_events::metrics::Event;
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
    CacheError, NEGATIVE_TTL_SECS, cached_record, is_json, mirror_route, project_negative_key, upstream_permit,
};

/// Fetch a page (buffered) and persist the raw body plus all file registrations in one transaction.
/// Used by the non-streaming path: HTML upstreams, HTML clients, and internal consumers.
///
/// Every outcome that a log line describes also lands in the metrics tree: revalidations (and
/// whether upstream actually changed), stale fallbacks, and hard upstream failures.
pub(super) async fn fetch_and_store(
    state: &ServingState,
    key: &str,
    name: &str,
    project: &str,
    client: &UpstreamClient,
) -> Result<Option<CachedIndex>, CacheError> {
    mirror_policy(state, name).check_project(PolicyAction::Cached, project)?;
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
            persist_page_from(state, key, name, project, &record, response.source.as_deref())?;
            Ok(Some(record))
        }
        Ok(response) if response.status == 304 => {
            let mut record = cached.ok_or(CacheError::Unavailable)?;
            record.fetched_at_unix = now;
            record.fresh_secs = response.max_age.or(record.fresh_secs);
            state
                .meta
                .touch_index_freshness(key, record.fetched_at_unix, record.fresh_secs)?;
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
        // Past `max_stale_secs` a stale page stops being an answer, so drop it and let the upstream
        // failure surface rather than papering over an outage with data of unbounded age.
        Ok(response) => cached
            .filter(|record| super::servable_stale(state, record))
            .map_or_else(
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
        Err(err) => cached
            .filter(|record| super::servable_stale(state, record))
            .map_or_else(
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
    peryx_events::security::Event::new("mirror_sync", result)
        .index(index)
        .project(Some(project))
        .changed(changed)
        .count(1)
        .reason(reason)
        .emit();
}

/// Map a cache key (`{cached index name}/{project}`) back to its cached index and client; the longest matching
/// name wins when one cached's name prefixes another's.
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
    }
    let detail = ProjectDetail {
        meta: parsed.meta,
        name: parsed.name,
        versions: parsed.versions,
        files: parsed.files,
    };
    Ok(to_json(&detail).into_bytes())
}

#[cfg(test)]
pub fn persist_page(
    state: &ServingState,
    key: &str,
    name: &str,
    project: &str,
    record: &CachedIndex,
) -> Result<(), CacheError> {
    persist_page_from(state, key, name, project, record, None)
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
            metadata.push((
                sha256.clone(),
                crate::stream::metadata_sibling(&file.url),
                digest.clone(),
            ));
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
            upstream,
            parsed.meta.project_status.as_deref(),
            parsed.meta.project_status_reason.as_deref(),
            &files,
            &metadata,
        )
        .map_err(CacheError::from)?;
    state.invalidate_project(project);
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
    head: SimpleHead,
    fetched_at_unix: i64,
) -> Result<ProjectSyncOutcome, ProjectSyncError> {
    match head.status {
        200 if head.content_length.is_some_and(|bytes| bytes > MAX_PROJECT_BYTES) => {
            return Err(ProjectSyncError::TooLarge);
        }
        200 => {}
        status => return Err(ProjectSyncError::Status(status)),
    }
    let source = head.source.clone().unwrap_or_else(|| redact_url(fallback_source));
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
    let mut file = tempfile::NamedTempFile::new()?;
    let bytes = write_project_stream(head.into_stream(), file.as_file_mut(), MAX_PROJECT_BYTES).await?;
    file.flush()?;
    file.rewind()?;

    let (generation, expected_active) = begin_project_generation(meta, index, project)?;
    let parsed = parse_project(
        file.as_file_mut(),
        format,
        &base,
        meta,
        index,
        policy,
        project,
        generation,
        &source,
        MAX_PROJECT_FILES,
    );
    let (files, detail) = match parsed {
        Ok(result) => result,
        Err(err) => {
            abort_project_generation(meta, index, project, generation)?;
            return Err(err);
        }
    };
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

#[allow(
    clippy::too_many_arguments,
    reason = "the parse threads every staging input through one call"
)]
fn parse_project(
    reader: &mut impl Read,
    format: &str,
    base: &Url,
    meta: &MetaStore,
    index: &str,
    policy: &Policy,
    project: &str,
    generation: u64,
    source: &str,
    max_files: u64,
) -> Result<(u64, ParsedDetailHeader), ProjectSyncError> {
    let mut batcher = FileBatcher::new(meta, index, project, policy, generation, source, max_files);
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
    source: &'a str,
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
        source: &'a str,
        max_files: u64,
    ) -> Self {
        Self {
            meta,
            index,
            project,
            policy,
            generation,
            source,
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
            self.source,
            None,
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
mod sync_tests {
    use std::collections::BTreeMap;

    use peryx_index::serving::Inflight;
    use peryx_policy::Policy;
    use peryx_storage::meta::MetaStore;
    use peryx_upstream::UpstreamClient;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        MAX_PROJECT_BYTES, PROJECT_FILE_BATCH, ProjectSyncError, ProjectSyncOutcome, parse_project,
        publish_project_response, sync_project_files, write_project_chunk,
    };
    use crate::SimpleClientExt as _;
    use crate::simple::{CoreMetadata, File, Provenance, Yanked};
    use crate::store::PypiStore as _;
    use crate::store::{
        ProjectGeneration, active_project_generation, begin_project_generation, list_project_files, project_meta_state,
        publish_project_generation, put_project_files,
    };

    const JSON: &str = "application/vnd.pypi.simple.v1+json";

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        (dir, meta)
    }

    fn file(filename: &str, sha256: &str) -> File {
        File {
            filename: filename.to_owned(),
            url: format!("https://files.example/{filename}"),
            hashes: BTreeMap::from([("sha256".to_owned(), sha256.to_owned())]),
            requires_python: Some(">=3.8".to_owned()),
            size: Some(10),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        }
    }

    fn seed_active(meta: &MetaStore, index: &str, project: &str, etag: &str, files: &[File]) -> u64 {
        let (id, expected) = begin_project_generation(meta, index, project).unwrap();
        let admitted = put_project_files(meta, index, project, id, index, None, files).unwrap();
        publish_project_generation(
            meta,
            index,
            project,
            expected,
            ProjectGeneration {
                generation: id,
                source: index.to_owned(),
                url: "https://pypi.org/simple/flask/".to_owned(),
                format: "json".to_owned(),
                etag: Some(etag.to_owned()),
                last_modified: None,
                last_serial: None,
                fetched_at_unix: 1,
                bytes: 1,
                files: admitted,
                versions: Vec::new(),
                project_status: None,
                project_status_reason: None,
            },
        )
        .unwrap();
        id
    }

    fn client_for(server: &MockServer) -> UpstreamClient {
        UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap()
    }

    #[tokio::test]
    async fn test_sync_publishes_a_json_detail() {
        let server = MockServer::start().await;
        let body = format!(
            r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","versions":["1.0"],"files":[
                {{"filename":"flask-1.0-py3-none-any.whl","url":"flask-1.0-py3-none-any.whl","hashes":{{"sha256":"{a}"}},"size":10}},
                {{"filename":"flask-1.0.tar.gz","url":"flask-1.0.tar.gz","hashes":{{"sha256":"{b}"}}}}]}}"#,
            a = "a".repeat(64),
            b = "b".repeat(64),
        );
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "v1")
                    .set_body_raw(body, JSON),
            )
            .mount(&server)
            .await;
        let client = client_for(&server);
        let (_dir, meta) = store();

        let outcome = sync_project_files(
            &client,
            &Inflight::default(),
            &meta,
            "pypi",
            &Policy::default(),
            "flask",
            client.base_url(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ProjectSyncOutcome::Published { files: 2 });
        let files = list_project_files(&meta, "pypi", "flask").unwrap();
        assert_eq!(files.len(), 2);
        let active = active_project_generation(&meta, "pypi", "flask").unwrap().unwrap();
        assert_eq!(active.format, "json");
        assert_eq!(active.etag.as_deref(), Some("v1"));
        assert!(meta.get_file_url(&"a".repeat(64)).unwrap().is_some());
    }

    #[tokio::test]
    async fn test_sync_html_and_json_agree_on_shared_fields() {
        let sha = "a".repeat(64);
        let json = format!(
            r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","files":[
                {{"filename":"flask-1.0.tar.gz","url":"https://files.example/flask-1.0.tar.gz","hashes":{{"sha256":"{sha}"}},"requires-python":">=3.8","size":10}}]}}"#,
        );
        let html = format!(
            r#"<!DOCTYPE html><html><body><a href="https://files.example/flask-1.0.tar.gz#sha256={sha}" data-requires-python="&gt;=3.8" data-size="10">flask-1.0.tar.gz</a></body></html>"#,
        );

        let mut listed = Vec::new();
        for (format, body, media) in [
            ("json", json, JSON),
            ("html", html, "application/vnd.pypi.simple.v1+html"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/simple/flask/"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(body, media))
                .mount(&server)
                .await;
            let client = client_for(&server);
            let (_dir, meta) = store();
            sync_project_files(
                &client,
                &Inflight::default(),
                &meta,
                format,
                &Policy::default(),
                "flask",
                client.base_url(),
            )
            .await
            .unwrap();
            listed.push(list_project_files(&meta, format, "flask").unwrap());
        }

        let (json_files, html_files) = (&listed[0], &listed[1]);
        assert_eq!(json_files.len(), 1);
        assert_eq!(json_files[0].filename, html_files[0].filename);
        assert_eq!(json_files[0].url, html_files[0].url);
        assert_eq!(json_files[0].hashes, html_files[0].hashes);
        assert_eq!(json_files[0].size, html_files[0].size);
        assert_eq!(json_files[0].requires_python, html_files[0].requires_python);
    }

    #[tokio::test]
    async fn test_sync_304_reuses_the_active_generation() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .and(header("if-none-match", "v1"))
            .respond_with(ResponseTemplate::new(304).insert_header("etag", "v2"))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let (_dir, meta) = store();
        let id = seed_active(
            &meta,
            "pypi",
            "flask",
            "v1",
            &[file("flask-1.0.tar.gz", &"a".repeat(64))],
        );

        let outcome = sync_project_files(
            &client,
            &Inflight::default(),
            &meta,
            "pypi",
            &Policy::default(),
            "flask",
            client.base_url(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ProjectSyncOutcome::NotModified { files: 1 });
        let active = active_project_generation(&meta, "pypi", "flask").unwrap().unwrap();
        assert_eq!(
            active.generation, id,
            "a 304 keeps the same generation, so artifact placement is untouched"
        );
        assert_eq!(active.etag.as_deref(), Some("v2"));
        assert_eq!(list_project_files(&meta, "pypi", "flask").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_sync_304_without_an_active_generation_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let (_dir, meta) = store();

        let error = sync_project_files(
            &client,
            &Inflight::default(),
            &meta,
            "pypi",
            &Policy::default(),
            "flask",
            client.base_url(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ProjectSyncError::Store(_)));
    }

    #[tokio::test]
    async fn test_sync_404_leaves_the_prior_generation_serviceable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let (_dir, meta) = store();
        seed_active(
            &meta,
            "pypi",
            "flask",
            "v1",
            &[file("flask-1.0.tar.gz", &"a".repeat(64))],
        );

        let outcome = sync_project_files(
            &client,
            &Inflight::default(),
            &meta,
            "pypi",
            &Policy::default(),
            "flask",
            client.base_url(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ProjectSyncOutcome::Missing);
        assert_eq!(list_project_files(&meta, "pypi", "flask").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_sync_parse_failure_preserves_the_active_generation() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("not json", JSON))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let (_dir, meta) = store();
        let id = seed_active(
            &meta,
            "pypi",
            "flask",
            "v1",
            &[file("flask-1.0.tar.gz", &"a".repeat(64))],
        );

        let error = sync_project_files(
            &client,
            &Inflight::default(),
            &meta,
            "pypi",
            &Policy::default(),
            "flask",
            client.base_url(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ProjectSyncError::Simple(_)));
        let state = project_meta_state(&meta, "pypi", "flask").unwrap();
        assert_eq!(state.active.unwrap().generation, id);
        assert!(state.staging.is_none());
        assert_eq!(list_project_files(&meta, "pypi", "flask").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_sync_replaces_the_active_generation_and_sweeps_the_retired_one() {
        let server = MockServer::start().await;
        let body = format!(
            r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","files":[
                {{"filename":"flask-2.0.tar.gz","url":"flask-2.0.tar.gz","hashes":{{"sha256":"{b}"}}}},
                {{"filename":"flask-2.0-py3-none-any.whl","url":"flask-2.0-py3-none-any.whl","hashes":{{"sha256":"{c}"}}}}]}}"#,
            b = "b".repeat(64),
            c = "c".repeat(64),
        );
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, JSON))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let (_dir, meta) = store();
        seed_active(
            &meta,
            "pypi",
            "flask",
            "v1",
            &[file("flask-1.0.tar.gz", &"a".repeat(64))],
        );

        let outcome = sync_project_files(
            &client,
            &Inflight::default(),
            &meta,
            "pypi",
            &Policy::default(),
            "flask",
            client.base_url(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ProjectSyncOutcome::Published { files: 2 });
        let files = list_project_files(&meta, "pypi", "flask").unwrap();
        assert_eq!(files.len(), 2, "only the new generation's files remain servable");
        let state = project_meta_state(&meta, "pypi", "flask").unwrap();
        assert!(
            state.retired.is_none(),
            "the displaced generation is swept after publication"
        );
    }

    #[tokio::test]
    async fn test_sync_skips_a_file_without_a_hash() {
        let server = MockServer::start().await;
        let body = format!(
            r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","files":[
                {{"filename":"flask-1.0.tar.gz","url":"flask-1.0.tar.gz","hashes":{{"sha256":"{a}"}}}},
                {{"filename":"unhashed.tar.gz","url":"unhashed.tar.gz","hashes":{{}}}}]}}"#,
            a = "a".repeat(64),
        );
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, JSON))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let (_dir, meta) = store();

        let outcome = sync_project_files(
            &client,
            &Inflight::default(),
            &meta,
            "pypi",
            &Policy::default(),
            "flask",
            client.base_url(),
        )
        .await
        .unwrap();

        assert_eq!(outcome, ProjectSyncOutcome::Published { files: 1 });
    }

    #[tokio::test]
    async fn test_sync_returns_the_upstream_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let (_dir, meta) = store();

        let error = sync_project_files(
            &client,
            &Inflight::default(),
            &meta,
            "pypi",
            &Policy::default(),
            "flask",
            client.base_url(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ProjectSyncError::Status(500)));
    }

    #[tokio::test]
    async fn test_sync_coalesces_concurrent_fetches() {
        let server = MockServer::start().await;
        let body = format!(
            r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","files":[
                {{"filename":"flask-1.0.tar.gz","url":"flask-1.0.tar.gz","hashes":{{"sha256":"{a}"}}}}]}}"#,
            a = "a".repeat(64),
        );
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, JSON))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let (_dir, meta) = store();
        let inflight = Inflight::default();
        let policy = Policy::default();

        let (first, second) = tokio::join!(
            sync_project_files(&client, &inflight, &meta, "pypi", &policy, "flask", client.base_url()),
            sync_project_files(&client, &inflight, &meta, "pypi", &policy, "flask", client.base_url()),
        );

        assert_eq!(first.unwrap(), ProjectSyncOutcome::Published { files: 1 });
        assert_eq!(second.unwrap(), ProjectSyncOutcome::NotModified { files: 1 });
    }

    #[tokio::test]
    async fn test_sync_scopes_same_key_coalescing_to_one_store() {
        let first_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .and(header("if-none-match", "v1"))
            .respond_with(ResponseTemplate::new(304).set_delay(std::time::Duration::from_millis(100)))
            .mount(&first_server)
            .await;
        let second_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .and(header("if-none-match", "v1"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&second_server)
            .await;
        let first_client = client_for(&first_server);
        let second_client = client_for(&second_server);
        let (_first_dir, first_meta) = store();
        let (_second_dir, second_meta) = store();
        let previous = [file("flask-1.0.tar.gz", &"a".repeat(64))];
        seed_active(&first_meta, "pypi", "flask", "v1", &previous);
        seed_active(&second_meta, "pypi", "flask", "v1", &previous);
        let first_inflight = Inflight::default();
        let second_inflight = Inflight::default();
        let policy = Policy::default();

        let (first, second) = tokio::join!(
            sync_project_files(
                &first_client,
                &first_inflight,
                &first_meta,
                "pypi",
                &policy,
                "flask",
                first_client.base_url(),
            ),
            sync_project_files(
                &second_client,
                &second_inflight,
                &second_meta,
                "pypi",
                &policy,
                "flask",
                second_client.base_url(),
            ),
        );

        assert_eq!(first.unwrap(), ProjectSyncOutcome::NotModified { files: 1 });
        assert_eq!(second.unwrap(), ProjectSyncOutcome::Missing);
    }

    #[tokio::test]
    async fn test_sync_rejects_a_declared_oversize_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/simple/flask/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{}", JSON))
            .mount(&server)
            .await;
        let client = client_for(&server);
        let (_dir, meta) = store();
        let mut head = client.head_project("flask", None).await.unwrap();
        head.content_length = Some(MAX_PROJECT_BYTES + 1);

        let error = publish_project_response(&meta, "pypi", &Policy::default(), "flask", client.base_url(), head, 1)
            .await
            .unwrap_err();

        assert!(matches!(error, ProjectSyncError::TooLarge));
        assert!(active_project_generation(&meta, "pypi", "flask").unwrap().is_none());
    }

    #[tokio::test]
    async fn test_sync_reports_an_unreachable_upstream() {
        let client = UpstreamClient::new("http://127.0.0.1:1/simple/").unwrap();
        let (_dir, meta) = store();

        let error = sync_project_files(
            &client,
            &Inflight::default(),
            &meta,
            "pypi",
            &Policy::default(),
            "flask",
            client.base_url(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ProjectSyncError::Upstream(_)));
    }

    #[test]
    fn test_write_project_chunk_caps_unknown_length() {
        let mut output = Vec::new();
        let mut bytes = 0;
        write_project_chunk(&mut output, b"1234", &mut bytes, 7).unwrap();
        let error = write_project_chunk(&mut output, b"5678", &mut bytes, 7).unwrap_err();
        assert!(matches!(error, ProjectSyncError::TooLarge));
        assert_eq!(output, b"1234");
    }

    #[test]
    fn test_write_project_chunk_propagates_a_writer_error() {
        struct FailWriter;
        impl std::io::Write for FailWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("disk full"))
            }
        }
        let mut writer = FailWriter;
        let mut bytes = 0;
        let error = write_project_chunk(&mut writer, b"data", &mut bytes, 100).unwrap_err();
        assert!(matches!(error, ProjectSyncError::Io(_)));
        assert!(
            std::io::Write::flush(&mut writer).is_err(),
            "the doubled writer fails every operation"
        );
    }

    #[test]
    fn test_project_sync_error_messages_name_the_limit() {
        assert_eq!(
            ProjectSyncError::Status(500).to_string(),
            "upstream project detail returned 500"
        );
        assert!(ProjectSyncError::TooLarge.to_string().contains("byte limit"));
        assert!(ProjectSyncError::TooManyFiles.to_string().contains("file limit"));
        assert_eq!(ProjectSyncError::Io(std::io::Error::other("boom")).to_string(), "boom");
    }

    #[test]
    fn test_parse_project_rejects_too_many_files() {
        let (_dir, meta) = store();
        let (id, _) = begin_project_generation(&meta, "pypi", "flask").unwrap();
        let html = format!(
            r#"<a href="https://files.example/a.tar.gz#sha256={a}">a.tar.gz</a><a href="https://files.example/b.tar.gz#sha256={b}">b.tar.gz</a>"#,
            a = "a".repeat(64),
            b = "b".repeat(64),
        );

        let error = parse_project(
            &mut std::io::Cursor::new(html),
            "html",
            &url::Url::parse("https://files.example/simple/flask/").unwrap(),
            &meta,
            "pypi",
            &Policy::default(),
            "flask",
            id,
            "pypi",
            1,
        )
        .unwrap_err();

        assert!(matches!(error, ProjectSyncError::TooManyFiles));
    }

    #[test]
    fn test_parse_project_flushes_at_the_batch_limit() {
        let (_dir, meta) = store();
        let (id, _) = begin_project_generation(&meta, "pypi", "flask").unwrap();
        let files = (0..PROJECT_FILE_BATCH)
            .map(|index| format!(r#"{{"filename":"pkg-{index}.tar.gz","url":"pkg-{index}.tar.gz","hashes":{{"sha256":"{index:064}"}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let body = format!(r#"{{"meta":{{"api-version":"1.1"}},"files":[{files}]}}"#);

        let (admitted, _) = parse_project(
            &mut std::io::Cursor::new(body),
            "json",
            &url::Url::parse("https://files.example/simple/flask/").unwrap(),
            &meta,
            "pypi",
            &Policy::default(),
            "flask",
            id,
            "pypi",
            super::MAX_PROJECT_FILES,
        )
        .unwrap();

        assert_eq!(admitted, PROJECT_FILE_BATCH as u64);
    }
}
