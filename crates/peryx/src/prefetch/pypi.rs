//! `PyPI` plan, sync, and verify: the read-through mirror flows over the Simple index.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use peryx_driver::AppState;
use peryx_ecosystem_pypi::store::CachedIndex;
use peryx_ecosystem_pypi::store::PypiStore as _;
use peryx_ecosystem_pypi::{ProjectDetail, parse_detail, parse_detail_html};
use peryx_storage::blob::Digest;
use peryx_upstream::SimpleResponse;

use super::report::{
    blob_size, unix_now, write_count, write_file_row, write_file_row_bytes, write_page_row, write_row,
};
use super::selection::{candidates, content_type_is_json, selection, target};
use super::{
    BlobCheck, FileCandidate, HEADER, Output, PrefetchFile, Row, Selection, SelectionSource, SyncOutcome, SyncSummary,
    Target,
};
use crate::cli::PrefetchOptions;
use crate::config::Config;

pub(super) async fn pypi_plan(
    config: &Config,
    state: &Arc<AppState>,
    options: &PrefetchOptions,
    out: &mut Output,
) -> anyhow::Result<()> {
    let target = target(config, state, &options.index)?;
    let selection = selection(state, &target, options, SelectionSource::Upstream).await?;
    out.write_all(HEADER.as_bytes())?;
    let mut projects = 0_u64;
    let mut files = 0_u64;
    let mut skipped = 0_u64;
    let mut failures = 0_u64;
    for project in &selection.projects {
        projects += 1;
        match plan_detail(state, &target, project).await {
            Ok(Some(detail)) => {
                write_row(out, Row::page(&target.index, project, "selected", ""))?;
                for candidate in candidates(&detail, selection.rules.get(project), &selection.filters) {
                    match candidate {
                        FileCandidate::Include(file) => {
                            files += 1;
                            write_file_row(out, &target.index, project, &file, "selected", "")?;
                            if let Some(metadata) = &file.metadata {
                                let metadata_filename = format!("{}.metadata", file.filename);
                                let row = Row::metadata(
                                    &target.index,
                                    project,
                                    &metadata_filename,
                                    metadata,
                                    None,
                                    "selected",
                                    "",
                                );
                                write_row(out, row)?;
                            }
                        }
                        FileCandidate::Skip(file, reason) => {
                            skipped += 1;
                            write_file_row(out, &target.index, project, &file, "skipped", reason)?;
                        }
                    }
                }
            }
            Ok(None) => {
                skipped += 1;
                write_row(out, Row::page(&target.index, project, "skipped", "project not found"))?;
            }
            Err(err) => {
                failures += 1;
                write_row(out, Row::page(&target.index, project, "failure", &err.to_string()))?;
            }
        }
    }
    write_count(out, &target.index, "projects", projects)?;
    write_count(out, &target.index, "files", files)?;
    write_count(out, &target.index, "skipped", skipped)?;
    write_count(out, &target.index, "failures", failures)?;
    if failures > 0 {
        bail!("prefetch plan found {failures} failure(s)");
    }
    Ok(())
}

pub(super) async fn pypi_sync(
    config: &Config,
    state: &Arc<AppState>,
    options: &PrefetchOptions,
    out: &mut Output,
) -> anyhow::Result<()> {
    let started_at = unix_now();
    let target = target(config, state, &options.index)?;
    let selection = selection(state, &target, options, SelectionSource::Upstream).await?;
    out.write_all(HEADER.as_bytes())?;
    let mut summary = SyncSummary::default();
    write_count(out, &target.index, "started_at", started_at)?;
    for project in &selection.projects {
        summary.projects += 1;
        match peryx_ecosystem_pypi::cache::materialize_detail(state.serving.clone(), target.position, project.clone())
            .await
        {
            Ok(Some(_)) => {
                let detail = cached_detail(state, &target, project)?;
                write_row(out, Row::page(&target.index, project, "synced", ""))?;
                sync_files(out, state, &target, project, &detail, &selection, &mut summary).await?;
            }
            Ok(None) => {
                summary.skipped += 1;
                write_row(out, Row::page(&target.index, project, "skipped", "project not found"))?;
            }
            Err(err) => {
                summary.failures += 1;
                write_row(out, Row::page(&target.index, project, "failure", &err.user_message()))?;
            }
        }
    }
    write_count(out, &target.index, "finished_at", unix_now())?;
    write_count(out, &target.index, "packages_seen", summary.projects)?;
    write_count(out, &target.index, "files_downloaded", summary.downloaded)?;
    write_count(out, &target.index, "bytes_downloaded", summary.bytes)?;
    write_count(out, &target.index, "skipped_files", summary.skipped)?;
    write_count(out, &target.index, "failures", summary.failures)?;
    if summary.failures > 0 {
        bail!("prefetch sync found {} failure(s)", summary.failures);
    }
    Ok(())
}

pub(super) async fn pypi_verify(
    config: &Config,
    state: &Arc<AppState>,
    options: &PrefetchOptions,
    out: &mut Output,
) -> anyhow::Result<()> {
    let target = target(config, state, &options.index)?;
    let selection = selection(state, &target, options, SelectionSource::Cache).await?;
    out.write_all(HEADER.as_bytes())?;
    let mut problems = 0_u64;
    for project in &selection.projects {
        let key = format!("{}/{}", target.cached, project);
        let Some(record) = state
            .meta
            .get_index(&key)
            .context(format!("read cached project {key}"))?
        else {
            problems += 1;
            write_page_row(out, &target.index, project, "missing", "project page is not cached")?;
            continue;
        };
        let detail = match raw_detail(project, &record) {
            Ok(detail) => detail,
            Err(err) => {
                problems += 1;
                write_page_row(out, &target.index, project, "failure", &err.to_string())?;
                continue;
            }
        };
        for candidate in candidates(&detail, selection.rules.get(project), &selection.filters) {
            let FileCandidate::Include(file) = candidate else {
                continue;
            };
            let check = BlobCheck {
                kind: "file",
                filename: &file.filename,
                digest_hex: &file.digest,
                url: &file.url,
            };
            problems += verify_blob(out, state, &target, project, check)?;
            if let Some(metadata) = &file.metadata {
                let metadata_filename = format!("{}.metadata", file.filename);
                let check = BlobCheck {
                    kind: "metadata",
                    filename: &metadata_filename,
                    digest_hex: &metadata.digest,
                    url: &metadata.url,
                };
                problems += verify_blob(out, state, &target, project, check)?;
            }
        }
    }
    write_count(out, &target.index, "problems", problems)?;
    if problems > 0 {
        bail!("prefetch verify found {problems} problem(s)");
    }
    Ok(())
}

async fn sync_files(
    out: &mut Output,
    state: &Arc<AppState>,
    target: &Target,
    project: &str,
    detail: &ProjectDetail,
    selection: &Selection,
    summary: &mut SyncSummary,
) -> anyhow::Result<()> {
    for candidate in candidates(detail, selection.rules.get(project), &selection.filters) {
        let file = match candidate {
            FileCandidate::Include(file) => file,
            FileCandidate::Skip(file, reason) => {
                summary.skipped += 1;
                write_file_row(out, &target.index, project, &file, "skipped", reason)?;
                continue;
            }
        };
        if let Some(metadata) = &file.metadata {
            let metadata_filename = format!("{}.metadata", file.filename);
            match sync_metadata(state, &target.route, &metadata_filename, &file.digest, &metadata.digest).await {
                Ok(SyncOutcome::Cached(bytes)) => {
                    let row = Row::metadata(
                        &target.index,
                        project,
                        &metadata_filename,
                        metadata,
                        Some(bytes),
                        "cached",
                        "",
                    );
                    write_row(out, row)?;
                }
                Ok(SyncOutcome::Downloaded(bytes)) => {
                    summary.downloaded += 1;
                    summary.bytes += bytes;
                    let row = Row::metadata(
                        &target.index,
                        project,
                        &metadata_filename,
                        metadata,
                        Some(bytes),
                        "downloaded",
                        "",
                    );
                    write_row(out, row)?;
                }
                Err(err) => {
                    summary.failures += 1;
                    let reason = err.user_message();
                    let row = Row::metadata(
                        &target.index,
                        project,
                        &metadata_filename,
                        metadata,
                        None,
                        "failure",
                        &reason,
                    );
                    write_row(out, row)?;
                }
            }
        }
        if selection.filters.metadata_only {
            summary.skipped += 1;
            write_file_row(out, &target.index, project, &file, "skipped", "metadata-only")?;
            continue;
        }
        match sync_file(state.clone(), target, &file).await {
            Ok(SyncOutcome::Cached(bytes)) => {
                write_file_row_bytes(out, &target.index, project, &file, Some(bytes), "cached", "")?;
            }
            Ok(SyncOutcome::Downloaded(bytes)) => {
                summary.downloaded += 1;
                summary.bytes += bytes;
                write_file_row_bytes(out, &target.index, project, &file, Some(bytes), "downloaded", "")?;
            }
            Err(err) => {
                summary.failures += 1;
                write_file_row(out, &target.index, project, &file, "failure", &err.user_message())?;
            }
        }
    }
    Ok(())
}

async fn sync_file(
    state: Arc<AppState>,
    target: &Target,
    file: &PrefetchFile,
) -> Result<SyncOutcome, peryx_ecosystem_pypi::cache::CacheError> {
    let digest = Digest::from_hex(&file.digest).ok_or(peryx_ecosystem_pypi::cache::CacheError::FileNotFound)?;
    if state.blobs.exists(&digest) {
        return Ok(SyncOutcome::Cached(blob_size(&state, &digest)));
    }
    let path = peryx_ecosystem_pypi::cache::file_path(
        state.serving.clone(),
        digest.clone(),
        target.route.clone(),
        file.filename.clone(),
    )
    .await?;
    Ok(SyncOutcome::Downloaded(
        path.metadata().map(|metadata| metadata.len()).unwrap_or_default(),
    ))
}

async fn sync_metadata(
    state: &Arc<AppState>,
    route: &str,
    metadata_filename: &str,
    artifact_digest: &str,
    metadata_digest: &str,
) -> Result<SyncOutcome, peryx_ecosystem_pypi::cache::CacheError> {
    let artifact = Digest::from_hex(artifact_digest).ok_or(peryx_ecosystem_pypi::cache::CacheError::FileNotFound)?;
    let metadata = Digest::from_hex(metadata_digest).ok_or(peryx_ecosystem_pypi::cache::CacheError::FileNotFound)?;
    if state.blobs.exists(&metadata) {
        return Ok(SyncOutcome::Cached(blob_size(state, &metadata)));
    }
    Ok(SyncOutcome::Downloaded(
        peryx_ecosystem_pypi::cache::metadata_bytes(&state.serving, &artifact, route, metadata_filename)
            .await?
            .len() as u64,
    ))
}

fn verify_blob(
    out: &mut Output,
    state: &Arc<AppState>,
    target: &Target,
    project: &str,
    check: BlobCheck<'_>,
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
    if !state.blobs.exists(&digest) {
        let row = Row::check(
            &target.index,
            project,
            check,
            digest.as_str(),
            "missing",
            "blob is not cached",
        );
        write_row(out, row)?;
        return Ok(1);
    }
    match state.blobs.verify(&digest) {
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
        Err(err) => {
            let reason = err.to_string();
            let row = Row::check(&target.index, project, check, digest.as_str(), "failure", &reason);
            write_row(out, row)?;
            Ok(1)
        }
    }
}

async fn plan_detail(state: &Arc<AppState>, target: &Target, project: &str) -> anyhow::Result<Option<ProjectDetail>> {
    if target.offline {
        let key = format!("{}/{}", target.cached, project);
        return state
            .meta
            .get_index(&key)?
            .map(|record| raw_detail(project, &record))
            .transpose();
    }
    let response = target.client.fetch_project(project, None).await?;
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

fn cached_detail(state: &Arc<AppState>, target: &Target, project: &str) -> anyhow::Result<ProjectDetail> {
    let key = format!("{}/{}", target.cached, project);
    let record = state
        .meta
        .get_index(&key)
        .context(format!("read cached project {key}"))?
        .context(format!("project {project:?} was not cached after sync"))?;
    raw_detail(project, &record)
}
