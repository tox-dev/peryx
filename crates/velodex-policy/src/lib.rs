//! Repository policy checks compiled from configuration.

use std::collections::{BTreeSet, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use velodex_ecosystem_pypi::{
    DistributionKind, File, ProjectDetail, ProjectList, normalize_name, parse_distribution_filename,
    parse_version_specifiers,
};

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    pub allow_projects: Vec<String>,
    pub block_projects: Vec<String>,
    pub allow_versions: Option<String>,
    pub allow_package_types: Vec<PackageType>,
    pub block_package_types: Vec<PackageType>,
    pub allow_wheel_pythons: Vec<String>,
    pub block_wheel_pythons: Vec<String>,
    pub allow_wheel_platforms: Vec<String>,
    pub block_wheel_platforms: Vec<String>,
    pub max_file_size_bytes: Option<u64>,
    pub max_project_size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageType {
    Wheel,
    Sdist,
}

impl PackageType {
    const fn mask(self) -> u8 {
        match self {
            Self::Wheel => 1,
            Self::Sdist => 2,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Wheel => "wheel",
            Self::Sdist => "sdist",
        }
    }
}

impl From<DistributionKind> for PackageType {
    fn from(value: DistributionKind) -> Self {
        match value {
            DistributionKind::Wheel => Self::Wheel,
            DistributionKind::SdistTarGz => Self::Sdist,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyConfigError {
    #[error("invalid PEP 440 version specifier {0:?}")]
    VersionSpecifiers(String),
    #[error("policy tag {0:?} is empty")]
    EmptyTag(String),
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    allow_projects: HashSet<String>,
    block_projects: HashSet<String>,
    allow_versions: Option<velodex_ecosystem_pypi::VersionSpecifiers>,
    allow_package_types: u8,
    block_package_types: u8,
    allow_wheel_pythons: HashSet<String>,
    block_wheel_pythons: HashSet<String>,
    allow_wheel_platforms: HashSet<String>,
    block_wheel_platforms: HashSet<String>,
    max_file_size_bytes: Option<u64>,
    max_project_size_bytes: Option<u64>,
    active: bool,
}

impl Policy {
    /// Compile operator configuration once at startup.
    ///
    /// # Errors
    /// Returns an error when a version specifier or wheel tag filter cannot be used.
    pub fn compile(config: &PolicyConfig) -> Result<Self, PolicyConfigError> {
        let allow_versions = config
            .allow_versions
            .as_deref()
            .map(|value| {
                parse_version_specifiers(value).ok_or_else(|| PolicyConfigError::VersionSpecifiers(value.to_owned()))
            })
            .transpose()?;
        let policy = Self {
            allow_projects: normalize_projects(&config.allow_projects),
            block_projects: normalize_projects(&config.block_projects),
            allow_versions,
            allow_package_types: package_mask(&config.allow_package_types),
            block_package_types: package_mask(&config.block_package_types),
            allow_wheel_pythons: tags(&config.allow_wheel_pythons)?,
            block_wheel_pythons: tags(&config.block_wheel_pythons)?,
            allow_wheel_platforms: tags(&config.allow_wheel_platforms)?,
            block_wheel_platforms: tags(&config.block_wheel_platforms)?,
            max_file_size_bytes: config.max_file_size_bytes,
            max_project_size_bytes: config.max_project_size_bytes,
            active: false,
        };
        Ok(Self {
            active: policy.is_active(),
            ..policy
        })
    }

    #[must_use]
    pub const fn has_project_size_limit(&self) -> bool {
        self.max_project_size_bytes.is_some()
    }

    #[must_use]
    fn is_active(&self) -> bool {
        !self.allow_projects.is_empty()
            || !self.block_projects.is_empty()
            || self.allow_versions.is_some()
            || self.allow_package_types != 0
            || self.block_package_types != 0
            || !self.allow_wheel_pythons.is_empty()
            || !self.block_wheel_pythons.is_empty()
            || !self.allow_wheel_platforms.is_empty()
            || !self.block_wheel_platforms.is_empty()
            || self.max_file_size_bytes.is_some()
            || self.max_project_size_bytes.is_some()
    }

    #[must_use]
    pub const fn active(&self) -> bool {
        self.active
    }

    /// Check whether a project name is allowed.
    ///
    /// # Errors
    /// Returns a denial when the project misses an allow list or matches a block list.
    pub fn check_project(&self, action: PolicyAction, project: &str) -> Result<(), PolicyDenial> {
        if self.allow_projects.is_empty() || self.allow_projects.contains(project) {
            if !self.block_projects.contains(project) {
                return Ok(());
            }
            return Err(PolicyDenial::new(
                action,
                project,
                None,
                None,
                "project-block-list",
                "project",
                format!("project {project:?} is blocked"),
            ));
        }
        Err(PolicyDenial::new(
            action,
            project,
            None,
            None,
            "project-allow-list",
            "project",
            format!("project {project:?} is not in the allow list"),
        ))
    }

    /// Check whether one Simple API file record is allowed.
    ///
    /// # Errors
    /// Returns a denial when the file's parsed facts match a configured policy rule.
    pub fn check_file(&self, action: PolicyAction, project: &str, file: &File) -> Result<(), PolicyDenial> {
        let facts = FileFacts::from_file(project, file);
        self.check_facts(action, &facts)
    }

    /// Check whether a direct artifact or metadata download is allowed.
    ///
    /// # Errors
    /// Returns a denial when the filename or known size matches a configured policy rule.
    pub fn check_download(&self, action: PolicyAction, filename: &str, size: Option<u64>) -> Result<(), PolicyDenial> {
        let artifact = filename.strip_suffix(".metadata").unwrap_or(filename);
        let facts = FileFacts::from_filename(artifact, size);
        self.check_facts(action, &facts)
    }

    /// Filter a project detail response through this policy.
    ///
    /// # Errors
    /// Returns a denial when project-wide rules reject the whole response.
    pub fn apply_detail(
        &self,
        action: PolicyAction,
        project: &str,
        mut detail: ProjectDetail,
    ) -> Result<ProjectDetail, PolicyDenial> {
        self.check_project(action, project)?;
        if !self.active {
            return Ok(detail);
        }
        detail
            .files
            .retain(|file| self.check_file(action, project, file).is_ok());
        if let Some(limit) = self.max_project_size_bytes {
            apply_project_size_limit(action, project, limit, &detail)?;
        }
        retain_versions_with_files(&mut detail);
        Ok(detail)
    }

    #[must_use]
    pub fn apply_list(&self, list: ProjectList) -> ProjectList {
        if self.allow_projects.is_empty() && self.block_projects.is_empty() {
            return list;
        }
        ProjectList {
            meta: list.meta,
            projects: list
                .projects
                .into_iter()
                .filter(|entry| {
                    self.check_project(PolicyAction::Serve, &normalize_name(&entry.name))
                        .is_ok()
                })
                .collect(),
        }
    }

    #[must_use]
    pub fn preview_detail(&self, action: PolicyAction, detail: &ProjectDetail) -> Vec<PolicyDenial> {
        let mut denials = Vec::new();
        if let Err(denial) = self.check_project(action, &detail.name) {
            denials.push(denial);
            return denials;
        }
        let mut allowed = Vec::new();
        for file in &detail.files {
            match self.check_file(action, &detail.name, file) {
                Ok(()) => allowed.push(file),
                Err(denial) => denials.push(denial),
            }
        }
        if let Some(limit) = self.max_project_size_bytes
            && let Some(denial) = project_size_denial(action, &detail.name, allowed, limit)
        {
            denials.push(denial);
        }
        denials
    }

    fn check_facts(&self, action: PolicyAction, facts: &FileFacts) -> Result<(), PolicyDenial> {
        self.check_project(action, &facts.project)?;
        self.check_version(action, facts)?;
        self.check_package_type(action, facts)?;
        self.check_wheel_tags(action, facts)?;
        self.check_file_size(action, facts)?;
        Ok(())
    }

    fn check_version(&self, action: PolicyAction, facts: &FileFacts) -> Result<(), PolicyDenial> {
        if let Some(specifiers) = &self.allow_versions {
            let Some(version) = &facts.version else {
                return Err(facts.denial(
                    action,
                    "version-specifier",
                    "version",
                    "file version is unknown".to_owned(),
                ));
            };
            if !specifiers.contains(version) {
                return Err(facts.denial(
                    action,
                    "version-specifier",
                    "version",
                    format!("version {version} is outside the allowed range"),
                ));
            }
        }
        Ok(())
    }

    fn check_package_type(&self, action: PolicyAction, facts: &FileFacts) -> Result<(), PolicyDenial> {
        if self.allow_package_types != 0 {
            let Some(kind) = facts.package_type else {
                return Err(facts.denial(
                    action,
                    "package-type-allow-list",
                    "package_type",
                    "package type is unknown".to_owned(),
                ));
            };
            if self.allow_package_types & kind.mask() == 0 {
                return Err(facts.denial(
                    action,
                    "package-type-allow-list",
                    "package_type",
                    format!("package type {} is not allowed", kind.as_str()),
                ));
            }
        }
        if let Some(kind) = facts.package_type
            && self.block_package_types & kind.mask() != 0
        {
            return Err(facts.denial(
                action,
                "package-type-block-list",
                "package_type",
                format!("package type {} is blocked", kind.as_str()),
            ));
        }
        Ok(())
    }

    fn check_wheel_tags(&self, action: PolicyAction, facts: &FileFacts) -> Result<(), PolicyDenial> {
        check_wheel_tag(
            action,
            facts,
            WheelTagRule {
                tag: facts.python_tag.as_deref(),
                tags: &self.allow_wheel_pythons,
                blocked: false,
                rule: "wheel-python-allow-list",
                field: "wheel_python",
            },
        )?;
        check_wheel_tag(
            action,
            facts,
            WheelTagRule {
                tag: facts.python_tag.as_deref(),
                tags: &self.block_wheel_pythons,
                blocked: true,
                rule: "wheel-python-block-list",
                field: "wheel_python",
            },
        )?;
        check_wheel_tag(
            action,
            facts,
            WheelTagRule {
                tag: facts.platform_tag.as_deref(),
                tags: &self.allow_wheel_platforms,
                blocked: false,
                rule: "wheel-platform-allow-list",
                field: "wheel_platform",
            },
        )?;
        check_wheel_tag(
            action,
            facts,
            WheelTagRule {
                tag: facts.platform_tag.as_deref(),
                tags: &self.block_wheel_platforms,
                blocked: true,
                rule: "wheel-platform-block-list",
                field: "wheel_platform",
            },
        )?;
        Ok(())
    }

    fn check_file_size(&self, action: PolicyAction, facts: &FileFacts) -> Result<(), PolicyDenial> {
        if let Some(limit) = self.max_file_size_bytes {
            let Some(size) = facts.size else {
                return Err(facts.denial(action, "max-file-size", "size", "file size is unknown".to_owned()));
            };
            if size > limit {
                return Err(facts.denial(
                    action,
                    "max-file-size",
                    "size",
                    format!("file size {size} exceeds limit {limit}"),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyAction {
    Upload,
    Cached,
    Serve,
}

impl fmt::Display for PolicyAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Upload => "upload",
            Self::Cached => "cached",
            Self::Serve => "serve",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyDenial {
    pub action: PolicyAction,
    pub project: Box<str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<Box<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<Box<str>>,
    pub rule: &'static str,
    pub field: &'static str,
    pub reason: Box<str>,
}

impl PolicyDenial {
    fn new(
        action: PolicyAction,
        project: &str,
        filename: Option<&str>,
        version: Option<String>,
        rule: &'static str,
        field: &'static str,
        reason: String,
    ) -> Self {
        Self {
            action,
            project: Box::from(project),
            filename: filename.map(Box::from),
            version: version.map(String::into_boxed_str),
            rule,
            field,
            reason: reason.into_boxed_str(),
        }
    }
}

impl fmt::Display for PolicyDenial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for PolicyDenial {}

#[derive(Clone, Copy)]
struct WheelTagRule<'a> {
    tag: Option<&'a str>,
    tags: &'a HashSet<String>,
    blocked: bool,
    rule: &'static str,
    field: &'static str,
}

fn check_wheel_tag(action: PolicyAction, facts: &FileFacts, rule: WheelTagRule<'_>) -> Result<(), PolicyDenial> {
    if rule.tags.is_empty() || facts.package_type != Some(PackageType::Wheel) {
        return Ok(());
    }
    let matches = rule
        .tag
        .is_some_and(|tag| tag.split('.').any(|part| rule.tags.contains(part)));
    match (rule.blocked, matches) {
        (true, true) => Err(facts.denial(
            action,
            rule.rule,
            rule.field,
            format!("wheel tag {tag:?} is blocked", tag = rule.tag.unwrap_or_default()),
        )),
        (false, false) => Err(facts.denial(
            action,
            rule.rule,
            rule.field,
            format!("wheel tag {tag:?} is not allowed", tag = rule.tag.unwrap_or_default()),
        )),
        _ => Ok(()),
    }
}

struct FileFacts {
    project: String,
    filename: Option<String>,
    version: Option<velodex_ecosystem_pypi::Version>,
    package_type: Option<PackageType>,
    python_tag: Option<String>,
    platform_tag: Option<String>,
    size: Option<u64>,
}

impl FileFacts {
    fn from_file(project: &str, file: &File) -> Self {
        let parsed = parse_distribution_filename(&file.filename).ok();
        Self {
            project: project.to_owned(),
            filename: Some(file.filename.clone()),
            version: parsed.as_ref().map(|parsed| parsed.version.clone()),
            package_type: parsed.as_ref().map(|parsed| PackageType::from(parsed.kind)),
            python_tag: parsed.as_ref().and_then(|parsed| parsed.python_tag.clone()),
            platform_tag: parsed.as_ref().and_then(|parsed| parsed.platform_tag.clone()),
            size: file.size,
        }
    }

    fn from_filename(filename: &str, size: Option<u64>) -> Self {
        let parsed = parse_distribution_filename(filename).ok();
        Self {
            project: parsed
                .as_ref()
                .map_or_else(|| "<unknown>".to_owned(), |parsed| parsed.normalized_name.clone()),
            filename: Some(filename.to_owned()),
            version: parsed.as_ref().map(|parsed| parsed.version.clone()),
            package_type: parsed.as_ref().map(|parsed| PackageType::from(parsed.kind)),
            python_tag: parsed.as_ref().and_then(|parsed| parsed.python_tag.clone()),
            platform_tag: parsed.as_ref().and_then(|parsed| parsed.platform_tag.clone()),
            size,
        }
    }

    fn denial(&self, action: PolicyAction, rule: &'static str, field: &'static str, reason: String) -> PolicyDenial {
        PolicyDenial::new(
            action,
            &self.project,
            self.filename.as_deref(),
            self.version.as_ref().map(ToString::to_string),
            rule,
            field,
            reason,
        )
    }
}

fn apply_project_size_limit(
    action: PolicyAction,
    project: &str,
    limit: u64,
    detail: &ProjectDetail,
) -> Result<(), PolicyDenial> {
    if let Some(denial) = project_size_denial(action, project, detail.files.iter(), limit) {
        return Err(denial);
    }
    Ok(())
}

fn project_size_denial<'a>(
    action: PolicyAction,
    project: &str,
    files: impl IntoIterator<Item = &'a File>,
    limit: u64,
) -> Option<PolicyDenial> {
    let mut total = 0_u64;
    for file in files {
        let Some(size) = file.size else {
            return Some(PolicyDenial::new(
                action,
                project,
                Some(&file.filename),
                None,
                "max-project-size",
                "size",
                format!(
                    "project size is unknown because file {:?} has no declared size",
                    file.filename
                ),
            ));
        };
        total = total.saturating_add(size);
    }
    (total > limit).then(|| {
        PolicyDenial::new(
            action,
            project,
            None,
            None,
            "max-project-size",
            "project_size",
            format!("project size {total} exceeds limit {limit}"),
        )
    })
}

fn retain_versions_with_files(detail: &mut ProjectDetail) {
    let versions = detail
        .files
        .iter()
        .filter_map(|file| parse_distribution_filename(&file.filename).ok())
        .map(|parsed| parsed.version.to_string())
        .collect::<BTreeSet<_>>();
    if versions.is_empty() {
        detail.versions.clear();
        return;
    }
    detail.versions.retain(|version| versions.contains(version));
    for version in versions {
        if !detail.versions.contains(&version) {
            detail.versions.push(version);
        }
    }
}

fn normalize_projects(projects: &[String]) -> HashSet<String> {
    projects.iter().map(|project| normalize_name(project)).collect()
}

fn package_mask(types: &[PackageType]) -> u8 {
    types.iter().fold(0, |mask, kind| mask | kind.mask())
}

fn tags(values: &[String]) -> Result<HashSet<String>, PolicyConfigError> {
    let mut tags = HashSet::with_capacity(values.len());
    for value in values {
        if value.is_empty() {
            return Err(PolicyConfigError::EmptyTag(value.clone()));
        }
        tags.insert(value.clone());
    }
    Ok(tags)
}

#[cfg(test)]
mod tests;
