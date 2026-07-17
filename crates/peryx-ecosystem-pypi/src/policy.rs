//! The `PyPI` half of the policy engine.
//!
//! [`peryx_policy::Policy`] enforces the ecosystem-neutral rules (project-name allow/block and byte
//! size). Everything a package format understands lives here: PEP 440 version specifiers, wheel and
//! sdist package types, wheel Python and platform tags, the config keys, the matchers compiled
//! into [`ArtifactRule`]s, and the mapping from `PyPI` `File`s and `ProjectDetail`s into neutral
//! [`ArtifactFacts`]. The neutral engine names no `PyPI` concept and pulls in no PEP 440 dependency.

use std::collections::{BTreeSet, HashSet};
use std::str::FromStr as _;
use std::sync::Arc;

use pep440_rs::{Version, VersionSpecifiers};
use peryx_policy::{ArtifactFacts, ArtifactRule, Policy, PolicyAction, PolicyDenial, retain_versions};
use serde::Deserialize;

use crate::{DistributionKind, File, ProjectDetail, ProjectList, normalize_name, parse_distribution_filename};

/// The `PyPI`-specific policy keys, parsed alongside the neutral [`peryx_policy::PolicyConfig`] and
/// compiled into [`ArtifactRule`]s with [`compile_rules`].
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PypiPolicyConfig {
    pub allow_versions: Option<String>,
    pub allow_package_types: Vec<PackageType>,
    pub block_package_types: Vec<PackageType>,
    pub allow_wheel_pythons: Vec<String>,
    pub block_wheel_pythons: Vec<String>,
    pub allow_wheel_platforms: Vec<String>,
    pub block_wheel_platforms: Vec<String>,
    pub min_release_age_secs: Option<u64>,
}

impl PypiPolicyConfig {
    /// The `[index.policy]` keys `PyPI` adds on top of the neutral set, so a config layer can reject a
    /// key that belongs to neither.
    pub const KEYS: &'static [&'static str] = &[
        "allow_versions",
        "allow_package_types",
        "block_package_types",
        "allow_wheel_pythons",
        "block_wheel_pythons",
        "allow_wheel_platforms",
        "block_wheel_platforms",
        "min_release_age_secs",
    ];
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

    fn parse(value: &str) -> Option<Self> {
        match value {
            "wheel" => Some(Self::Wheel),
            "sdist" => Some(Self::Sdist),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PypiPolicyError {
    #[error("invalid PEP 440 version specifier {0:?}")]
    VersionSpecifiers(String),
    #[error("policy tag {0:?} is empty")]
    EmptyTag(String),
}

/// Compile the `PyPI` policy keys into rules to attach to a neutral [`Policy`] via
/// [`Policy::with_rules`](peryx_policy::Policy::with_rules).
///
/// # Errors
/// Returns an error when a version specifier does not parse or a tag filter is empty.
pub fn compile_rules(config: &PypiPolicyConfig) -> Result<Vec<Arc<dyn ArtifactRule>>, PypiPolicyError> {
    let mut rules: Vec<Arc<dyn ArtifactRule>> = Vec::new();
    if let Some(specifier) = &config.allow_versions {
        let allowed = VersionSpecifiers::from_str(specifier)
            .map_err(|_| PypiPolicyError::VersionSpecifiers(specifier.clone()))?;
        rules.push(Arc::new(VersionRule { allowed }));
    }
    let allow = package_mask(&config.allow_package_types);
    let block = package_mask(&config.block_package_types);
    if allow != 0 || block != 0 {
        rules.push(Arc::new(PackageTypeRule { allow, block }));
    }
    push_wheel_tag_rule(
        &mut rules,
        WheelTagSpec {
            attribute: "python_tag",
            field: "wheel_python",
            allow_rule: "wheel-python-allow-list",
            block_rule: "wheel-python-block-list",
        },
        &config.allow_wheel_pythons,
        &config.block_wheel_pythons,
    )?;
    push_wheel_tag_rule(
        &mut rules,
        WheelTagSpec {
            attribute: "platform_tag",
            field: "wheel_platform",
            allow_rule: "wheel-platform-allow-list",
            block_rule: "wheel-platform-block-list",
        },
        &config.allow_wheel_platforms,
        &config.block_wheel_platforms,
    )?;
    if let Some(secs) = config.min_release_age_secs.filter(|secs| *secs > 0) {
        rules.push(Arc::new(ReleaseDelayRule {
            min_age_secs: i64::try_from(secs).unwrap_or(i64::MAX),
        }));
    }
    Ok(rules)
}

fn push_wheel_tag_rule(
    rules: &mut Vec<Arc<dyn ArtifactRule>>,
    spec: WheelTagSpec,
    allow: &[String],
    block: &[String],
) -> Result<(), PypiPolicyError> {
    let allow = tags(allow)?;
    let block = tags(block)?;
    if !allow.is_empty() || !block.is_empty() {
        rules.push(Arc::new(WheelTagRule { spec, allow, block }));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct WheelTagSpec {
    attribute: &'static str,
    field: &'static str,
    allow_rule: &'static str,
    block_rule: &'static str,
}

#[derive(Debug)]
struct VersionRule {
    allowed: VersionSpecifiers,
}

impl ArtifactRule for VersionRule {
    fn check(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        let Some(version) = &facts.version else {
            return Err(facts.denial(
                action,
                "version-specifier",
                "version",
                "file version is unknown".to_owned(),
            ));
        };
        let parsed =
            Version::from_str(version).expect("facts version is the string form of a parsed distribution version");
        if !self.allowed.contains(&parsed) {
            return Err(facts.denial(
                action,
                "version-specifier",
                "version",
                format!("version {version} is outside the allowed range"),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
struct PackageTypeRule {
    allow: u8,
    block: u8,
}

impl ArtifactRule for PackageTypeRule {
    fn check(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        let kind = facts.attribute("package_type").and_then(PackageType::parse);
        if self.allow != 0 {
            let Some(kind) = kind else {
                return Err(facts.denial(
                    action,
                    "package-type-allow-list",
                    "package_type",
                    "package type is unknown".to_owned(),
                ));
            };
            if self.allow & kind.mask() == 0 {
                return Err(facts.denial(
                    action,
                    "package-type-allow-list",
                    "package_type",
                    format!("package type {} is not allowed", kind.as_str()),
                ));
            }
        }
        if let Some(kind) = kind
            && self.block & kind.mask() != 0
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
}

#[derive(Debug)]
struct WheelTagRule {
    spec: WheelTagSpec,
    allow: HashSet<String>,
    block: HashSet<String>,
}

impl ArtifactRule for WheelTagRule {
    fn check(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        // Wheel tags only constrain wheels; an sdist carries none, so it passes.
        if facts.attribute("package_type") != Some(PackageType::Wheel.as_str()) {
            return Ok(());
        }
        let tag = facts.attribute(self.spec.attribute);
        let hits = |set: &HashSet<String>| tag.is_some_and(|value| value.split('.').any(|part| set.contains(part)));
        if !self.allow.is_empty() && !hits(&self.allow) {
            return Err(facts.denial(
                action,
                self.spec.allow_rule,
                self.spec.field,
                format!("wheel tag {tag:?} is not allowed", tag = tag.unwrap_or_default()),
            ));
        }
        if !self.block.is_empty() && hits(&self.block) {
            return Err(facts.denial(
                action,
                self.spec.block_rule,
                self.spec.field,
                format!("wheel tag {tag:?} is blocked", tag = tag.unwrap_or_default()),
            ));
        }
        Ok(())
    }
}

/// Quarantine a fresh upstream release: hide a file until it has aged past `min_age_secs`, the window
/// an operator wants before a new upload can be served, to blunt a malicious or mistaken release.
#[derive(Debug)]
struct ReleaseDelayRule {
    min_age_secs: i64,
}

impl ArtifactRule for ReleaseDelayRule {
    fn check(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        // A path with no clock (catalog indexing, an upload check) cannot age a release, so it passes;
        // the time-aware serve path supplies `now` and enforces the delay.
        let Some(now) = facts.now else { return Ok(()) };
        let Some(uploaded) = facts.upload_time else {
            return Err(facts.denial(
                action,
                "release-delay",
                "upload_time",
                "release has no upstream upload time to age against".to_owned(),
            ));
        };
        let age = now.saturating_sub(uploaded);
        if age < self.min_age_secs {
            return Err(facts.denial(
                action,
                "release-delay",
                "upload_time",
                format!(
                    "release is {age}s old, within the {}s upstream delay",
                    self.min_age_secs
                ),
            ));
        }
        Ok(())
    }
}

fn package_mask(types: &[PackageType]) -> u8 {
    types.iter().fold(0, |mask, kind| mask | kind.mask())
}

fn tags(values: &[String]) -> Result<HashSet<String>, PypiPolicyError> {
    let mut tags = HashSet::with_capacity(values.len());
    for value in values {
        if value.is_empty() {
            return Err(PypiPolicyError::EmptyTag(value.clone()));
        }
        tags.insert(value.clone());
    }
    Ok(tags)
}

/// Policy operations phrased in `PyPI` terms, implemented on the neutral [`Policy`].
pub trait PypiPolicy {
    /// Check whether one Simple-API file record is allowed.
    ///
    /// # Errors
    /// Returns a denial when the file's parsed facts match a configured policy rule.
    fn check_file(&self, action: PolicyAction, project: &str, file: &File) -> Result<(), PolicyDenial>;

    /// Check whether a direct artifact or metadata download is allowed.
    ///
    /// # Errors
    /// Returns a denial when the filename or known size matches a configured policy rule.
    fn check_download(&self, action: PolicyAction, filename: &str, size: Option<u64>) -> Result<(), PolicyDenial>;

    /// Filter a project detail response through this policy. `now` is the serve clock as a Unix
    /// timestamp, or `None` on a path with no clock (catalog indexing); a time-based rule such as the
    /// release-age delay only applies when it is supplied.
    ///
    /// # Errors
    /// Returns a denial when project-wide rules reject the whole response.
    fn apply_detail(
        &self,
        action: PolicyAction,
        project: &str,
        detail: ProjectDetail,
        now: Option<i64>,
    ) -> Result<ProjectDetail, PolicyDenial>;

    /// Filter a project list to the projects this policy allows.
    fn apply_list(&self, list: ProjectList) -> ProjectList;

    /// Every denial a project detail would raise, for dry-run reporting.
    fn preview_detail(&self, action: PolicyAction, detail: &ProjectDetail) -> Vec<PolicyDenial>;
}

impl PypiPolicy for Policy {
    fn check_file(&self, action: PolicyAction, project: &str, file: &File) -> Result<(), PolicyDenial> {
        self.check_facts(action, &facts_from_file(project, file))
    }

    fn check_download(&self, action: PolicyAction, filename: &str, size: Option<u64>) -> Result<(), PolicyDenial> {
        let artifact = filename.strip_suffix(".metadata").unwrap_or(filename);
        self.check_facts(action, &facts_from_filename(artifact, size))
    }

    fn apply_detail(
        &self,
        action: PolicyAction,
        project: &str,
        mut detail: ProjectDetail,
        now: Option<i64>,
    ) -> Result<ProjectDetail, PolicyDenial> {
        self.check_project(action, project)?;
        if !self.active() {
            return Ok(detail);
        }
        detail.files.retain(|file| {
            let mut facts = facts_from_file(project, file);
            facts.now = now;
            self.check_facts(action, &facts).is_ok()
        });
        if let Some(limit) = self.max_project_size() {
            apply_project_size_limit(action, project, limit, &detail)?;
        }
        retain_versions_with_files(&mut detail);
        Ok(detail)
    }

    fn apply_list(&self, list: ProjectList) -> ProjectList {
        if !self.active() {
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

    fn preview_detail(&self, action: PolicyAction, detail: &ProjectDetail) -> Vec<PolicyDenial> {
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
        if let Some(limit) = self.max_project_size()
            && let Some(denial) = project_size_denial(action, &detail.name, allowed, limit)
        {
            denials.push(denial);
        }
        denials
    }
}

const fn package_type_of(kind: DistributionKind) -> PackageType {
    match kind {
        DistributionKind::Wheel => PackageType::Wheel,
        DistributionKind::SdistTarGz | DistributionKind::SdistZip => PackageType::Sdist,
    }
}

fn facts_from_file(project: &str, file: &File) -> ArtifactFacts {
    let parsed = parse_distribution_filename(&file.filename).ok();
    ArtifactFacts {
        project: project.to_owned(),
        filename: Some(file.filename.clone()),
        version: parsed.as_ref().map(|parsed| parsed.version.to_string()),
        source: None,
        size: file.size,
        upload_time: file.upload_time.as_deref().and_then(parse_upload_time),
        now: None,
        attributes: parsed.as_ref().map(pypi_attributes).unwrap_or_default(),
    }
}

fn facts_from_filename(filename: &str, size: Option<u64>) -> ArtifactFacts {
    let parsed = parse_distribution_filename(filename).ok();
    ArtifactFacts {
        project: parsed
            .as_ref()
            .map_or_else(|| "<unknown>".to_owned(), |parsed| parsed.normalized_name.clone()),
        filename: Some(filename.to_owned()),
        version: parsed.as_ref().map(|parsed| parsed.version.to_string()),
        source: None,
        size,
        upload_time: None,
        now: None,
        attributes: parsed.as_ref().map(pypi_attributes).unwrap_or_default(),
    }
}

/// Parse a Simple-API `upload-time` (RFC 3339, per PEP 700) into a Unix timestamp. A value without an
/// offset, or otherwise unparseable, yields `None`, which the release-delay rule treats as a missing
/// upload time.
fn parse_upload_time(value: &str) -> Option<i64> {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(time::OffsetDateTime::unix_timestamp)
}

fn pypi_attributes(parsed: &crate::DistributionFilename) -> Vec<(&'static str, String)> {
    let mut attributes = vec![("package_type", package_type_of(parsed.kind).as_str().to_owned())];
    if let Some(python) = &parsed.python_tag {
        attributes.push(("python_tag", python.clone()));
    }
    if let Some(platform) = &parsed.platform_tag {
        attributes.push(("platform_tag", platform.clone()));
    }
    attributes
}

fn apply_project_size_limit(
    action: PolicyAction,
    project: &str,
    limit: u64,
    detail: &ProjectDetail,
) -> Result<(), PolicyDenial> {
    project_size_denial(action, project, detail.files.iter(), limit).map_or(Ok(()), Err)
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
    retain_versions(&mut detail.versions, versions);
}

#[cfg(test)]
mod tests {
    use super::PackageType;

    #[test]
    fn test_package_type_parse_rejects_an_unknown_value() {
        assert_eq!(PackageType::parse("wheel"), Some(PackageType::Wheel));
        assert_eq!(PackageType::parse("sdist"), Some(PackageType::Sdist));
        assert_eq!(PackageType::parse("egg"), None);
    }
}
