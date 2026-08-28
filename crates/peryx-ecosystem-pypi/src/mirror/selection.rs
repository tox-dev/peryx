use crate::store::PypiStore as _;
use crate::{
    CoreMetadata, DistributionKind, File, ProjectDetail, is_valid_name, normalize_name, parse_distribution_filename,
    parse_version_specifiers,
};
use anyhow::{Context as _, bail};
use peryx_driver::ServingState;
use peryx_index::{Index, IndexKind};
use peryx_upstream::UpstreamClient;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::{
    ArtifactFilters, FileCandidate, PrefetchConfig, PrefetchFile, PrefetchMetadata, PrefetchMode, PrefetchOptions,
    ProjectRule, ProjectSelector, Selection, SelectionSource, Target,
};

pub(super) async fn selection(
    state: &ServingState,
    target: &Target,
    options: &PrefetchOptions,
    source: SelectionSource,
) -> anyhow::Result<Selection> {
    let mut filters = target.prefetch.clone();
    if let Some(mode) = options.mode {
        filters.mode = mode;
        if matches!(mode, PrefetchMode::MetadataOnly) {
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
        PrefetchMode::All => all_projects(state, target, source).await?,
        PrefetchMode::Selected | PrefetchMode::MetadataOnly => {
            if rules.is_empty() {
                bail!(
                    "cached index {} has no selected packages; add [index.prefetch].packages or --option 'packages=[\"requests\"]'",
                    target.index
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

async fn all_projects(state: &ServingState, target: &Target, source: SelectionSource) -> anyhow::Result<Vec<String>> {
    if matches!(source, SelectionSource::Cache) || target.offline {
        return Ok(normalized_projects(state.meta.list_projects(&target.cached)?));
    }
    let router = state
        .upstream_routes
        .get(&target.cached)
        .expect("a cached index always has an upstream route");
    let sync = crate::catalog::sync_catalog(
        router,
        &state.cache.inflight,
        &state.meta,
        &target.cached,
        target.client.base_url(),
    )
    .await;
    let outcome = match &sync {
        Ok(crate::catalog::CatalogSyncOutcome::Published { projects }) => {
            crate::catalog_job::CatalogMetricOutcome::Published { projects: *projects }
        }
        Ok(crate::catalog::CatalogSyncOutcome::NotModified { projects }) => {
            crate::catalog_job::CatalogMetricOutcome::NotModified { projects: *projects }
        }
        Err(_) => crate::catalog_job::CatalogMetricOutcome::Error,
    };
    crate::catalog_job::record_catalog_metrics(&state.metrics, &target.cached, outcome);
    sync?;
    Ok(normalized_projects(state.meta.list_projects(&target.cached)?))
}

fn normalized_projects(projects: Vec<String>) -> Vec<String> {
    let mut normalized = BTreeSet::new();
    for project in projects {
        normalized.insert(normalize_name(&project));
    }
    normalized.into_iter().collect()
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
    let trimmed = raw[name_end..].trim();
    let spec_text = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']').map(|(_, after)| after.trim()))
        .unwrap_or(trimmed);
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
    for logical in logical_lines(&text) {
        let line = requirement_line(&logical);
        if let Some(nested) = include_target(line) {
            let fallback_parent = Path::new(".");
            let nested = path.parent().unwrap_or(fallback_parent).join(nested);
            read_requirements(&nested, selectors, seen)?;
        } else if !line.starts_with('-') {
            selectors.push(line.to_owned());
        }
    }
    Ok(())
}

// pip accepts `-r`/`--requirement` and `-c`/`--constraint` with the path attached (`-rchild.txt`),
// joined by `=` (`--requirement=child.txt`), or separated by whitespace, so match each form rather
// than a single space-delimited prefix.
fn include_target(line: &str) -> Option<&str> {
    for flag in ["--requirement", "--constraint", "-r", "-c"] {
        let Some(rest) = line.strip_prefix(flag) else {
            continue;
        };
        let target = match rest.chars().next() {
            None => continue,
            Some('=') => &rest[1..],
            Some(ch) if ch.is_whitespace() => rest,
            Some(_) if flag.starts_with("--") => continue,
            Some(_) => rest,
        };
        let target = target.trim();
        if !target.is_empty() {
            return Some(target);
        }
    }
    None
}

// Reduce a requirements file to pip's logical lines: join backslash continuations, then drop
// comments. pip joins a physical line ending in an unescaped `\` with the next one, but never
// treats a comment line as a continuation even when it ends in `\`.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut pending: Option<String> = None;
    for raw in text.lines() {
        if !is_comment_line(raw) && raw.ends_with('\\') {
            pending
                .get_or_insert_with(String::new)
                .push_str(raw.trim_end_matches('\\'));
            continue;
        }
        let logical = pending.take().map_or_else(
            || raw.to_owned(),
            |mut head| {
                head.push_str(raw);
                head
            },
        );
        push_logical(&mut lines, &logical);
    }
    if let Some(head) = pending {
        push_logical(&mut lines, &head);
    }
    lines
}

fn push_logical(lines: &mut Vec<String>, logical: &str) {
    let content = strip_comment(logical).trim();
    if !content.is_empty() {
        lines.push(content.to_owned());
    }
}

fn is_comment_line(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

// pip's COMMENT_RE strips from the first `#` that starts a line or follows whitespace (a tab
// counts); a `#` glued to a preceding token is part of the token, not a comment.
fn strip_comment(line: &str) -> &str {
    let mut after_whitespace = true;
    for (idx, ch) in line.char_indices() {
        if ch == '#' && after_whitespace {
            return &line[..idx];
        }
        after_whitespace = ch.is_whitespace();
    }
    line
}

fn requirement_line(line: &str) -> &str {
    line.split_once(" --")
        .map_or(line, |(requirement, _)| requirement)
        .trim()
}

pub(super) fn candidates<'a>(
    detail: &'a ProjectDetail,
    rule: Option<&'a ProjectRule>,
    filters: &'a ArtifactFilters,
) -> impl Iterator<Item = FileCandidate> + 'a {
    detail.files.iter().map(move |file| {
        let file = prefetch_file(file);
        if file.digest.is_empty() {
            return FileCandidate::Skip(file, "missing sha256");
        }
        match decision(&file, rule, filters) {
            Ok(()) => FileCandidate::Include(file),
            Err(reason) => FileCandidate::Skip(file, reason),
        }
    })
}

fn prefetch_file(file: &File) -> PrefetchFile {
    let digest = file.hashes.get("sha256").cloned();
    let metadata = metadata_sibling(file);
    let source = parse_distribution_filename(&file.filename).ok();
    PrefetchFile {
        filename: file.filename.clone(),
        digest: digest.unwrap_or_default(),
        url: file.url.clone(),
        size: file.size,
        metadata,
        source,
    }
}

fn metadata_sibling(file: &File) -> Option<PrefetchMetadata> {
    let CoreMetadata::Hashes(hashes) = file.metadata() else {
        return None;
    };
    Some(PrefetchMetadata {
        url: format!("{}.metadata", file.url),
        digest: hashes.get("sha256")?.clone(),
    })
}

fn decision(file: &PrefetchFile, rule: Option<&ProjectRule>, filters: &ArtifactFilters) -> Result<(), &'static str> {
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
        DistributionKind::SdistTarGz | DistributionKind::SdistZip => {
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

pub(super) fn target(configured: &PrefetchConfig, state: &ServingState, selector: &str) -> anyhow::Result<Target> {
    let position = state
        .indexes
        .iter()
        .position(|index| index.name == selector || index.route == selector)
        .context(format!("unknown cached index {selector:?}"))?;
    let index = state.index_at(position);
    let (cached, client, offline) = target_upstream(state, index)?;
    Ok(Target {
        index: selector.to_owned(),
        route: index.route.clone(),
        position,
        cached,
        client,
        offline,
        prefetch: configured.clone(),
    })
}

fn target_upstream(state: &ServingState, index: &Index) -> anyhow::Result<(String, UpstreamClient, bool)> {
    match &index.kind {
        IndexKind::Cached { client, offline } => Ok((index.name.clone(), client.clone(), *offline)),
        IndexKind::Hosted { .. } => bail!("index {:?} is hosted and has no upstream", index.name),
        IndexKind::Virtual { layers, .. } => {
            let mut cached = None;
            for &pos in layers {
                let layer = state.index_at(pos);
                if let IndexKind::Cached { client, offline } = &layer.kind
                    && cached.replace((layer.name.clone(), client.clone(), *offline)).is_some()
                {
                    bail!("index {:?} has more than one cached member", index.name);
                }
            }
            cached.context(format!("index {:?} has no cached member", index.name))
        }
    }
}

pub(super) fn content_type_is_json(content_type: Option<&str>) -> bool {
    content_type.is_none_or(|content_type| content_type.contains("json"))
}

#[cfg(test)]
#[path = "../../tests/unit/mirror/selection_tests.rs"]
mod tests;
