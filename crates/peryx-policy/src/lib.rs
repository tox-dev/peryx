//! Owner capabilities keep ecosystem rules outside shared policy code.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

mod retention;

pub use retention::{
    RetentionCandidate, RetentionClass, RetentionConfig, RetentionDecision, RetentionFrontier, RetentionOutcome,
    RetentionPlan, RetentionPolicy, RetentionSelector, RetentionSummary, RetentionVisibility,
};

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    pub allow_resources: Vec<String>,
    pub block_resources: Vec<String>,
    pub protected_resources: Vec<String>,
    pub max_artifact_size_bytes: Option<u64>,
    pub max_resource_size_bytes: Option<u64>,
    pub max_accounted_bytes: Option<u64>,
    pub max_resources: Option<u64>,
    /// Records quota denials without rejecting writes.
    pub quota_audit: bool,
}

impl PolicyConfig {
    pub const KEYS: &'static [&'static str] = &[
        "allow_resources",
        "block_resources",
        "protected_resources",
        "max_artifact_size_bytes",
        "max_resource_size_bytes",
        "max_accounted_bytes",
        "max_resources",
        "quota_audit",
    ];
}

#[derive(Debug, Clone, Default)]
pub struct ArtifactFacts {
    pub resource: String,
    pub artifact: Option<String>,
    pub group: Option<String>,
    pub source: Option<String>,
    pub size: Option<u64>,
    pub upload_time: Option<i64>,
    pub now: Option<i64>,
    pub attributes: Vec<(&'static str, String)>,
}

impl ArtifactFacts {
    #[must_use]
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find_map(|(name, value)| (*name == key).then_some(value.as_str()))
    }

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
            &self.resource,
            self.artifact.as_deref(),
            self.group.clone(),
            rule,
            field,
            reason,
        )
    }
}

pub trait ArtifactRule: Send + Sync + fmt::Debug {
    /// # Errors
    ///
    /// Returns a [`PolicyDenial`] when the artifact violates the rule.
    fn check(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial>;
}

pub trait ResourceRule: Send + Sync + fmt::Debug {
    /// # Errors
    ///
    /// Returns a denial when the resource is not eligible for the action.
    fn check(&self, action: PolicyAction, resource: &str) -> Result<(), PolicyDenial>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PolicyLimits {
    pub max_artifact_bytes: Option<u64>,
    pub max_resource_bytes: Option<u64>,
    pub max_accounted_bytes: Option<u64>,
    pub max_resources: Option<u64>,
    pub max_groups_per_resource: Option<u64>,
}

impl PolicyLimits {
    const fn merge(self, other: Self) -> Self {
        Self {
            max_artifact_bytes: minimum(self.max_artifact_bytes, other.max_artifact_bytes),
            max_resource_bytes: minimum(self.max_resource_bytes, other.max_resource_bytes),
            max_accounted_bytes: minimum(self.max_accounted_bytes, other.max_accounted_bytes),
            max_resources: minimum(self.max_resources, other.max_resources),
            max_groups_per_resource: minimum(self.max_groups_per_resource, other.max_groups_per_resource),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PolicyCapabilities {
    artifact_rules: Vec<Arc<dyn ArtifactRule>>,
    resource_rules: Vec<Arc<dyn ResourceRule>>,
    limits: PolicyLimits,
    owner_settings: BTreeMap<String, String>,
    active: bool,
}

impl PolicyCapabilities {
    #[must_use]
    pub fn with_artifact_rules(mut self, rules: Vec<Arc<dyn ArtifactRule>>) -> Self {
        self.artifact_rules = rules;
        self
    }

    #[must_use]
    pub fn with_resource_rules(mut self, rules: Vec<Arc<dyn ResourceRule>>) -> Self {
        self.resource_rules = rules;
        self
    }

    #[must_use]
    pub const fn with_limits(mut self, limits: PolicyLimits) -> Self {
        self.limits = self.limits.merge(limits);
        self
    }

    #[must_use]
    pub fn with_owner_setting(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.owner_settings.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub const fn with_policy_activation(mut self) -> Self {
        self.active = true;
        self
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.artifact_rules.is_empty()
            && self.resource_rules.is_empty()
            && self.limits == PolicyLimits::default()
            && self.owner_settings.is_empty()
            && !self.active
    }
}

impl From<Vec<Arc<dyn ArtifactRule>>> for PolicyCapabilities {
    fn from(rules: Vec<Arc<dyn ArtifactRule>>) -> Self {
        Self::default().with_artifact_rules(rules)
    }
}

const fn minimum(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(match left.checked_sub(right) {
            Some(_) => right,
            None => left,
        }),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecisionState {
    Allow,
    Deny,
    Wait,
}

#[derive(Debug, Clone, Copy)]
pub struct PolicyEvaluation<'a> {
    pub action: PolicyAction,
    pub resource: &'a str,
    pub artifact: Option<&'a str>,
    pub group: Option<&'a str>,
    pub source: Option<&'a str>,
    pub state: PolicyDecisionState,
    pub rule: Option<&'static str>,
    pub reason: Option<&'a str>,
    pub next_eligible_at_unix: Option<i64>,
}

pub trait PolicyDecisionRecorder: Send + Sync + fmt::Debug {
    fn record(&self, evaluation: PolicyEvaluation<'_>);
}

#[derive(Clone, Default, Debug)]
struct ProtectedResources {
    exact: BTreeSet<String>,
    prefixes: BTreeSet<String>,
}

impl ProtectedResources {
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

    fn matched(&self, resource: &str) -> Option<String> {
        if self.exact.contains(resource) {
            return Some(resource.to_owned());
        }
        self.prefixes
            .iter()
            .find(|prefix| resource.starts_with(prefix.as_str()))
            .map(|prefix| format!("{prefix}*"))
    }
}

#[derive(Clone, Default, Debug)]
pub struct Policy {
    allow_resources: HashSet<String>,
    block_resources: HashSet<String>,
    protected_resources: ProtectedResources,
    limits: PolicyLimits,
    quota_audit: bool,
    artifact_rules: Vec<Arc<dyn ArtifactRule>>,
    resource_rules: Vec<Arc<dyn ResourceRule>>,
    owner_settings: BTreeMap<String, String>,
    owner_active: bool,
    recorder: Option<Arc<dyn PolicyDecisionRecorder>>,
    active: bool,
}

impl Policy {
    /// `normalize` must match the owner lookup normalization.
    #[must_use]
    pub fn compile(config: &PolicyConfig, normalize: impl Fn(&str) -> String) -> Self {
        let normalize_all = |names: &[String]| names.iter().map(|name| normalize(name)).collect();
        let policy = Self {
            allow_resources: normalize_all(&config.allow_resources),
            block_resources: normalize_all(&config.block_resources),
            protected_resources: ProtectedResources::compile(&config.protected_resources, &normalize),
            limits: PolicyLimits {
                max_artifact_bytes: config.max_artifact_size_bytes,
                max_resource_bytes: config.max_resource_size_bytes,
                max_accounted_bytes: config.max_accounted_bytes,
                max_resources: config.max_resources,
                max_groups_per_resource: None,
            },
            quota_audit: config.quota_audit,
            artifact_rules: Vec::new(),
            resource_rules: Vec::new(),
            owner_settings: BTreeMap::new(),
            owner_active: false,
            recorder: None,
            active: false,
        };
        Self {
            active: policy.compute_active(),
            ..policy
        }
    }

    #[must_use]
    pub fn with_rules(self, capabilities: impl Into<PolicyCapabilities>) -> Self {
        self.with_capabilities(capabilities.into())
    }

    #[must_use]
    pub fn with_capabilities(mut self, capabilities: PolicyCapabilities) -> Self {
        self.artifact_rules.extend(capabilities.artifact_rules);
        self.resource_rules.extend(capabilities.resource_rules);
        self.limits = self.limits.merge(capabilities.limits);
        self.owner_settings.extend(capabilities.owner_settings);
        self.owner_active |= capabilities.active;
        self.active = self.compute_active();
        self
    }

    #[must_use]
    pub fn with_decision_recorder(mut self, recorder: Arc<dyn PolicyDecisionRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    #[must_use]
    pub const fn max_artifact_size(&self) -> Option<u64> {
        self.limits.max_artifact_bytes
    }

    #[must_use]
    pub const fn has_resource_size_limit(&self) -> bool {
        self.max_resource_size().is_some()
    }

    #[must_use]
    pub const fn max_resource_size(&self) -> Option<u64> {
        self.limits.max_resource_bytes
    }

    #[must_use]
    pub const fn max_accounted_bytes(&self) -> Option<u64> {
        self.limits.max_accounted_bytes
    }

    #[must_use]
    pub const fn max_resources(&self) -> Option<u64> {
        self.limits.max_resources
    }

    #[must_use]
    pub const fn max_groups_per_resource(&self) -> Option<u64> {
        self.limits.max_groups_per_resource
    }

    #[must_use]
    pub const fn quota_audit(&self) -> bool {
        self.quota_audit
    }

    #[must_use]
    pub const fn enforces_quota(&self) -> bool {
        self.limits.max_accounted_bytes.is_some()
            || self.limits.max_resources.is_some()
            || self.limits.max_groups_per_resource.is_some()
    }

    #[must_use]
    pub fn owner_setting(&self, key: &str) -> Option<&str> {
        self.owner_settings.get(key).map(String::as_str)
    }

    fn compute_active(&self) -> bool {
        !self.allow_resources.is_empty()
            || !self.block_resources.is_empty()
            || !self.protected_resources.is_empty()
            || self.limits.max_artifact_bytes.is_some()
            || self.limits.max_resource_bytes.is_some()
            || self.enforces_quota()
            || !self.artifact_rules.is_empty()
            || !self.resource_rules.is_empty()
            || self.owner_active
    }

    #[must_use]
    pub const fn active(&self) -> bool {
        self.active
    }

    /// # Errors
    ///
    /// Returns a denial when the resource violates shared or owner rules.
    pub fn check_resource(&self, action: PolicyAction, resource: &str) -> Result<(), PolicyDenial> {
        let result = self.evaluate_resource(action, resource);
        self.record(action, resource, None, None, None, &result);
        result
    }

    fn evaluate_resource(&self, action: PolicyAction, resource: &str) -> Result<(), PolicyDenial> {
        if action == PolicyAction::Cached
            && let Some(rule) = self.protected_resources.matched(resource)
        {
            return Err(PolicyDenial::new(
                action,
                resource,
                None,
                None,
                "protected-name",
                "resource",
                format!("resource {resource:?} is protected from upstream fallback by rule {rule:?}"),
            ));
        }
        if !self.allow_resources.is_empty() && !self.allow_resources.contains(resource) {
            return Err(PolicyDenial::new(
                action,
                resource,
                None,
                None,
                "resource-allow-list",
                "resource",
                format!("resource {resource:?} is not in the allow list"),
            ));
        }
        if self.block_resources.contains(resource) {
            return Err(PolicyDenial::new(
                action,
                resource,
                None,
                None,
                "resource-block-list",
                "resource",
                format!("resource {resource:?} is blocked"),
            ));
        }
        for rule in &self.resource_rules {
            rule.check(action, resource)?;
        }
        Ok(())
    }

    /// # Errors
    ///
    /// Returns a denial when the facts match a configured policy rule.
    pub fn check_facts(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        let result = self.evaluate_facts(action, facts);
        self.record(
            action,
            &facts.resource,
            facts.artifact.as_deref(),
            facts.group.as_deref(),
            facts.source.as_deref(),
            &result,
        );
        result
    }

    /// # Errors
    ///
    /// Returns a denial when the resource or size violates policy.
    pub fn check_size(&self, action: PolicyAction, resource: &str, size: u64) -> Result<(), PolicyDenial> {
        let result = self.evaluate_size(action, resource, size);
        self.record(action, resource, None, None, None, &result);
        result
    }

    fn evaluate_facts(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        self.evaluate_resource(action, &facts.resource)?;
        self.check_artifact_size(action, facts)?;
        for rule in &self.artifact_rules {
            rule.check(action, facts)?;
        }
        Ok(())
    }

    fn evaluate_size(&self, action: PolicyAction, resource: &str, size: u64) -> Result<(), PolicyDenial> {
        self.evaluate_resource(action, resource)?;
        if let Some(limit) = self.max_artifact_size()
            && size > limit
        {
            return Err(PolicyDenial::new(
                action,
                resource,
                None,
                None,
                "max-artifact-size",
                "size",
                format!("artifact size {size} exceeds limit {limit}"),
            ));
        }
        Ok(())
    }

    fn record(
        &self,
        action: PolicyAction,
        resource: &str,
        artifact: Option<&str>,
        group: Option<&str>,
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
            resource,
            artifact,
            group,
            source,
            state,
            rule,
            reason,
            next_eligible_at_unix: None,
        });
    }

    fn check_artifact_size(&self, action: PolicyAction, facts: &ArtifactFacts) -> Result<(), PolicyDenial> {
        if let Some(limit) = self.max_artifact_size() {
            let Some(size) = facts.size else {
                return Err(facts.denial(
                    action,
                    "max-artifact-size",
                    "size",
                    "artifact size is unknown".to_owned(),
                ));
            };
            if size > limit {
                return Err(facts.denial(
                    action,
                    "max-artifact-size",
                    "size",
                    format!("artifact size {size} exceeds limit {limit}"),
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
    pub resource: Box<str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<Box<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<Box<str>>,
    pub rule: &'static str,
    pub field: &'static str,
    pub reason: Box<str>,
}

impl PolicyDenial {
    #[must_use]
    pub fn new(
        action: PolicyAction,
        resource: &str,
        artifact: Option<&str>,
        group: Option<String>,
        rule: &'static str,
        field: &'static str,
        reason: String,
    ) -> Self {
        Self {
            action,
            resource: Box::from(resource),
            artifact: artifact.map(Box::from),
            group: group.map(String::into_boxed_str),
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

#[cfg(test)]
#[path = "../tests/unit/tests.rs"]
mod tests;
