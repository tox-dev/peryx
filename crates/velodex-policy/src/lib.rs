//! Repository policy checks compiled from configuration.
//!
//! The engine is ecosystem-neutral: it never names a package format. Callers turn one artifact into a
//! neutral [`FileFacts`] (project, version, package type, wheel tags, size) and ask [`Policy`] whether
//! the configured rules allow it. The `PyPI` mapping from Simple-API records to facts (and the
//! detail/list filtering built on it) lives in `velodex-ecosystem-pypi`, so this crate carries no
//! format dependency.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::str::FromStr as _;

use pep440_rs::{Version, VersionSpecifiers};
use serde::{Deserialize, Serialize};

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
    allow_versions: Option<VersionSpecifiers>,
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
                VersionSpecifiers::from_str(value).map_err(|_| PolicyConfigError::VersionSpecifiers(value.to_owned()))
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

    /// The configured per-project size limit, if any.
    #[must_use]
    pub const fn max_project_size(&self) -> Option<u64> {
        self.max_project_size_bytes
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

    /// Check one artifact's neutral [`FileFacts`] against every configured rule.
    ///
    /// # Errors
    /// Returns a denial when the facts match a configured policy rule.
    pub fn check_facts(&self, action: PolicyAction, facts: &FileFacts) -> Result<(), PolicyDenial> {
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
    /// Build a denial. Ecosystem mappers construct these when a format-specific check (a project-wide
    /// size total, say) fails outside [`Policy::check_facts`].
    #[must_use]
    pub fn new(
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

/// The neutral facts one artifact contributes to a policy decision.
///
/// Ecosystem code fills these from its own records (a `PyPI` Simple-API file, a wheel/sdist filename)
/// so [`Policy`] never sees a format type.
#[derive(Debug, Clone)]
pub struct FileFacts {
    pub project: String,
    pub filename: Option<String>,
    pub version: Option<Version>,
    pub package_type: Option<PackageType>,
    pub python_tag: Option<String>,
    pub platform_tag: Option<String>,
    pub size: Option<u64>,
}

impl FileFacts {
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

/// Normalize a project name per PEP 503 (lowercase, collapse runs of `-`, `_`, `.` to a single `-`).
/// Mirrors `velodex-ecosystem-pypi`'s `normalize_name`; kept local so the policy engine carries no
/// format dependency. The rule is fixed by PEP 503, so the two cannot drift.
fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut in_separator = false;
    for ch in name.chars() {
        if matches!(ch, '-' | '_' | '.') {
            if !in_separator {
                out.push('-');
                in_separator = true;
            }
        } else {
            in_separator = false;
            out.extend(ch.to_lowercase());
        }
    }
    out
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

/// Retain from `versions` only those present in `keep`, appending any missing ones.
///
/// This keeps a project's version list matching the files that survived filtering; `keep` is the set
/// of versions whose files remain. Exposed for ecosystem mappers that filter a detail response.
pub fn retain_versions(versions: &mut Vec<String>, keep: BTreeSet<String>) {
    if keep.is_empty() {
        versions.clear();
        return;
    }
    versions.retain(|version| keep.contains(version));
    for version in keep {
        if !versions.contains(&version) {
            versions.push(version);
        }
    }
}
