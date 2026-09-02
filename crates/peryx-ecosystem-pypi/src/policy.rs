//! Maps `PyPI` policy config and metadata into neutral facts and rules.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::str::FromStr as _;
use std::sync::Arc;

use pep440_rs::{Version, VersionSpecifiers};
use peryx_policy::{
    ArtifactFacts, ArtifactRule, Policy, PolicyAction, PolicyCapabilities, PolicyDenial, PolicyLimits, ResourceRule,
};
use serde::Deserialize;

use crate::{DistributionKind, File, ProjectDetail, ProjectList, normalize_name, parse_distribution_filename};

/// The `PyPI`-specific policy keys, parsed alongside the neutral [`peryx_policy::PolicyConfig`] and
/// compiled into [`ArtifactRule`]s with [`compile_capabilities`].
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PypiPolicyConfig {
    pub fallback_mode: FallbackMode,
    pub upstream_attestations: RemoteMetadataMode,
    pub allow_projects: Vec<String>,
    pub block_projects: Vec<String>,
    pub protected_names: Vec<String>,
    pub max_file_size_bytes: Option<u64>,
    pub max_project_size_bytes: Option<u64>,
    pub max_projects: Option<u64>,
    pub max_versions_per_project: Option<u64>,
    pub allow_versions: Option<String>,
    pub allow_package_types: Vec<PackageType>,
    pub block_package_types: Vec<PackageType>,
    pub allow_wheel_pythons: Vec<String>,
    pub block_wheel_pythons: Vec<String>,
    pub allow_wheel_platforms: Vec<String>,
    pub block_wheel_platforms: Vec<String>,
    pub min_release_age_secs: Option<u64>,
    /// The in-toto predicate types an upload must carry a PEP 740 attestation for. Empty leaves
    /// uploads unconstrained; any entry turns the [`AttestationMode`] rule on.
    pub required_attestations: Vec<String>,
    pub attestation_mode: AttestationMode,
}

impl PypiPolicyConfig {
    /// The `[index.policy]` keys `PyPI` adds on top of the neutral set, so a config layer can reject a
    /// key that belongs to neither.
    pub const KEYS: &'static [&'static str] = &[
        "fallback_mode",
        "upstream_attestations",
        "allow_projects",
        "block_projects",
        "protected_names",
        "max_file_size_bytes",
        "max_project_size_bytes",
        "max_projects",
        "max_versions_per_project",
        "allow_versions",
        "allow_package_types",
        "block_package_types",
        "allow_wheel_pythons",
        "block_wheel_pythons",
        "allow_wheel_platforms",
        "block_wheel_platforms",
        "min_release_age_secs",
        "required_attestations",
        "attestation_mode",
    ];
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FallbackMode {
    #[default]
    Fallback,
    PrivateFirst,
    NoFallback,
}

impl FallbackMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::PrivateFirst => "private-first",
            Self::NoFallback => "no-fallback",
        }
    }
}

impl std::fmt::Display for FallbackMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteMetadataMode {
    #[default]
    Direct,
    Proxy,
    Cache,
}

impl RemoteMetadataMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Proxy => "proxy",
            Self::Cache => "cache",
        }
    }
}

/// Whether an unmet required-attestation rule blocks the upload or only records what it would block.
///
/// Each mode carries its own denial rule name through `AttestationMode::rule_name`, which reaches the
/// upload handler through the persisted decision, so the handler tells an audit observation from an
/// enforced rejection.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttestationMode {
    /// Reject an upload that is missing a required predicate type.
    #[default]
    Enforce,
    /// Record the unmet requirement but publish the upload anyway.
    Audit,
}

/// The denial rule an enforcing required-attestation policy raises.
pub const REQUIRED_ATTESTATION_RULE: &str = "required-attestation";

/// The denial rule an auditing required-attestation policy raises; the upload handler treats it as a
/// recorded observation rather than a rejection.
pub const REQUIRED_ATTESTATION_AUDIT_RULE: &str = "required-attestation-audit";

impl AttestationMode {
    const fn rule_name(self) -> &'static str {
        match self {
            Self::Enforce => REQUIRED_ATTESTATION_RULE,
            Self::Audit => REQUIRED_ATTESTATION_AUDIT_RULE,
        }
    }
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
    #[error("required attestation predicate type is empty")]
    EmptyPredicateType,
}

/// # Errors
/// Returns an error when a version specifier does not parse or a tag filter is empty.
pub fn compile_capabilities(config: &PypiPolicyConfig) -> Result<PolicyCapabilities, PypiPolicyError> {
    let mut capabilities = PolicyCapabilities::default().with_limits(PolicyLimits {
        max_artifact_bytes: config.max_file_size_bytes,
        max_resource_bytes: config.max_project_size_bytes,
        max_accounted_bytes: None,
        max_resources: config.max_projects,
        max_groups_per_resource: config.max_versions_per_project,
    });
    if config.fallback_mode != FallbackMode::default() {
        capabilities = capabilities
            .with_owner_setting("pypi.fallback-mode", config.fallback_mode.as_str())
            .with_policy_activation();
    }
    if config.upstream_attestations != RemoteMetadataMode::default() {
        capabilities =
            capabilities.with_owner_setting("pypi.remote-metadata-mode", config.upstream_attestations.as_str());
    }
    if !config.allow_projects.is_empty() || !config.block_projects.is_empty() || !config.protected_names.is_empty() {
        capabilities = capabilities.with_resource_rules(vec![Arc::new(PypiProjectRule::new(config))]);
    }
    let mut rules: Vec<Arc<dyn ArtifactRule>> = Vec::new();
    if let Some(specifier) = &config.allow_versions {
        let Ok(allowed) = VersionSpecifiers::from_str(specifier) else {
            return Err(PypiPolicyError::VersionSpecifiers(specifier.clone()));
        };
        // The same specifier reaches a declared version string through `VersionAdmission`, which has
        // no file to build artifact facts from.
        capabilities = capabilities.with_owner_setting(ALLOW_VERSIONS_SETTING, specifier);
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
    // The attestation rule runs last so a distribution rejected on filename, size, or a tag reports
    // that structural denial, and the requirement applies only to a file that would otherwise publish.
    if !config.required_attestations.is_empty() {
        let mut required = BTreeSet::new();
        for predicate_type in &config.required_attestations {
            if predicate_type.is_empty() {
                return Err(PypiPolicyError::EmptyPredicateType);
            }
            required.insert(predicate_type.clone());
        }
        rules.push(Arc::new(RequiredAttestationRule {
            required,
            mode: config.attestation_mode,
        }));
    }
    Ok(capabilities.with_artifact_rules(rules))
}

#[derive(Debug)]
struct PypiProjectRule {
    allow_projects: HashSet<String>,
    block_projects: HashSet<String>,
    protected_names: Vec<String>,
}

impl PypiProjectRule {
    fn new(config: &PypiPolicyConfig) -> Self {
        Self {
            allow_projects: config.allow_projects.iter().map(|name| normalize_name(name)).collect(),
            block_projects: config.block_projects.iter().map(|name| normalize_name(name)).collect(),
            protected_names: config.protected_names.iter().map(|name| normalize_name(name)).collect(),
        }
    }

    fn protected(&self, project: &str) -> bool {
        self.protected_names.iter().any(|rule| {
            rule.strip_suffix('*')
                .map_or_else(|| rule == project, |prefix| project.starts_with(prefix))
        })
    }
}

impl ResourceRule for PypiProjectRule {
    fn check(&self, action: PolicyAction, project: &str) -> Result<(), PolicyDenial> {
        let denial = if action == PolicyAction::Cached && self.protected(project) {
            Some((
                "protected-name",
                format!("project {project:?} is protected from upstream fallback"),
            ))
        } else if !self.allow_projects.is_empty() && !self.allow_projects.contains(project) {
            Some((
                "project-allow-list",
                format!("project {project:?} is not in the allow list"),
            ))
        } else if self.block_projects.contains(project) {
            Some(("project-block-list", format!("project {project:?} is blocked")))
        } else {
            None
        };
        denial.map_or(Ok(()), |(rule, reason)| {
            Err(PolicyDenial::new(action, project, None, None, rule, "project", reason))
        })
    }
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

/// Where [`VersionAdmission`] reads the configured version specifier back from.
const ALLOW_VERSIONS_SETTING: &str = "pypi.allow-versions";

/// Whether policy admits a release version named on its own.
///
/// [`VersionRule`] judges a file, and builds its version from the parsed filename. A `versions` entry
/// has no file behind it: it arrives as upstream text that may not parse as a version at all, and
/// routing it through [`ArtifactFacts`] would also subject it to the size and package-type rules,
/// which have nothing to say about a release. This reads the one rule that does.
#[derive(Debug, Clone, Default)]
pub struct VersionAdmission {
    allowed: Option<VersionSpecifiers>,
}

impl VersionAdmission {
    #[must_use]
    pub fn of(policy: &Policy) -> Self {
        Self {
            allowed: policy
                .owner_setting(ALLOW_VERSIONS_SETTING)
                .and_then(|specifier| VersionSpecifiers::from_str(specifier).ok()),
        }
    }

    /// With no specifier configured every declared version stands, which keeps the upstream set
    /// intact when no rule needs to read it. With one, a version peryx cannot parse cannot be shown
    /// to satisfy it, so it is not listed.
    #[must_use]
    pub fn admits(&self, version: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|allowed| Version::from_str(version).is_ok_and(|version| allowed.contains(&version)))
    }
}

/// The `versions` a policy-filtered project detail lists, and the only place either serving path
/// decides it.
///
/// The Simple Repository API requires every served file to belong to a listed version and permits a
/// listed version to carry no files. So the set follows the declared releases through version policy
/// rather than the surviving filenames: filtering artifacts leaves an allowed release listed even
/// when it loses every file, while a release policy denies disappears from both halves.
///
/// `declared` is what upstream listed and `local` what this index published itself; both face the
/// same check, so a locally published version policy denies is no more listed than an upstream one.
/// `served` carries the releases of the files that survived, which is what keeps a file whose release
/// upstream failed to declare from being served under no listed version at all. Those files already
/// passed the artifact rules, version rule included, so they need no second check.
#[must_use]
pub fn listed_versions(
    admission: &VersionAdmission,
    declared: impl IntoIterator<Item = String>,
    local: impl IntoIterator<Item = String>,
    served: impl IntoIterator<Item = String>,
) -> BTreeSet<String> {
    let mut listed: BTreeSet<String> = declared
        .into_iter()
        .chain(local)
        .filter(|version| admission.admits(version))
        .collect();
    listed.extend(served);
    listed
}

/// The release a served file belongs to, as its filename gives it.
///
/// A filename peryx cannot parse names no release, and the legacy egg is the standing example peryx
/// serves anyway, so it contributes nothing rather than being withheld.
#[must_use]
pub fn served_version(filename: &str) -> Option<String> {
    parse_distribution_filename(filename)
        .ok()
        .map(|parsed| parsed.version.to_string())
}

#[derive(Debug)]
struct VersionRule {
    allowed: VersionSpecifiers,
}

impl ArtifactRule for VersionRule {
    fn check(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        let Some(version) = &facts.group else {
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

/// The facts attribute the upload path sets to the newline-joined predicate types an upload carries.
/// Only the upload boundary supplies it, so serve, catalog, and offline-audit facts lack it and the
/// requirement passes there. An empty value still marks an upload the rule judges.
const ATTESTATION_PREDICATE_TYPES: &str = "attestation_predicate_types";

/// Require every configured in-toto predicate type to appear among an upload's bound attestations.
/// The rule reads the upload's declared types from [`ATTESTATION_PREDICATE_TYPES`]; a fact without
/// that attribute is not an upload the rule can judge and passes.
#[derive(Debug)]
struct RequiredAttestationRule {
    required: BTreeSet<String>,
    mode: AttestationMode,
}

impl ArtifactRule for RequiredAttestationRule {
    fn check(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        let Some(declared) = facts.attribute(ATTESTATION_PREDICATE_TYPES) else {
            return Ok(());
        };
        let present: HashSet<&str> = declared.split('\n').filter(|part| !part.is_empty()).collect();
        let missing = self
            .required
            .iter()
            .filter(|predicate_type| !present.contains(predicate_type.as_str()))
            .map(String::as_str)
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(());
        }
        Err(facts.denial(
            action,
            self.mode.rule_name(),
            "attestations",
            format!(
                "upload is missing a required attestation predicate type: {}",
                missing.join(", ")
            ),
        ))
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

/// What a policy withholds from one project page, judged against the whole file set so a release-wide
/// rule such as the project size limit sees the siblings that would reach a client together.
///
/// [`PypiPolicy::apply_detail`] drops what this refuses. A caller that must explain a refusal rather
/// than silently drop the file reads the denials instead.
#[derive(Debug, Default)]
pub struct Admission {
    /// The denial that withdraws the whole project: a resource rule, or a release-wide rule the
    /// admitted files violate together.
    pub project: Option<PolicyDenial>,
    /// Each refused file's denial, keyed by filename.
    pub files: BTreeMap<String, PolicyDenial>,
}

/// Policy operations phrased in `PyPI` terms, implemented on the neutral [`Policy`].
pub trait PypiPolicy {
    fn fallback_mode(&self) -> FallbackMode;
    fn remote_metadata_mode(&self) -> RemoteMetadataMode;
    /// # Errors
    /// Returns a denial when the file's parsed facts match a configured policy rule.
    fn check_file(&self, action: PolicyAction, project: &str, file: &File) -> Result<(), PolicyDenial>;

    /// Check whether a hosted upload is allowed, judging the neutral and `PyPI` file rules together
    /// with the required-attestation rule against `predicate_types`, the in-toto predicate types the
    /// upload's bound attestations declare.
    ///
    /// # Errors
    /// Returns a denial when the file's facts or its attestations match a configured policy rule.
    fn check_upload(
        &self,
        action: PolicyAction,
        project: &str,
        file: &File,
        predicate_types: &BTreeSet<String>,
    ) -> Result<(), PolicyDenial>;

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

    /// The refusals `action` raises over `detail` without applying them, so a caller can report why a
    /// file is withheld. `now` is the serve clock, as in [`apply_detail`](Self::apply_detail).
    fn admit_detail(&self, action: PolicyAction, project: &str, detail: &ProjectDetail, now: Option<i64>) -> Admission;

    fn apply_list(&self, list: ProjectList) -> ProjectList;

    fn preview_detail(&self, action: PolicyAction, detail: &ProjectDetail) -> Vec<PolicyDenial>;
}

impl PypiPolicy for Policy {
    fn fallback_mode(&self) -> FallbackMode {
        match self.owner_setting("pypi.fallback-mode") {
            Some("private-first") => FallbackMode::PrivateFirst,
            Some("no-fallback") => FallbackMode::NoFallback,
            _ => FallbackMode::Fallback,
        }
    }

    fn remote_metadata_mode(&self) -> RemoteMetadataMode {
        match self.owner_setting("pypi.remote-metadata-mode") {
            Some("proxy") => RemoteMetadataMode::Proxy,
            Some("cache") => RemoteMetadataMode::Cache,
            _ => RemoteMetadataMode::Direct,
        }
    }

    fn check_file(&self, action: PolicyAction, project: &str, file: &File) -> Result<(), PolicyDenial> {
        self.check_facts(action, &facts_from_file(project, file))
    }

    fn check_upload(
        &self,
        action: PolicyAction,
        project: &str,
        file: &File,
        predicate_types: &BTreeSet<String>,
    ) -> Result<(), PolicyDenial> {
        self.check_facts(action, &facts_from_upload(project, file, predicate_types))
    }

    fn check_download(&self, action: PolicyAction, filename: &str, size: Option<u64>) -> Result<(), PolicyDenial> {
        let artifact = filename
            .strip_suffix(".metadata")
            .or_else(|| filename.strip_suffix(crate::attestation::PROVENANCE_SUFFIX))
            .unwrap_or(filename);
        self.check_facts(action, &facts_from_filename(artifact, size))
    }

    fn apply_detail(
        &self,
        action: PolicyAction,
        project: &str,
        mut detail: ProjectDetail,
        now: Option<i64>,
    ) -> Result<ProjectDetail, PolicyDenial> {
        let admission = self.admit_detail(action, project, &detail, now);
        if let Some(denial) = admission.project {
            return Err(denial);
        }
        if !self.active() {
            return Ok(detail);
        }
        let declared = std::mem::take(&mut detail.versions);
        detail
            .files
            .retain(|file| !admission.files.contains_key(&file.filename));
        let served = detail.files.iter().filter_map(|file| served_version(&file.filename));
        detail.versions = listed_versions(&VersionAdmission::of(self), declared, [], served)
            .into_iter()
            .collect();
        Ok(detail)
    }

    fn admit_detail(&self, action: PolicyAction, project: &str, detail: &ProjectDetail, now: Option<i64>) -> Admission {
        if let Err(denial) = self.check_resource(action, project) {
            return Admission {
                project: Some(denial),
                files: BTreeMap::new(),
            };
        }
        if !self.active() {
            return Admission::default();
        }
        let mut files = BTreeMap::new();
        let mut admitted = Vec::new();
        for file in &detail.files {
            let mut facts = facts_from_file(project, file);
            facts.now = now;
            match self.check_facts(action, &facts) {
                Ok(()) => admitted.push(file),
                Err(denial) => {
                    files.insert(file.filename.clone(), denial);
                }
            }
        }
        let project = self
            .max_resource_size()
            .and_then(|limit| project_size_denial(action, project, admitted, limit));
        Admission { project, files }
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
                    self.check_resource(PolicyAction::Serve, &normalize_name(&entry.name))
                        .is_ok()
                })
                .collect(),
        }
    }

    fn preview_detail(&self, action: PolicyAction, detail: &ProjectDetail) -> Vec<PolicyDenial> {
        let admission = self.admit_detail(action, &detail.name, detail, None);
        admission.files.into_values().chain(admission.project).collect()
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
        resource: project.to_owned(),
        artifact: Some(file.filename.clone()),
        group: parsed.as_ref().map(|parsed| parsed.version.to_string()),
        source: None,
        size: file.size,
        upload_time: file.upload_time.as_deref().and_then(parse_upload_time),
        now: None,
        attributes: parsed.as_ref().map(pypi_attributes).unwrap_or_default(),
    }
}

/// Build upload facts that also carry the attestation predicate types the required-attestation rule
/// judges. This path always sets the attribute, even for an empty set, so the rule tells an upload
/// with no attestations from a serve fact it must not judge.
fn facts_from_upload(project: &str, file: &File, predicate_types: &BTreeSet<String>) -> ArtifactFacts {
    let mut facts = facts_from_file(project, file);
    facts.attributes.push((
        ATTESTATION_PREDICATE_TYPES,
        predicate_types
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    facts
}

fn facts_from_filename(filename: &str, size: Option<u64>) -> ArtifactFacts {
    let parsed = parse_distribution_filename(filename).ok();
    ArtifactFacts {
        resource: parsed
            .as_ref()
            .map_or_else(|| "<unknown>".to_owned(), |parsed| parsed.normalized_name.clone()),
        artifact: Some(filename.to_owned()),
        group: parsed.as_ref().map(|parsed| parsed.version.to_string()),
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
pub(crate) fn parse_upload_time(value: &str) -> Option<i64> {
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

#[cfg(test)]
#[path = "../tests/unit/policy/tests.rs"]
mod tests;
