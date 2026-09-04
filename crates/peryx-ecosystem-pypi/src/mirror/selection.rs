use crate::policy::PypiPolicy as _;
use crate::source_policy::SourceSelection;
use crate::store::PypiStore as _;
use crate::{
    CoreMetadata, DistributionKind, File, ProjectDetail, is_valid_name, normalize_name, parse_distribution_filename,
    parse_version_specifiers,
};
use anyhow::{Context as _, bail};
use peryx_driver::ServingState;
use peryx_index::{Index, IndexKind};
use peryx_policy::{PolicyAction, PolicyDenial};
use peryx_upstream::UpstreamClient;
use std::borrow::Cow;
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
        if mode.requires_metadata_only() {
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
    if matches!(source, SelectionSource::UpstreamPreview) {
        return Ok(crate::catalog::read_catalog_projects(router).await?);
    }
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
    let mut processed = BTreeSet::new();
    for path in paths {
        read_requirements(path, &mut selectors, &mut processed)?;
    }
    Ok(selectors)
}

fn read_requirements(
    path: &Path,
    selectors: &mut Vec<String>,
    processed: &mut BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    let canonical = canonical_requirements_path(path)?;
    if processed.contains(&canonical) {
        return Ok(());
    }
    let mut stack = vec![requirement_file(path.to_path_buf(), canonical)?];
    while let Some(file) = stack.last_mut() {
        let Some(logical) = file.lines.next() else {
            processed.insert(stack.pop().expect("the stack has a completed file").canonical);
            continue;
        };
        let line = requirement_line(&logical);
        if let Some(nested) = include_target(line) {
            // Every file on this stack came from `requirement_file`, which read it, and `Path::parent`
            // answers `None` only for an empty path or one that is nothing but a root or a prefix.
            // A root is a directory and an empty path names nothing, so neither is readable and
            // neither reaches here. A bare filename keeps its empty parent, which joins to the
            // sibling this wants.
            let parent = file.path.parent().expect("requirement_file read this path as a file");
            let nested = parent.join(nested);
            let canonical = canonical_requirements_path(&nested)?;
            if let Some(cycle_start) = stack.iter().position(|file| file.canonical == canonical) {
                let mut cycle = stack[cycle_start..]
                    .iter()
                    .map(|file| file.path.display().to_string())
                    .collect::<Vec<_>>();
                cycle.push(nested.display().to_string());
                bail!("requirements include cycle: {}", cycle.join(" -> "));
            }
            if !processed.contains(&canonical) {
                stack.push(requirement_file(nested, canonical)?);
            }
        } else if !line.starts_with('-') {
            selectors.push(line.to_owned());
        }
    }
    Ok(())
}

fn canonical_requirements_path(path: &Path) -> anyhow::Result<PathBuf> {
    std::fs::canonicalize(path).context(format!("read requirements {}", path.display()))
}

fn requirement_file(path: PathBuf, canonical: PathBuf) -> anyhow::Result<RequirementFile> {
    let text = std::fs::read_to_string(&canonical).context(format!("read requirements {}", path.display()))?;
    Ok(RequirementFile {
        path,
        canonical,
        lines: logical_lines(&text).into_iter(),
    })
}

struct RequirementFile {
    path: PathBuf,
    canonical: PathBuf,
    lines: std::vec::IntoIter<String>,
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

/// The policy verdict a prefetch run reaches for one project, so a refusal reaches the report instead
/// of thinning the candidate set behind the operator's back.
pub(super) struct ProjectAdmission {
    project: Option<String>,
    files: BTreeMap<String, String>,
}

impl ProjectAdmission {
    /// Why the whole project is withheld, when policy withholds it.
    pub(super) fn refusal(&self) -> Option<&str> {
        self.project.as_deref()
    }
}

/// The two policies a prefetched file passes, in the order serving applies them: the cached member
/// decides what peryx may fetch at all, then the target index decides what it would serve. Each stage
/// judges only the files the previous one admitted, so a release-wide rule such as the project size
/// limit sees the sibling set that would actually reach a client.
///
/// Reads no store and writes nothing beyond the policy decisions every evaluation records, so plan
/// reaches the same verdict sync does without materializing a page.
pub(super) fn admission(
    state: &ServingState,
    target: &Target,
    project: &str,
    detail: &ProjectDetail,
) -> ProjectAdmission {
    let now = Some((state.clock)());
    let mut files = BTreeMap::new();
    let mut admitted = Cow::Borrowed(detail);
    for (index, action) in stages(state, target) {
        let verdict = index.policy.admit_detail(action, project, &admitted, now);
        if let Some(denial) = verdict.project {
            return ProjectAdmission {
                project: Some(refusal_reason(&denial)),
                files,
            };
        }
        if verdict.files.is_empty() {
            continue;
        }
        admitted = Cow::Owned(retaining(&admitted, &verdict.files));
        files.extend(
            verdict
                .files
                .into_iter()
                .map(|(filename, denial)| (filename, refusal_reason(&denial))),
        );
    }
    ProjectAdmission { project: None, files }
}

/// The refusal a run can reach from the project name alone, before it spends an upstream request on a
/// page the target would not serve.
pub(super) fn refusal(state: &ServingState, target: &Target, project: &str) -> Option<String> {
    // The rule that names the project comes first: it explains the refusal in the operator's own
    // terms, where the source policy can only say that no cached member survived.
    stages(state, target)
        .into_iter()
        .find_map(|(index, action)| index.policy.check_resource(action, project).err())
        .map(|denial| refusal_reason(&denial))
        .or_else(|| shadow_refusal(state, target, project))
}

fn stages<'a>(state: &'a ServingState, target: &Target) -> [(&'a Index, PolicyAction); 2] {
    [
        (state.index_at(target.cached_position), PolicyAction::Cached),
        (state.index_at(target.position), PolicyAction::Serve),
    ]
}

/// Why a virtual target's source policy keeps every cached candidate out of the merged page. Mirroring
/// a leaf the repository has already ranked out of its own view buys storage nothing.
fn shadow_refusal(state: &ServingState, target: &Target, project: &str) -> Option<String> {
    let index = state.index_at(target.position);
    let IndexKind::Virtual { layers, .. } = &index.kind else {
        return None;
    };
    let hosted = peryx_index::leaf_order(&state.indexes, layers)
        .into_iter()
        .filter(|&position| matches!(state.index_at(position).kind, IndexKind::Hosted { .. }))
        .any(|position| hosted_files(state, &state.index_at(position).name, project));
    let excluded = SourceSelection::new(index, project).cached_exclusion(hosted)?;
    Some(format!(
        "virtual policy: cached members excluded by {}",
        excluded.as_str()
    ))
}

fn hosted_files(state: &ServingState, hosted: &str, project: &str) -> bool {
    crate::cache::local_detail(state, hosted, project).is_ok_and(|detail| detail.is_some())
}

/// One refusal, phrased for the report: which stage refused, and the rule's own explanation.
pub(super) fn refusal_reason(denial: &PolicyDenial) -> String {
    format!("{} policy: {}", denial.action, denial.reason)
}

fn retaining(detail: &ProjectDetail, refused: &BTreeMap<String, PolicyDenial>) -> ProjectDetail {
    ProjectDetail {
        meta: detail.meta.clone(),
        name: detail.name.clone(),
        versions: detail.versions.clone(),
        files: detail
            .files
            .iter()
            .filter(|file| !refused.contains_key(&file.filename))
            .cloned()
            .collect(),
    }
}

pub(super) fn candidates<'a>(
    detail: &'a ProjectDetail,
    rule: Option<&'a ProjectRule>,
    filters: &'a ArtifactFilters,
    admission: &'a ProjectAdmission,
) -> impl Iterator<Item = FileCandidate> + 'a {
    detail.files.iter().map(move |file| {
        let refused = admission.files.get(&file.filename).cloned();
        let file = prefetch_file(file);
        if let Some(reason) = refused {
            return FileCandidate::Skip(file, Cow::Owned(reason));
        }
        if file.digest.is_empty() {
            return FileCandidate::Skip(file, Cow::Borrowed("missing sha256"));
        }
        match decision(&file, rule, filters) {
            Ok(()) => FileCandidate::Include(file),
            Err(reason) => FileCandidate::Skip(file, Cow::Borrowed(reason)),
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
    let (cached_position, client, offline) = target_upstream(state, position)?;
    Ok(Target {
        index: selector.to_owned(),
        route: index.route.clone(),
        position,
        cached_position,
        cached: state.index_at(cached_position).name.clone(),
        client,
        offline,
        prefetch: configured.clone(),
    })
}

fn target_upstream(state: &ServingState, position: usize) -> anyhow::Result<(usize, UpstreamClient, bool)> {
    let index = state.index_at(position);
    match &index.kind {
        IndexKind::Cached { client, offline } => Ok((position, client.clone(), *offline)),
        IndexKind::Hosted { .. } => bail!("index {:?} is hosted and has no upstream", index.name),
        IndexKind::Virtual { layers, .. } => {
            let mut cached = None;
            for &pos in layers {
                let layer = state.index_at(pos);
                if let IndexKind::Cached { client, offline } = &layer.kind
                    && cached.replace((pos, client.clone(), *offline)).is_some()
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
