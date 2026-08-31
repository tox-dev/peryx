use std::future::Future;
use std::io::Write;
use std::sync::Arc;

use crate::store::CachedIndex;
use crate::store::PypiStore as _;
use crate::{ProjectDetail, SimpleClientExt as _, SimpleResponse, parse_detail, parse_detail_html};
use anyhow::{Context as _, bail};
use futures_util::{StreamExt as _, stream};
use peryx_driver::rate_limit::UpstreamLimits;
use peryx_driver::{AppState, ServingState};
use peryx_storage::blob::Digest;
use tokio::sync::Semaphore;

use super::report::{unix_now, write_count, write_file_row, write_file_row_bytes, write_page_row, write_row};
use super::selection::{candidates, content_type_is_json, selection, target};
use super::{
    BlobCheck, FileCandidate, HEADER, PrefetchConfig, PrefetchCounts, PrefetchFile, PrefetchMetadata, PrefetchOptions,
    Row, Selection, SelectionSource, SyncOutcome, Target,
};

/// Upstream work a prefetch run overlaps when its index sets no ceiling of its own. Serving leaves an
/// index uncapped by default, and a run that started every selected transfer at once would hold the
/// whole selection in memory.
const DEFAULT_PREFETCH_CONCURRENCY: usize = 8;

/// Cached blobs a verify run rehashes at once. Verification never leaves the local store, so an
/// upstream ceiling says nothing about how much of it is safe to overlap.
const VERIFY_CONCURRENCY: usize = 8;

/// One task's share of the report: the rows it rendered and the counters it collected.
struct PrefetchReport {
    rows: Vec<u8>,
    counts: PrefetchCounts,
}

/// The upstream overlap a prefetch run may take: whatever the index already grants the serving path.
fn upstream_ceiling(limits: &UpstreamLimits, index: &str) -> usize {
    match limits.snapshots().into_iter().find(|snapshot| snapshot.index == index) {
        Some(snapshot) if snapshot.max_concurrent > 0 => snapshot.max_concurrent,
        _ => DEFAULT_PREFETCH_CONCURRENCY,
    }
}

/// Runs `work` under `gate`. The permit is released with `work`, so a task that fans out never holds
/// the gate while the tasks it fanned out to queue for one.
async fn gated<T>(gate: &Semaphore, work: impl Future<Output = T>) -> T {
    let _permit = gate
        .acquire()
        .await
        .expect("a prefetch gate stays open for the whole run");
    work.await
}

pub(super) async fn pypi_plan(
    configured: &PrefetchConfig,
    state: &Arc<AppState>,
    index: &str,
    options: &PrefetchOptions,
    out: &mut (dyn Write + Send + '_),
) -> anyhow::Result<()> {
    let state = state.serving.as_ref();
    let target = target(configured, state, index)?;
    let selection = selection(state, &target, options, SelectionSource::Upstream).await?;
    out.write_all(HEADER.as_bytes())?;
    let mut counts = PrefetchCounts::default();
    let mut planned = stream::iter(selection.projects.iter().cloned())
        .map(|project| plan_project(state, &target, project, &selection))
        .buffered(upstream_ceiling(&state.upstream_limits, &target.cached));
    while let Some(report) = planned.next().await {
        let report = report?;
        counts.merge(&report.counts);
        out.write_all(&report.rows)?;
    }
    write_count(out, &target.index, "projects", counts.projects)?;
    write_count(out, &target.index, "files", counts.files)?;
    write_count(out, &target.index, "skipped", counts.skipped)?;
    write_count(out, &target.index, "failures", counts.failures)?;
    if counts.failures > 0 {
        bail!("prefetch plan found {} failure(s)", counts.failures);
    }
    Ok(())
}

async fn plan_project(
    state: &ServingState,
    target: &Target,
    project: String,
    selection: &Selection,
) -> anyhow::Result<PrefetchReport> {
    let mut rows = Vec::new();
    let mut counts = PrefetchCounts {
        projects: 1,
        ..PrefetchCounts::default()
    };
    match plan_detail(state, target, &project).await {
        Ok(Some(detail)) => {
            write_row(&mut rows, Row::page(&target.index, &project, "selected", ""))?;
            for candidate in candidates(&detail, selection.rules.get(&project), &selection.filters) {
                match candidate {
                    FileCandidate::Include(file) => {
                        counts.files += 1;
                        write_file_row(&mut rows, &target.index, &project, &file, "selected", "")?;
                        if let Some(metadata) = &file.metadata {
                            let metadata_filename = format!("{}.metadata", file.filename);
                            let row = Row::metadata(
                                &target.index,
                                &project,
                                &metadata_filename,
                                metadata,
                                None,
                                "selected",
                                "",
                            );
                            write_row(&mut rows, row)?;
                        }
                    }
                    FileCandidate::Skip(file, reason) => {
                        counts.skipped += 1;
                        write_file_row(&mut rows, &target.index, &project, &file, "skipped", reason)?;
                    }
                }
            }
        }
        Ok(None) => {
            counts.skipped += 1;
            let row = Row::page(&target.index, &project, "skipped", "project not found");
            write_row(&mut rows, row)?;
        }
        Err(err) => {
            counts.failures += 1;
            let reason = err.to_string();
            let row = Row::page(&target.index, &project, "failure", &reason);
            write_row(&mut rows, row)?;
        }
    }
    Ok(PrefetchReport { rows, counts })
}

pub(super) async fn pypi_sync(
    configured: &PrefetchConfig,
    state: &Arc<AppState>,
    index: &str,
    options: &PrefetchOptions,
    out: &mut (dyn Write + Send + '_),
) -> anyhow::Result<()> {
    let started_at = unix_now();
    let state = state.serving.clone();
    let target = target(configured, &state, index)?;
    let selection = selection(&state, &target, options, SelectionSource::Upstream).await?;
    out.write_all(HEADER.as_bytes())?;
    write_count(out, &target.index, "started_at", started_at)?;
    let concurrency = upstream_ceiling(&state.upstream_limits, &target.cached);
    let run = SyncRun {
        state: &state,
        target: &target,
        selection: &selection,
        transfers: Semaphore::new(concurrency),
        concurrency,
    };
    let mut counts = PrefetchCounts::default();
    let mut synced = stream::iter(selection.projects.iter().cloned())
        .map(|project| run.project(project))
        .buffered(concurrency);
    while let Some(report) = synced.next().await {
        let report = report?;
        counts.merge(&report.counts);
        out.write_all(&report.rows)?;
    }
    write_count(out, &target.index, "finished_at", unix_now())?;
    write_count(out, &target.index, "packages_seen", counts.projects)?;
    write_count(out, &target.index, "files_downloaded", counts.downloaded)?;
    write_count(out, &target.index, "bytes_downloaded", counts.bytes)?;
    write_count(out, &target.index, "skipped_files", counts.skipped)?;
    write_count(out, &target.index, "failures", counts.failures)?;
    if counts.failures > 0 {
        bail!("prefetch sync found {} failure(s)", counts.failures);
    }
    Ok(())
}

/// A sync run's shared, read-only context plus the ceiling its tasks transfer under.
struct SyncRun<'a> {
    state: &'a Arc<ServingState>,
    target: &'a Target,
    selection: &'a Selection,
    /// One permit per transfer in flight, so nesting file tasks inside project tasks cannot multiply
    /// the ceiling.
    transfers: Semaphore,
    concurrency: usize,
}

impl SyncRun<'_> {
    async fn project(&self, project: String) -> anyhow::Result<PrefetchReport> {
        let mut rows = Vec::new();
        let mut counts = PrefetchCounts {
            projects: 1,
            ..PrefetchCounts::default()
        };
        let materialized = gated(
            &self.transfers,
            crate::cache::materialize_detail(Arc::clone(self.state), self.target.position, project.clone()),
        )
        .await;
        match materialized {
            Ok(Some(_)) => {
                let detail = cached_detail(self.state, self.target, &project)?;
                write_row(&mut rows, Row::page(&self.target.index, &project, "synced", ""))?;
                self.files(&project, &detail, &mut rows, &mut counts).await?;
            }
            Ok(None) => {
                counts.skipped += 1;
                let row = Row::page(&self.target.index, &project, "skipped", "project not found");
                write_row(&mut rows, row)?;
            }
            Err(err) => {
                counts.failures += 1;
                let reason = err.user_message();
                let row = Row::page(&self.target.index, &project, "failure", &reason);
                write_row(&mut rows, row)?;
            }
        }
        Ok(PrefetchReport { rows, counts })
    }

    async fn files(
        &self,
        project: &str,
        detail: &ProjectDetail,
        rows: &mut Vec<u8>,
        counts: &mut PrefetchCounts,
    ) -> anyhow::Result<()> {
        let selected = candidates(detail, self.selection.rules.get(project), &self.selection.filters);
        let mut transfers = stream::iter(selected)
            .map(|candidate| self.candidate(project, candidate))
            .buffered(self.concurrency);
        while let Some(report) = transfers.next().await {
            let report = report?;
            counts.merge(&report.counts);
            rows.extend_from_slice(&report.rows);
        }
        Ok(())
    }

    async fn candidate(&self, project: &str, candidate: FileCandidate) -> anyhow::Result<PrefetchReport> {
        let mut rows = Vec::new();
        let mut counts = PrefetchCounts::default();
        let file = match candidate {
            FileCandidate::Include(file) => file,
            FileCandidate::Skip(file, reason) => {
                counts.skipped += 1;
                write_file_row(&mut rows, &self.target.index, project, &file, "skipped", reason)?;
                return Ok(PrefetchReport { rows, counts });
            }
        };
        if let Some(metadata) = &file.metadata {
            self.metadata(project, &file, metadata, &mut rows, &mut counts).await?;
        }
        if self.selection.filters.metadata_only {
            counts.skipped += 1;
            let index = &self.target.index;
            write_file_row(&mut rows, index, project, &file, "skipped", "metadata-only")?;
            return Ok(PrefetchReport { rows, counts });
        }
        self.artifact(project, &file, &mut rows, &mut counts).await?;
        Ok(PrefetchReport { rows, counts })
    }

    async fn metadata(
        &self,
        project: &str,
        file: &PrefetchFile,
        metadata: &PrefetchMetadata,
        rows: &mut Vec<u8>,
        counts: &mut PrefetchCounts,
    ) -> anyhow::Result<()> {
        let filename = format!("{}.metadata", file.filename);
        let outcome = gated(
            &self.transfers,
            sync_metadata(self.state, self.target, &filename, &file.digest, &metadata.digest),
        )
        .await;
        let (bytes, status, reason) = match outcome {
            Ok(SyncOutcome::Cached(bytes)) => (Some(bytes), "cached", String::new()),
            Ok(SyncOutcome::Downloaded(bytes)) => {
                counts.downloaded += 1;
                counts.bytes += bytes;
                (Some(bytes), "downloaded", String::new())
            }
            Err(err) => {
                counts.failures += 1;
                (None, "failure", err.user_message())
            }
        };
        let row = Row::metadata(&self.target.index, project, &filename, metadata, bytes, status, &reason);
        write_row(rows, row)
    }

    async fn artifact(
        &self,
        project: &str,
        file: &PrefetchFile,
        rows: &mut Vec<u8>,
        counts: &mut PrefetchCounts,
    ) -> anyhow::Result<()> {
        match gated(&self.transfers, sync_file(Arc::clone(self.state), self.target, file)).await {
            Ok(SyncOutcome::Cached(bytes)) => {
                write_file_row_bytes(rows, &self.target.index, project, file, Some(bytes), "cached", "")
            }
            Ok(SyncOutcome::Downloaded(bytes)) => {
                counts.downloaded += 1;
                counts.bytes += bytes;
                write_file_row_bytes(rows, &self.target.index, project, file, Some(bytes), "downloaded", "")
            }
            Err(err) => {
                counts.failures += 1;
                write_file_row(rows, &self.target.index, project, file, "failure", &err.user_message())
            }
        }
    }
}

pub(super) async fn pypi_verify(
    configured: &PrefetchConfig,
    state: &Arc<AppState>,
    index: &str,
    options: &PrefetchOptions,
    out: &mut (dyn Write + Send + '_),
) -> anyhow::Result<()> {
    let state = state.serving.as_ref();
    let target = target(configured, state, index)?;
    let selection = selection(state, &target, options, SelectionSource::Cache).await?;
    out.write_all(HEADER.as_bytes())?;
    let checks = Semaphore::new(VERIFY_CONCURRENCY);
    let mut counts = PrefetchCounts::default();
    let mut verified = stream::iter(selection.projects.iter().cloned())
        .map(|project| verify_project(state, &target, project, &selection, &checks))
        .buffered(VERIFY_CONCURRENCY);
    while let Some(report) = verified.next().await {
        let report = report?;
        counts.merge(&report.counts);
        out.write_all(&report.rows)?;
    }
    write_count(out, &target.index, "problems", counts.problems)?;
    if counts.problems > 0 {
        bail!("prefetch verify found {} problem(s)", counts.problems);
    }
    Ok(())
}

async fn verify_project(
    state: &ServingState,
    target: &Target,
    project: String,
    selection: &Selection,
    checks: &Semaphore,
) -> anyhow::Result<PrefetchReport> {
    let mut rows = Vec::new();
    let mut counts = PrefetchCounts {
        projects: 1,
        ..PrefetchCounts::default()
    };
    let key = format!("{}/{}", target.cached, project);
    let Some(record) = state
        .meta
        .get_index(&key)
        .context(format!("read cached project {key}"))?
    else {
        counts.problems += 1;
        let index = &target.index;
        write_page_row(&mut rows, index, &project, "missing", "project page is not cached")?;
        return Ok(PrefetchReport { rows, counts });
    };
    let detail = match raw_detail(&project, &record) {
        Ok(detail) => detail,
        Err(err) => {
            counts.problems += 1;
            write_page_row(&mut rows, &target.index, &project, "failure", &err.to_string())?;
            return Ok(PrefetchReport { rows, counts });
        }
    };
    let included =
        candidates(&detail, selection.rules.get(&project), &selection.filters).filter_map(
            |candidate| match candidate {
                FileCandidate::Include(file) => Some(file),
                FileCandidate::Skip(..) => None,
            },
        );
    let mut checked = stream::iter(included)
        .map(|file| verify_file(state, target, &project, file, checks))
        .buffered(VERIFY_CONCURRENCY);
    while let Some(report) = checked.next().await {
        let report = report?;
        counts.merge(&report.counts);
        rows.extend_from_slice(&report.rows);
    }
    Ok(PrefetchReport { rows, counts })
}

async fn verify_file(
    state: &ServingState,
    target: &Target,
    project: &str,
    file: PrefetchFile,
    checks: &Semaphore,
) -> anyhow::Result<PrefetchReport> {
    let mut rows = Vec::new();
    let mut counts = PrefetchCounts::default();
    let check = BlobCheck {
        kind: "file",
        filename: &file.filename,
        digest_hex: &file.digest,
        url: &file.url,
    };
    counts.problems += verify_blob(&mut rows, state, target, project, check, checks).await?;
    if let Some(metadata) = &file.metadata {
        let metadata_filename = format!("{}.metadata", file.filename);
        let check = BlobCheck {
            kind: "metadata",
            filename: &metadata_filename,
            digest_hex: &metadata.digest,
            url: &metadata.url,
        };
        counts.problems += verify_blob(&mut rows, state, target, project, check, checks).await?;
    }
    Ok(PrefetchReport { rows, counts })
}

async fn sync_file(
    state: Arc<ServingState>,
    target: &Target,
    file: &PrefetchFile,
) -> Result<SyncOutcome, crate::cache::CacheError> {
    let digest = Digest::from_hex(&file.digest).ok_or(crate::cache::CacheError::FileNotFound)?;
    if let Some(metadata) = state.blobs.head(&digest).await? {
        return Ok(SyncOutcome::Cached(metadata.bytes));
    }
    let (_, bytes) =
        crate::cache::file_path_with_size(state, digest, target.route.clone(), file.filename.clone()).await?;
    Ok(SyncOutcome::Downloaded(bytes))
}

async fn sync_metadata(
    state: &Arc<ServingState>,
    target: &Target,
    metadata_filename: &str,
    artifact_digest: &str,
    metadata_digest: &str,
) -> Result<SyncOutcome, crate::cache::CacheError> {
    let artifact = Digest::from_hex(artifact_digest).ok_or(crate::cache::CacheError::FileNotFound)?;
    let metadata = Digest::from_hex(metadata_digest).ok_or(crate::cache::CacheError::FileNotFound)?;
    if let Some(metadata) = state.blobs.head(&metadata).await? {
        return Ok(SyncOutcome::Cached(metadata.bytes));
    }
    Ok(SyncOutcome::Downloaded(
        crate::cache::metadata_bytes(
            state,
            state.index_at(target.position),
            &artifact,
            &target.route,
            metadata_filename,
        )
        .await?
        .len() as u64,
    ))
}

async fn verify_blob(
    out: &mut (dyn Write + Send + '_),
    state: &ServingState,
    target: &Target,
    project: &str,
    check: BlobCheck<'_>,
    checks: &Semaphore,
) -> anyhow::Result<u64> {
    let Some(digest) = Digest::from_hex(check.digest_hex) else {
        let row = Row::check(
            &target.index,
            project,
            check,
            check.digest_hex,
            "failure",
            "invalid sha256 digest",
        );
        write_row(out, row)?;
        return Ok(1);
    };
    match gated(checks, state.blobs.verify(&digest)).await {
        Ok(true) => Ok(0),
        Ok(false) => {
            let row = Row::check(
                &target.index,
                project,
                check,
                digest.as_str(),
                "failure",
                "digest mismatch",
            );
            write_row(out, row)?;
            Ok(1)
        }
        Err(err) if err.kind() == peryx_storage::blob::BlobErrorKind::NotFound => {
            let row = Row::check(
                &target.index,
                project,
                check,
                digest.as_str(),
                "missing",
                "blob is not cached",
            );
            write_row(out, row)?;
            Ok(1)
        }
        Err(err) => {
            let reason = err.to_string();
            let row = Row::check(&target.index, project, check, digest.as_str(), "failure", &reason);
            write_row(out, row)?;
            Ok(1)
        }
    }
}

async fn plan_detail(state: &ServingState, target: &Target, project: &str) -> anyhow::Result<Option<ProjectDetail>> {
    if target.offline {
        let key = format!("{}/{}", target.cached, project);
        return state
            .meta
            .get_index(&key)?
            .map_or_else(|| Ok(None), |record| raw_detail(project, &record).map(Some));
    }
    let router = state
        .upstream_routes
        .get(&target.cached)
        .expect("a cached index always has an upstream route");
    let response = router.fetch_project(project, None).await?;
    match response.status {
        200 => parse_response_detail(project, &response).map(Some),
        404 => Ok(None),
        status => bail!("upstream returned {status}"),
    }
}

fn parse_response_detail(project: &str, response: &SimpleResponse) -> anyhow::Result<ProjectDetail> {
    let parsed = if content_type_is_json(response.content_type.as_deref()) {
        parse_detail(&response.body)?
    } else {
        parse_detail_html(project, &String::from_utf8_lossy(&response.body), &response.url)?
    };
    Ok(ProjectDetail {
        meta: parsed.meta,
        name: parsed.name,
        versions: parsed.versions,
        files: parsed.files,
    })
}

fn raw_detail(project: &str, record: &CachedIndex) -> anyhow::Result<ProjectDetail> {
    let parsed = parse_detail(&record.body).context(format!("parse cached project {project}"))?;
    Ok(ProjectDetail {
        meta: parsed.meta,
        name: parsed.name,
        versions: parsed.versions,
        files: parsed.files,
    })
}

fn cached_detail(state: &ServingState, target: &Target, project: &str) -> anyhow::Result<ProjectDetail> {
    let key = format!("{}/{}", target.cached, project);
    let record = state
        .meta
        .get_index(&key)
        .context(format!("read cached project {key}"))?
        .context(format!("project {project:?} was not cached after sync"))?;
    raw_detail(project, &record)
}

#[cfg(test)]
#[path = "../../tests/unit/mirror/run_tests.rs"]
mod tests;
