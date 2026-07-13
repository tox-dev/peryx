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
    pub max_file_size_bytes: Option<u64>,
    pub max_project_size_bytes: Option<u64>,
}

impl PolicyConfig {
    /// The TOML keys this neutral config claims, for a caller that splits one policy table across the
    /// neutral engine and an ecosystem's own keys and rejects the rest.
    pub const KEYS: &'static [&'static str] = &[
        "allow_projects",
        "block_projects",
        "max_file_size_bytes",
        "max_project_size_bytes",
    ];
}

/// One artifact's neutral facts, filled by ecosystem code and matched by [`Policy`] and its rules.
///
/// The core fields (project, size) drive the neutral rules; `version` is a plain string a rule may
/// parse in its own format, and `attributes` carries any extra format-specific values (a wheel's
/// Python or platform tag, a package type) as named strings so the engine never sees a format type.
#[derive(Debug, Clone, Default)]
pub struct ArtifactFacts {
    pub project: String,
    pub filename: Option<String>,
    pub version: Option<String>,
    pub size: Option<u64>,
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

#[derive(Clone, Default, Debug)]
pub struct Policy {
    allow_projects: HashSet<String>,
    block_projects: HashSet<String>,
    max_file_size_bytes: Option<u64>,
    max_project_size_bytes: Option<u64>,
    rules: Vec<Arc<dyn ArtifactRule>>,
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
            max_file_size_bytes: config.max_file_size_bytes,
            max_project_size_bytes: config.max_project_size_bytes,
            rules: Vec::new(),
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

    #[must_use]
    pub const fn has_project_size_limit(&self) -> bool {
        self.max_project_size_bytes.is_some()
    }

    /// The configured per-file size limit, if any.
    #[must_use]
    pub const fn max_file_size(&self) -> Option<u64> {
        self.max_file_size_bytes
    }

    /// The configured per-project size limit, if any.
    #[must_use]
    pub const fn max_project_size(&self) -> Option<u64> {
        self.max_project_size_bytes
    }

    fn compute_active(&self) -> bool {
        !self.allow_projects.is_empty()
            || !self.block_projects.is_empty()
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

    /// Check an artifact's project, byte size, and every attached format-specific rule.
    ///
    /// # Errors
    /// Returns a denial when the facts match a configured policy rule.
    pub fn check_facts(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        self.check_project(action, &facts.project)?;
        self.check_file_size(action, facts)?;
        for rule in &self.rules {
            rule.check(action, facts)?;
        }
        Ok(())
    }

    /// Check an upload's project name and byte size: the neutral rules an ecosystem with no
    /// format-specific facts (an OCI blob or manifest) enforces without building full facts.
    ///
    /// # Errors
    /// Returns a denial when the project is disallowed or the size exceeds `max_file_size_bytes`.
    pub fn check_size(&self, action: PolicyAction, project: &str, size: u64) -> Result<(), PolicyDenial> {
        self.check_project(action, project)?;
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
