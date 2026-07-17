//! Ecosystem-neutral index policy engine.
//!
//! The engine enforces the rules every ecosystem shares (project-name allow/block lists and byte-size
//! limits) and runs any format-specific rules a driver supplies as [`ArtifactRule`] trait objects. A
//! matcher that understands a package format (a `PyPI` version specifier, a wheel tag) is implemented
//! in that ecosystem's crate and attached through [`Policy::with_rules`], so this crate names no
//! package format and depends on no format library.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// The ecosystem-neutral policy keys. A driver parses its own format-specific keys separately and
/// compiles them into [`ArtifactRule`]s.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    pub allow_projects: Vec<String>,
    pub block_projects: Vec<String>,
    pub protected_names: Vec<String>,
    pub max_file_size_bytes: Option<u64>,
    pub max_project_size_bytes: Option<u64>,
}

impl PolicyConfig {
    /// The TOML keys this neutral config claims, for a caller that splits one policy table across the
    /// neutral engine and an ecosystem's own keys and rejects the rest.
    pub const KEYS: &'static [&'static str] = &[
        "allow_projects",
        "block_projects",
        "protected_names",
        "max_file_size_bytes",
        "max_project_size_bytes",
    ];
}

/// One artifact's neutral facts, filled by ecosystem code and matched by [`Policy`] and its rules.
///
/// The core fields (project, size) drive the neutral rules; `source` identifies the routed input when
/// known, `version` is a plain string a rule may parse in its own format, and `attributes` carries any
/// extra format-specific values as named strings so the engine never sees a format type.
#[derive(Debug, Clone, Default)]
pub struct ArtifactFacts {
    pub project: String,
    pub filename: Option<String>,
    pub version: Option<String>,
    pub source: Option<String>,
    pub size: Option<u64>,
    /// The artifact's upstream publish time as a Unix timestamp, when the source declares one. A rule
    /// that ages a release (a supply-chain quarantine) reads it; `None` means the source gave no time.
    pub upload_time: Option<i64>,
    /// The evaluation clock as a Unix timestamp, supplied by a time-aware serve path. A rule that needs
    /// wall-clock time reads it; `None` means this path does not evaluate against a clock, so such a
    /// rule passes rather than guess.
    pub now: Option<i64>,
    pub attributes: Vec<(&'static str, String)>,
}

impl ArtifactFacts {
    /// The value of a named format-specific attribute, if the fact carries it.
    #[must_use]
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find_map(|(name, value)| (*name == key).then_some(value.as_str()))
    }

    /// Build a denial carrying this artifact's project, filename, and version context. Ecosystem rules
    /// use it so their denials read like the engine's own.
    #[must_use]
    pub fn denial(
        &self,
        action: PolicyAction,
        rule: &'static str,
        field: &'static str,
        reason: String,
    ) -> PolicyDenial {
        PolicyDenial::new(
            action,
            &self.project,
            self.filename.as_deref(),
            self.version.clone(),
            rule,
            field,
            reason,
        )
    }
}

/// A format-specific policy rule an ecosystem crate implements and attaches to a [`Policy`].
pub trait ArtifactRule: Send + Sync + fmt::Debug {
    /// Check one artifact's facts, returning a denial when they violate this rule.
    ///
    /// # Errors
    /// Returns a [`PolicyDenial`] when the facts match this rule's block criteria or miss its allow
    /// criteria.
    fn check(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial>;
}

/// The result retained for one policy subject and action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecisionState {
    Allow,
    Deny,
    Wait,
}

/// One completed policy evaluation, borrowed for the duration of a recorder call.
#[derive(Debug, Clone, Copy)]
pub struct PolicyEvaluation<'a> {
    pub action: PolicyAction,
    pub project: &'a str,
    pub filename: Option<&'a str>,
    pub version: Option<&'a str>,
    pub source: Option<&'a str>,
    pub state: PolicyDecisionState,
    pub rule: Option<&'static str>,
    pub reason: Option<&'a str>,
    pub next_eligible_at_unix: Option<i64>,
}

/// A synchronous audit sink attached by the runtime after it opens metadata storage.
pub trait PolicyDecisionRecorder: Send + Sync + fmt::Debug {
    fn record(&self, evaluation: PolicyEvaluation<'_>);
}

/// Names an operator reserves so a request for them never falls back to an upstream mirror. This is
/// the dependency-confusion defense phrased as policy: a private project name resolves only from a
/// hosted member, never from the public index that a typo, a deletion, or a rename would otherwise
/// let answer it.
///
/// An entry is an exact normalized name (`acme-secrets`) or a namespace prefix ending in `*`
/// (`acme-*`), so one rule can reserve a whole naming convention. Both are normalized through the
/// ecosystem's own function, so `-`, `_`, and `.` spellings collapse the same way the incoming name
/// does before it is compared.
#[derive(Clone, Default, Debug)]
struct ProtectedNames {
    exact: BTreeSet<String>,
    prefixes: BTreeSet<String>,
}

impl ProtectedNames {
    fn compile(names: &[String], normalize: &impl Fn(&str) -> String) -> Self {
        let mut exact = BTreeSet::new();
        let mut prefixes = BTreeSet::new();
        for name in names {
            let prefix = name.strip_suffix('*');
            let normalized = normalize(prefix.unwrap_or(name));
            if prefix.is_some() {
                prefixes.insert(normalized);
            } else {
                exact.insert(normalized);
            }
        }
        Self { exact, prefixes }
    }

    fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.prefixes.is_empty()
    }

    /// The rule that reserves `project`, formatted for a denial, or `None` when the name is free to
    /// fall back upstream.
    fn matched(&self, project: &str) -> Option<String> {
        if self.exact.contains(project) {
            return Some(project.to_owned());
        }
        self.prefixes
            .iter()
            .find(|prefix| project.starts_with(prefix.as_str()))
            .map(|prefix| format!("{prefix}*"))
    }
}

#[derive(Clone, Default, Debug)]
pub struct Policy {
    allow_projects: HashSet<String>,
    block_projects: HashSet<String>,
    protected_names: ProtectedNames,
    max_file_size_bytes: Option<u64>,
    max_project_size_bytes: Option<u64>,
    rules: Vec<Arc<dyn ArtifactRule>>,
    recorder: Option<Arc<dyn PolicyDecisionRecorder>>,
    active: bool,
}

impl Policy {
    /// Compile the neutral operator configuration once at startup. Format-specific rules are attached
    /// afterward with [`Policy::with_rules`].
    ///
    /// `normalize` folds a configured project key into the form the ecosystem checks against. `PyPI`
    /// applies [PEP 503](https://peps.python.org/pep-0503/) normalization; `OCI` leaves a repository
    /// name untouched. The same function must key the incoming name at check time, so the engine holds
    /// no format assumption of its own. Pass the identity closure for a case-sensitive match.
    #[must_use]
    pub fn compile(config: &PolicyConfig, normalize: impl Fn(&str) -> String) -> Self {
        let normalize_all = |names: &[String]| names.iter().map(|name| normalize(name)).collect();
        let policy = Self {
            allow_projects: normalize_all(&config.allow_projects),
            block_projects: normalize_all(&config.block_projects),
            protected_names: ProtectedNames::compile(&config.protected_names, &normalize),
            max_file_size_bytes: config.max_file_size_bytes,
            max_project_size_bytes: config.max_project_size_bytes,
            rules: Vec::new(),
            recorder: None,
            active: false,
        };
        Self {
            active: policy.compute_active(),
            ..policy
        }
    }

    /// Attach an ecosystem's compiled format-specific rules.
    #[must_use]
    pub fn with_rules(mut self, rules: Vec<Arc<dyn ArtifactRule>>) -> Self {
        self.rules = rules;
        self.active = self.compute_active();
        self
    }

    /// Attach the runtime's durable decision recorder.
    #[must_use]
    pub fn with_decision_recorder(mut self, recorder: Arc<dyn PolicyDecisionRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// The configured per-file size limit, if any.
    #[must_use]
    pub const fn max_file_size(&self) -> Option<u64> {
        self.max_file_size_bytes
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

    fn compute_active(&self) -> bool {
        !self.allow_projects.is_empty()
            || !self.block_projects.is_empty()
            || !self.protected_names.is_empty()
            || self.max_file_size_bytes.is_some()
            || self.max_project_size_bytes.is_some()
            || !self.rules.is_empty()
    }

    #[must_use]
    pub const fn active(&self) -> bool {
        self.active
    }

    /// Check whether a project name is allowed.
    ///
    /// A [protected name](ProtectedNames) is denied only for [`PolicyAction::Cached`], the upstream
    /// mirror path: a hosted member may still serve and accept uploads for it, but a request the local
    /// members cannot satisfy is refused rather than answered from the public index.
    ///
    /// # Errors
    /// Returns a denial when the project misses an allow list, matches a block list, or is protected
    /// from upstream fallback.
    pub fn check_project(&self, action: PolicyAction, project: &str) -> Result<(), PolicyDenial> {
        let result = self.evaluate_project(action, project);
        self.record(action, project, None, None, None, &result);
        result
    }

    fn evaluate_project(&self, action: PolicyAction, project: &str) -> Result<(), PolicyDenial> {
        if action == PolicyAction::Cached
            && let Some(rule) = self.protected_names.matched(project)
        {
            return Err(PolicyDenial::new(
                action,
                project,
                None,
                None,
                "protected-name",
                "project",
                format!("project {project:?} is protected from upstream fallback by rule {rule:?}"),
            ));
        }
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

    /// Check an artifact's project, byte size, and every attached format-specific rule.
    ///
    /// # Errors
    /// Returns a denial when the facts match a configured policy rule.
    pub fn check_facts(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        let result = self.evaluate_facts(action, facts);
        self.record(
            action,
            &facts.project,
            facts.filename.as_deref(),
            facts.version.as_deref(),
            facts.source.as_deref(),
            &result,
        );
        result
    }

    /// Check an upload's project name and byte size: the neutral rules an ecosystem with no
    /// format-specific facts (an OCI blob or manifest) enforces without building full facts.
    ///
    /// # Errors
    /// Returns a denial when the project is disallowed or the size exceeds `max_file_size_bytes`.
    pub fn check_size(&self, action: PolicyAction, project: &str, size: u64) -> Result<(), PolicyDenial> {
        let result = self.evaluate_size(action, project, size);
        self.record(action, project, None, None, None, &result);
        result
    }

    fn evaluate_facts(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        self.evaluate_project(action, &facts.project)?;
        self.check_file_size(action, facts)?;
        for rule in &self.rules {
            rule.check(action, facts)?;
        }
        Ok(())
    }

    fn evaluate_size(&self, action: PolicyAction, project: &str, size: u64) -> Result<(), PolicyDenial> {
        self.evaluate_project(action, project)?;
        if let Some(limit) = self.max_file_size_bytes
            && size > limit
        {
            return Err(PolicyDenial::new(
                action,
                project,
                None,
                None,
                "max-file-size",
                "size",
                format!("file size {size} exceeds limit {limit}"),
            ));
        }
        Ok(())
    }

    fn record(
        &self,
        action: PolicyAction,
        project: &str,
        filename: Option<&str>,
        version: Option<&str>,
        source: Option<&str>,
        result: &Result<(), PolicyDenial>,
    ) {
        let Some(recorder) = &self.recorder else {
            return;
        };
        let (state, rule, reason) = match result {
            Ok(()) => (PolicyDecisionState::Allow, None, None),
            Err(denial) => (
                PolicyDecisionState::Deny,
                Some(denial.rule),
                Some(denial.reason.as_ref()),
            ),
        };
        recorder.record(PolicyEvaluation {
            action,
            project,
            filename,
            version,
            source,
            state,
            rule,
            reason,
            next_eligible_at_unix: None,
        });
    }

    fn check_file_size(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Build a denial. Ecosystem rules and mappers construct these when a check fails.
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
