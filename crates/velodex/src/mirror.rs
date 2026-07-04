//! Mirror planning, synchronization, and verification.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use velodex_format::pypi::{
    CoreMetadata, DistributionFilename, DistributionKind, File, ProjectDetail, Version, VersionSpecifiers,
    is_valid_name, normalize_name, parse_detail, parse_detail_html, parse_distribution_filename, parse_index,
    parse_index_html, parse_version_specifiers,
};
use velodex_http::{AppState, Index, IndexKind};
use velodex_storage::blob::Digest;
use velodex_storage::meta::CachedIndex;
use velodex_upstream::{SimpleResponse, UpstreamClient};

use crate::cli::{MirrorCommand, MirrorOptions};
use crate::config::{Config, IndexKind as ConfigIndexKind, MirrorPrefetchConfig, MirrorPrefetchMode};
use crate::server;

const HEADER: &str = "kind\trepository\tproject\tfilename\tdigest\turl\tbytes\tstatus\treason\n";

type Output = dyn Write + Send;

/// Run a `velodex mirror` subcommand.
///
/// # Errors
/// Returns an error when configuration is invalid, upstream access fails, selected cache entries
/// fail verification, or output cannot be written.
pub async fn run(config: &Config, command: &MirrorCommand, out: &mut Output) -> anyhow::Result<()> {
    match command {
        MirrorCommand::Plan(args) => plan(config, &args.options, out).await,
        MirrorCommand::Sync(args) => sync(config, &args.options, out).await,
        MirrorCommand::Verify(args) => verify(config, &args.options, out).await,
    }
}

async fn plan(config: &Config, options: &MirrorOptions, out: &mut Output) -> anyhow::Result<()> {
    let state = server::build_state(config)?;
    let target = target(config, &state, &options.repo)?;
    let selection = selection(&state, &target, options, SelectionSource::Upstream).await?;
    out.write_all(HEADER.as_bytes())?;
    let mut projects = 0_u64;
    let mut files = 0_u64;
    let mut skipped = 0_u64;
    let mut failures = 0_u64;
    for project in &selection.projects {
        projects += 1;
        match plan_detail(&state, &target, project).await {
            Ok(Some(detail)) => {
                write_row(out, Row::page(&target.repo, project, "selected", ""))?;
                for candidate in candidates(&detail, selection.rules.get(project), &selection.filters) {
                    match candidate {
                        FileCandidate::Include(file) => {
                            files += 1;
                            write_file_row(out, &target.repo, project, &file, "selected", "")?;
                            if let Some(metadata) = &file.metadata {
                                let metadata_filename = format!("{}.metadata", file.filename);
                                let row = Row::metadata(
                                    &target.repo,
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
                            write_file_row(out, &target.repo, project, &file, "skipped", reason)?;
                        }
                    }
                }
            }
            Ok(None) => {
                skipped += 1;
                write_row(out, Row::page(&target.repo, project, "skipped", "project not found"))?;
            }
            Err(err) => {
                failures += 1;
                write_row(out, Row::page(&target.repo, project, "failure", &err.to_string()))?;
            }
        }
    }
    write_count(out, &target.repo, "projects", projects)?;
    write_count(out, &target.repo, "files", files)?;
    write_count(out, &target.repo, "skipped", skipped)?;
    write_count(out, &target.repo, "failures", failures)?;
    if failures > 0 {
        bail!("mirror plan found {failures} failure(s)");
    }
    Ok(())
}

async fn sync(config: &Config, options: &MirrorOptions, out: &mut Output) -> anyhow::Result<()> {
    let started_at = unix_now();
    let state = server::build_state(config)?;
    let target = target(config, &state, &options.repo)?;
    let selection = selection(&state, &target, options, SelectionSource::Upstream).await?;
    out.write_all(HEADER.as_bytes())?;
    let mut summary = SyncSummary::default();
    write_count(out, &target.repo, "started_at", started_at)?;
    for project in &selection.projects {
        summary.projects += 1;
        match velodex_http::cache::materialize_detail(state.clone(), target.position, project.clone()).await {
            Ok(Some(_)) => {
                let detail = cached_detail(&state, &target, project)?;
                write_row(out, Row::page(&target.repo, project, "synced", ""))?;
                sync_files(out, &state, &target, project, &detail, &selection, &mut summary).await?;
            }
            Ok(None) => {
                summary.skipped += 1;
                write_row(out, Row::page(&target.repo, project, "skipped", "project not found"))?;
            }
            Err(err) => {
                summary.failures += 1;
                write_row(out, Row::page(&target.repo, project, "failure", &err.user_message()))?;
            }
        }
    }
    write_count(out, &target.repo, "finished_at", unix_now())?;
    write_count(out, &target.repo, "packages_seen", summary.projects)?;
    write_count(out, &target.repo, "files_downloaded", summary.downloaded)?;
    write_count(out, &target.repo, "bytes_downloaded", summary.bytes)?;
    write_count(out, &target.repo, "skipped_files", summary.skipped)?;
    write_count(out, &target.repo, "failures", summary.failures)?;
    if summary.failures > 0 {
        bail!("mirror sync found {} failure(s)", summary.failures);
    }
    Ok(())
}

async fn verify(config: &Config, options: &MirrorOptions, out: &mut Output) -> anyhow::Result<()> {
    let state = server::build_state(config)?;
    let target = target(config, &state, &options.repo)?;
    let selection = selection(&state, &target, options, SelectionSource::Cache).await?;
    out.write_all(HEADER.as_bytes())?;
    let mut problems = 0_u64;
    for project in &selection.projects {
        let key = format!("{}/{}", target.mirror, project);
        let Some(record) = state
            .meta
            .get_index(&key)
            .context(format!("read cached project {key}"))?
        else {
            problems += 1;
            write_page_row(out, &target.repo, project, "missing", "project page is not cached")?;
            continue;
        };
        let detail = match raw_detail(project, &record) {
            Ok(detail) => detail,
            Err(err) => {
                problems += 1;
                write_page_row(out, &target.repo, project, "failure", &err.to_string())?;
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
            problems += verify_blob(out, &state, &target, project, check)?;
            if let Some(metadata) = &file.metadata {
                let metadata_filename = format!("{}.metadata", file.filename);
                let check = BlobCheck {
                    kind: "metadata",
                    filename: &metadata_filename,
                    digest_hex: &metadata.digest,
                    url: &metadata.url,
                };
                problems += verify_blob(out, &state, &target, project, check)?;
            }
        }
    }
    write_count(out, &target.repo, "problems", problems)?;
    if problems > 0 {
        bail!("mirror verify found {problems} problem(s)");
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
                write_file_row(out, &target.repo, project, &file, "skipped", reason)?;
                continue;
            }
        };
        if let Some(metadata) = &file.metadata {
            let metadata_filename = format!("{}.metadata", file.filename);
            match sync_metadata(state, &target.route, &metadata_filename, &file.digest, &metadata.digest).await {
                Ok(SyncOutcome::Cached(bytes)) => {
                    let row = Row::metadata(
                        &target.repo,
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
                        &target.repo,
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
                        &target.repo,
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
            write_file_row(out, &target.repo, project, &file, "skipped", "metadata-only")?;
            continue;
        }
        match sync_file(state.clone(), target, &file).await {
            Ok(SyncOutcome::Cached(bytes)) => {
                write_file_row_bytes(out, &target.repo, project, &file, Some(bytes), "cached", "")?;
            }
            Ok(SyncOutcome::Downloaded(bytes)) => {
                summary.downloaded += 1;
                summary.bytes += bytes;
                write_file_row_bytes(out, &target.repo, project, &file, Some(bytes), "downloaded", "")?;
            }
            Err(err) => {
                summary.failures += 1;
                write_file_row(out, &target.repo, project, &file, "failure", &err.user_message())?;
            }
        }
    }
    Ok(())
}

async fn sync_file(
    state: Arc<AppState>,
    target: &Target,
    file: &MirrorFile,
) -> Result<SyncOutcome, velodex_http::cache::CacheError> {
    let digest = Digest::from_hex(&file.digest).ok_or(velodex_http::cache::CacheError::FileNotFound)?;
    if state.blobs.exists(&digest) {
        return Ok(SyncOutcome::Cached(blob_size(&state, &digest)));
    }
    let path = velodex_http::cache::file_path(
        state.clone(),
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
) -> Result<SyncOutcome, velodex_http::cache::CacheError> {
    let artifact = Digest::from_hex(artifact_digest).ok_or(velodex_http::cache::CacheError::FileNotFound)?;
    let metadata = Digest::from_hex(metadata_digest).ok_or(velodex_http::cache::CacheError::FileNotFound)?;
    if state.blobs.exists(&metadata) {
        return Ok(SyncOutcome::Cached(blob_size(state, &metadata)));
    }
    Ok(SyncOutcome::Downloaded(
        velodex_http::cache::metadata_bytes(state, &artifact, route, metadata_filename)
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
            &target.repo,
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
            &target.repo,
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
                &target.repo,
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
            let row = Row::check(&target.repo, project, check, digest.as_str(), "failure", &reason);
            write_row(out, row)?;
            Ok(1)
        }
    }
}

async fn plan_detail(state: &Arc<AppState>, target: &Target, project: &str) -> anyhow::Result<Option<ProjectDetail>> {
    if target.offline {
        let key = format!("{}/{}", target.mirror, project);
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
    let key = format!("{}/{}", target.mirror, project);
    let record = state
        .meta
        .get_index(&key)
        .context(format!("read cached project {key}"))?
        .context(format!("project {project:?} was not cached after sync"))?;
    raw_detail(project, &record)
}

async fn selection(
    state: &Arc<AppState>,
    target: &Target,
    options: &MirrorOptions,
    source: SelectionSource,
) -> anyhow::Result<Selection> {
    let mut filters = target.prefetch.clone();
    if let Some(mode) = options.mode {
        filters.mode = mode;
        if matches!(mode, MirrorPrefetchMode::MetadataOnly) {
            filters.metadata_only = true;
        }
    }
    filters.packages.extend(options.packages.clone());
    filters.requirements.extend(options.requirements.clone());
    filters.metadata_only |= options.metadata_only;
    if options.no_wheels {
        filters.include_wheels = false;
    }
    if options.no_sdists {
        filters.include_sdists = false;
    }
    filters.python_tags.extend(options.python_tags.clone());
    filters.abi_tags.extend(options.abi_tags.clone());
    filters.platform_tags.extend(options.platform_tags.clone());
    if let Some(max) = options.max_file_size_bytes {
        filters.max_file_size_bytes = Some(max);
    }

    let mut rules = BTreeMap::<String, ProjectRule>::new();
    for selector in &filters.packages {
        insert_selector(&mut rules, selector).context(format!("parse package selector {selector:?}"))?;
    }
    for selector in requirement_selectors(&filters.requirements)? {
        insert_selector(&mut rules, &selector).context(format!("parse requirement {selector:?}"))?;
    }
    let projects = match filters.mode {
        MirrorPrefetchMode::All => all_projects(state, target, source).await?,
        MirrorPrefetchMode::Selected | MirrorPrefetchMode::MetadataOnly => {
            if rules.is_empty() {
                bail!(
                    "mirror {} has no selected packages; add [index.prefetch].packages or --package",
                    target.repo
                );
            }
            rules.keys().cloned().collect()
        }
    };
    Ok(Selection {
        projects,
        rules,
        filters: ArtifactFilters::from(filters),
    })
}

async fn all_projects(state: &Arc<AppState>, target: &Target, source: SelectionSource) -> anyhow::Result<Vec<String>> {
    if matches!(source, SelectionSource::Cache) || target.offline {
        return Ok(state
            .meta
            .list_projects(&target.mirror)?
            .into_iter()
            .map(|name| normalize_name(&name))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect());
    }
    let response = target.client.fetch_index().await?;
    if response.status != 200 {
        bail!("upstream project list returned {}", response.status);
    }
    let list = if content_type_is_json(response.content_type.as_deref()) {
        parse_index(&response.body)?
    } else {
        parse_index_html(&String::from_utf8_lossy(&response.body), &response.url)?
    };
    Ok(list
        .projects
        .into_iter()
        .map(|entry| normalize_name(&entry.name))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

fn insert_selector(rules: &mut BTreeMap<String, ProjectRule>, raw: &str) -> anyhow::Result<()> {
    let selector = parse_selector(raw)?;
    rules.entry(selector.project).or_default().specs.push(selector.spec);
    Ok(())
}

fn parse_selector(raw: &str) -> anyhow::Result<ProjectSelector> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty selector");
    }
    if raw.contains('@') {
        bail!("direct-reference requirements are not supported");
    }
    let name_end = raw
        .find(|ch: char| matches!(ch, '<' | '>' | '=' | '!' | '~' | '[' | ';') || ch.is_whitespace())
        .unwrap_or(raw.len());
    let name = raw[..name_end].trim();
    if !is_valid_name(name) {
        bail!("invalid package name {name:?}");
    }
    let spec_text_fallback = raw[name_end..].trim();
    let spec_text = raw[name_end..]
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']').map(|(_, after)| after.trim()))
        .unwrap_or(spec_text_fallback);
    let spec_text = spec_text.split_once(';').map_or(spec_text, |(spec, _)| spec).trim();
    let spec = if spec_text.is_empty() {
        None
    } else {
        Some(parse_version_specifiers(spec_text).context(format!("invalid version specifier {spec_text:?}"))?)
    };
    Ok(ProjectSelector {
        project: normalize_name(name),
        spec,
    })
}

fn requirement_selectors(paths: &[PathBuf]) -> anyhow::Result<Vec<String>> {
    let mut selectors = Vec::new();
    let mut seen = BTreeSet::new();
    for path in paths {
        read_requirements(path, &mut selectors, &mut seen)?;
    }
    Ok(selectors)
}

fn read_requirements(path: &Path, selectors: &mut Vec<String>, seen: &mut BTreeSet<PathBuf>) -> anyhow::Result<()> {
    let path = path.to_path_buf();
    if !seen.insert(path.clone()) {
        return Ok(());
    }
    let text = std::fs::read_to_string(&path).context(format!("read requirements {}", path.display()))?;
    for line in text.lines() {
        let line = requirement_line(line);
        if line.is_empty() {
            continue;
        }
        if let Some(nested) = line
            .strip_prefix("-r ")
            .or_else(|| line.strip_prefix("--requirement "))
            .or_else(|| line.strip_prefix("-c "))
            .or_else(|| line.strip_prefix("--constraint "))
        {
            let fallback_parent = Path::new(".");
            let nested = path.parent().unwrap_or(fallback_parent).join(nested.trim());
            read_requirements(&nested, selectors, seen)?;
        } else if !line.starts_with('-') {
            selectors.push(line.to_owned());
        }
    }
    Ok(())
}

fn requirement_line(line: &str) -> &str {
    let line = line.trim();
    if line.starts_with('#') {
        return "";
    }
    let line = line.split_once(" #").map_or(line, |(requirement, _)| requirement);
    line.split_once(" --")
        .map_or(line, |(requirement, _)| requirement)
        .trim()
}

fn candidates<'a>(
    detail: &'a ProjectDetail,
    rule: Option<&'a ProjectRule>,
    filters: &'a ArtifactFilters,
) -> impl Iterator<Item = FileCandidate> + 'a {
    detail.files.iter().map(move |file| {
        let file = mirror_file(file);
        if file.digest.is_empty() {
            return FileCandidate::Skip(file, "missing sha256");
        }
        match decision(&file, rule, filters) {
            Ok(()) => FileCandidate::Include(file),
            Err(reason) => FileCandidate::Skip(file, reason),
        }
    })
}

fn mirror_file(file: &File) -> MirrorFile {
    let digest = file.hashes.get("sha256").cloned();
    let metadata = metadata_sibling(file);
    let source = parse_distribution_filename(&file.filename).ok();
    MirrorFile {
        filename: file.filename.clone(),
        digest: digest.unwrap_or_default(),
        url: file.url.clone(),
        size: file.size,
        metadata,
        source,
    }
}

fn metadata_sibling(file: &File) -> Option<MirrorMetadata> {
    let CoreMetadata::Hashes(hashes) = file.metadata() else {
        return None;
    };
    Some(MirrorMetadata {
        url: format!("{}.metadata", file.url),
        digest: hashes.get("sha256")?.clone(),
    })
}

fn decision(file: &MirrorFile, rule: Option<&ProjectRule>, filters: &ArtifactFilters) -> Result<(), &'static str> {
    let Some(source) = file.source.as_ref() else {
        return Err("unsupported filename");
    };
    match source.kind {
        DistributionKind::Wheel => {
            if !filters.include_wheels {
                return Err("wheels disabled");
            }
            if !wheel_tags_allowed(&file.filename, filters) {
                return Err("wheel tag filtered");
            }
        }
        DistributionKind::SdistTarGz => {
            if !filters.include_sdists {
                return Err("sdists disabled");
            }
        }
    }
    if let Some(max) = filters.max_file_size_bytes
        && file.size.is_some_and(|size| size > max)
    {
        return Err("size filtered");
    }
    if let Some(rule) = rule
        && !rule.allows(&source.version)
    {
        return Err("version filtered");
    }
    Ok(())
}

fn wheel_tags_allowed(filename: &str, filters: &ArtifactFilters) -> bool {
    if filters.python_tags.is_empty() && filters.abi_tags.is_empty() && filters.platform_tags.is_empty() {
        return true;
    }
    let stem = &filename[..filename.len() - ".whl".len()];
    let mut parts = stem.rsplit('-');
    let platform = parts.next().expect("validated wheel filename includes a platform tag");
    let abi = parts.next().expect("validated wheel filename includes an ABI tag");
    let python = parts.next().expect("validated wheel filename includes a Python tag");
    tags_allowed(python, &filters.python_tags)
        && tags_allowed(abi, &filters.abi_tags)
        && tags_allowed(platform, &filters.platform_tags)
}

fn tags_allowed(value: &str, filters: &BTreeSet<String>) -> bool {
    filters.is_empty() || value.split('.').any(|tag| filters.contains(tag))
}

fn target(config: &Config, state: &Arc<AppState>, repo: &str) -> anyhow::Result<Target> {
    let position = state
        .indexes
        .iter()
        .position(|index| index.name == repo || index.route == repo)
        .context(format!("unknown mirror repository {repo:?}"))?;
    let index = state.index_at(position);
    let (mirror, client, offline) = target_mirror(state, index)?;
    let prefetch = mirror_prefetch(config, &mirror)?.clone();
    Ok(Target {
        repo: repo.to_owned(),
        route: index.route.clone(),
        position,
        mirror,
        client,
        offline,
        prefetch,
    })
}

fn target_mirror(state: &AppState, index: &Index) -> anyhow::Result<(String, UpstreamClient, bool)> {
    match &index.kind {
        IndexKind::Mirror { client, offline } => Ok((index.name.clone(), client.clone(), *offline)),
        IndexKind::Local { .. } => bail!("repository {:?} is local and has no upstream mirror", index.name),
        IndexKind::Overlay { layers, .. } => {
            let mut mirror = None;
            for &pos in layers {
                let layer = state.index_at(pos);
                if let IndexKind::Mirror { client, offline } = &layer.kind
                    && mirror.replace((layer.name.clone(), client.clone(), *offline)).is_some()
                {
                    bail!("repository {:?} has more than one mirror layer", index.name);
                }
            }
            mirror.context(format!("repository {:?} has no mirror layer", index.name))
        }
    }
}

fn mirror_prefetch<'a>(config: &'a Config, mirror: &str) -> anyhow::Result<&'a MirrorPrefetchConfig> {
    config
        .indexes
        .iter()
        .find_map(|index| match (index.name == mirror, &index.kind) {
            (true, ConfigIndexKind::Mirror { prefetch, .. }) => Some(prefetch.as_ref()),
            _ => None,
        })
        .context(format!("mirror config {mirror:?} not found"))
}

fn content_type_is_json(content_type: Option<&str>) -> bool {
    content_type.is_none_or(|content_type| content_type.contains("json"))
}

fn write_page_row(out: &mut Output, repo: &str, project: &str, status: &str, reason: &str) -> anyhow::Result<()> {
    write_row(out, Row::page(repo, project, status, reason))
}

fn write_file_row(
    out: &mut Output,
    repo: &str,
    project: &str,
    file: &MirrorFile,
    status: &str,
    reason: &str,
) -> anyhow::Result<()> {
    write_file_row_bytes(out, repo, project, file, file.size, status, reason)
}

fn write_file_row_bytes(
    out: &mut Output,
    repo: &str,
    project: &str,
    file: &MirrorFile,
    bytes: Option<u64>,
    status: &str,
    reason: &str,
) -> anyhow::Result<()> {
    write_row(
        out,
        Row {
            kind: "file",
            repo,
            project,
            filename: &file.filename,
            digest: &file.digest,
            url: &file.url,
            bytes,
            status,
            reason,
        },
    )
}

fn write_row(out: &mut Output, row: Row<'_>) -> anyhow::Result<()> {
    let bytes = row.bytes.map_or_else(String::new, |bytes| bytes.to_string());
    let cells = [
        row.kind,
        row.repo,
        row.project,
        row.filename,
        row.digest,
        row.url,
        &bytes,
        row.status,
        row.reason,
    ];
    let mut separator = "";
    for cell in cells {
        out.write_all(separator.as_bytes())?;
        out.write_all(cell.as_bytes())?;
        separator = "\t";
    }
    out.write_all(b"\n")?;
    Ok(())
}

fn write_count(out: &mut Output, repo: &str, name: &str, value: u64) -> anyhow::Result<()> {
    write_row(
        out,
        Row {
            kind: "summary",
            repo,
            project: "",
            filename: name,
            digest: "",
            url: "",
            bytes: Some(value),
            status: name,
            reason: "",
        },
    )
}

fn blob_size(state: &AppState, digest: &Digest) -> u64 {
    state
        .blobs
        .path_for(digest)
        .metadata()
        .map(|metadata| metadata.len())
        .unwrap_or_default()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionSource {
    Upstream,
    Cache,
}

#[derive(Default)]
struct SyncSummary {
    projects: u64,
    downloaded: u64,
    bytes: u64,
    skipped: u64,
    failures: u64,
}

struct Selection {
    projects: Vec<String>,
    rules: BTreeMap<String, ProjectRule>,
    filters: ArtifactFilters,
}

#[derive(Default)]
struct ProjectRule {
    specs: Vec<Option<VersionSpecifiers>>,
}

impl ProjectRule {
    fn allows(&self, version: &Version) -> bool {
        self.specs.is_empty()
            || self
                .specs
                .iter()
                .any(|spec| spec.as_ref().is_none_or(|spec| spec.contains(version)))
    }
}

#[derive(Clone, Copy)]
struct BlobCheck<'a> {
    kind: &'a str,
    filename: &'a str,
    digest_hex: &'a str,
    url: &'a str,
}

#[derive(Clone, Copy)]
struct Row<'a> {
    kind: &'a str,
    repo: &'a str,
    project: &'a str,
    filename: &'a str,
    digest: &'a str,
    url: &'a str,
    bytes: Option<u64>,
    status: &'a str,
    reason: &'a str,
}

impl<'a> Row<'a> {
    const fn page(repo: &'a str, project: &'a str, status: &'a str, reason: &'a str) -> Self {
        Self {
            kind: "page",
            repo,
            project,
            filename: "",
            digest: "",
            url: "",
            bytes: None,
            status,
            reason,
        }
    }

    fn metadata(
        repo: &'a str,
        project: &'a str,
        filename: &'a str,
        metadata: &'a MirrorMetadata,
        bytes: Option<u64>,
        status: &'a str,
        reason: &'a str,
    ) -> Self {
        Self {
            kind: "metadata",
            repo,
            project,
            filename,
            digest: &metadata.digest,
            url: &metadata.url,
            bytes,
            status,
            reason,
        }
    }

    const fn check(
        repo: &'a str,
        project: &'a str,
        check: BlobCheck<'a>,
        digest: &'a str,
        status: &'a str,
        reason: &'a str,
    ) -> Self {
        Self {
            kind: check.kind,
            repo,
            project,
            filename: check.filename,
            digest,
            url: check.url,
            bytes: None,
            status,
            reason,
        }
    }
}

struct ProjectSelector {
    project: String,
    spec: Option<VersionSpecifiers>,
}

struct ArtifactFilters {
    include_wheels: bool,
    include_sdists: bool,
    python_tags: BTreeSet<String>,
    abi_tags: BTreeSet<String>,
    platform_tags: BTreeSet<String>,
    max_file_size_bytes: Option<u64>,
    metadata_only: bool,
}

impl From<MirrorPrefetchConfig> for ArtifactFilters {
    fn from(config: MirrorPrefetchConfig) -> Self {
        Self {
            include_wheels: config.include_wheels,
            include_sdists: config.include_sdists,
            python_tags: config.python_tags.into_iter().collect(),
            abi_tags: config.abi_tags.into_iter().collect(),
            platform_tags: config.platform_tags.into_iter().collect(),
            max_file_size_bytes: config.max_file_size_bytes,
            metadata_only: config.metadata_only,
        }
    }
}

enum FileCandidate {
    Include(MirrorFile),
    Skip(MirrorFile, &'static str),
}

struct MirrorFile {
    filename: String,
    digest: String,
    url: String,
    size: Option<u64>,
    metadata: Option<MirrorMetadata>,
    source: Option<DistributionFilename>,
}

struct MirrorMetadata {
    url: String,
    digest: String,
}

struct Target {
    repo: String,
    route: String,
    position: usize,
    mirror: String,
    client: UpstreamClient,
    offline: bool,
    prefetch: MirrorPrefetchConfig,
}

enum SyncOutcome {
    Cached(u64),
    Downloaded(u64),
}
