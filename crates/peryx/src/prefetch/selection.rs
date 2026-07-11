//! Project and artifact selection: resolving the target index, applying filters, and deciding
//! which distribution files a prefetch run touches.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use peryx_ecosystem_pypi::store::PypiStore as _;
use peryx_ecosystem_pypi::{
    CoreMetadata, DistributionKind, File, ProjectDetail, is_valid_name, normalize_name, parse_distribution_filename,
    parse_index, parse_index_html, parse_version_specifiers,
};
use peryx_http::{AppState, Index, IndexKind};
use peryx_upstream::UpstreamClient;

use super::{
    ArtifactFilters, FileCandidate, PrefetchFile, PrefetchMetadata, ProjectRule, ProjectSelector, Selection,
    SelectionSource, Target,
};
use crate::cli::PrefetchOptions;
use crate::config::{Config, IndexKind as ConfigIndexKind, PrefetchConfig, PrefetchMode};

pub(super) async fn selection(
    state: &Arc<AppState>,
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
                    "cached index {} has no selected packages; add [index.prefetch].packages or --package",
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

async fn all_projects(state: &Arc<AppState>, target: &Target, source: SelectionSource) -> anyhow::Result<Vec<String>> {
    if matches!(source, SelectionSource::Cache) || target.offline {
        return Ok(state
            .meta
            .list_projects(&target.cached)?
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

pub(super) fn target(config: &Config, state: &Arc<AppState>, selector: &str) -> anyhow::Result<Target> {
    let position = state
        .indexes
        .iter()
        .position(|index| index.name == selector || index.route == selector)
        .context(format!("unknown cached index {selector:?}"))?;
    let index = state.index_at(position);
    let (cached, client, offline) = target_upstream(state, index)?;
    let prefetch = cached_prefetch(config, &cached)?.clone();
    Ok(Target {
        index: selector.to_owned(),
        route: index.route.clone(),
        position,
        cached,
        client,
        offline,
        prefetch,
    })
}

fn target_upstream(state: &AppState, index: &Index) -> anyhow::Result<(String, UpstreamClient, bool)> {
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

fn cached_prefetch<'a>(config: &'a Config, cached: &str) -> anyhow::Result<&'a PrefetchConfig> {
    config
        .indexes
        .iter()
        .find_map(|index| match (index.name == cached, &index.kind) {
            (true, ConfigIndexKind::Cached { prefetch, .. }) => Some(prefetch.as_ref()),
            _ => None,
        })
        .context(format!("prefetch config {cached:?} not found"))
}

pub(super) fn content_type_is_json(content_type: Option<&str>) -> bool {
    content_type.is_none_or(|content_type| content_type.contains("json"))
}
