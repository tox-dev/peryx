//! Keep rules take precedence. Callers assign each group a newest-first rank.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClass {
    Hosted,
    Cached,
    Generated,
    Trash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionVisibility {
    Active,
    Withdrawn,
    Hidden,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "selector", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RetentionSelector {
    Age { older_than_seconds: u64 },
    Source { name: String },
    ResourcePrefix { prefix: String },
    KeepLatestGroups { count: u64 },
    Cached,
    Trash,
    Orphan,
    Visibility { state: RetentionVisibility },
}

impl RetentionSelector {
    /// Persisted in [`RetentionDecision::rule`].
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Age { .. } => "age",
            Self::Source { .. } => "source",
            Self::ResourcePrefix { .. } => "resource-prefix",
            Self::KeepLatestGroups { .. } => "keep-latest-groups",
            Self::Cached => "cached",
            Self::Trash => "trash",
            Self::Orphan => "orphan",
            Self::Visibility { .. } => "visibility",
        }
    }

    fn matches(&self, candidate: &RetentionCandidate, now: Option<i64>) -> bool {
        match self {
            Self::Age { older_than_seconds } => {
                matches!(
                    (now, candidate.upload_time_unix),
                    (Some(now), Some(uploaded))
                        if i128::from(now) - i128::from(uploaded) >= i128::from(*older_than_seconds)
                )
            }
            Self::Source { name } => candidate.source.as_deref() == Some(name.as_str()),
            Self::ResourcePrefix { prefix } => candidate.resource.starts_with(prefix.as_str()),
            Self::KeepLatestGroups { count } => candidate.rank < *count,
            Self::Cached => candidate.class == RetentionClass::Cached,
            Self::Trash => candidate.class == RetentionClass::Trash,
            Self::Orphan => candidate.orphan,
            Self::Visibility { state } => candidate.visibility == *state,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionCandidate {
    pub resource: String,
    pub group: Option<String>,
    pub artifact: String,
    pub digest: String,
    pub class: RetentionClass,
    pub visibility: RetentionVisibility,
    pub source: Option<String>,
    pub bytes: u64,
    pub upload_time_unix: Option<i64>,
    pub rank: u64,
    pub orphan: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    pub keep: Vec<RetentionSelector>,
    pub expire: Vec<RetentionSelector>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPolicy {
    keep: Vec<RetentionSelector>,
    expire: Vec<RetentionSelector>,
    version: u64,
}

impl RetentionPolicy {
    #[must_use]
    pub fn compile(config: &RetentionConfig, normalize: impl Fn(&str) -> String) -> Self {
        let config = RetentionConfig {
            keep: normalize_selectors(&config.keep, &normalize),
            expire: normalize_selectors(&config.expire, &normalize),
        };
        Self {
            version: policy_version(&config),
            keep: config.keep,
            expire: config.expire,
        }
    }

    /// Changes when selector order or configuration changes.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.keep.is_empty() && self.expire.is_empty()
    }

    /// Returns keep selectors followed by expire selectors.
    pub fn selectors(&self) -> impl Iterator<Item = &RetentionSelector> {
        self.keep.iter().chain(&self.expire)
    }

    /// Uses rank, artifact, and digest order for deterministic plans.
    #[must_use]
    pub fn plan_resource(&self, now: Option<i64>, mut candidates: Vec<RetentionCandidate>) -> Vec<RetentionDecision> {
        candidates.sort_by(|left, right| {
            (left.rank, &left.artifact, &left.digest).cmp(&(right.rank, &right.artifact, &right.digest))
        });
        let mut decisions: Vec<RetentionDecision> = candidates
            .into_iter()
            .map(|candidate| {
                let (outcome, rule) = self.classify(&candidate, now);
                RetentionDecision {
                    resource: candidate.resource,
                    group: candidate.group,
                    artifact: candidate.artifact,
                    digest: candidate.digest,
                    class: candidate.class,
                    visibility: candidate.visibility,
                    source: candidate.source,
                    bytes: candidate.bytes,
                    outcome,
                    rule,
                    retained_groups: Vec::new(),
                }
            })
            .collect();
        let retained: BTreeSet<String> = decisions
            .iter()
            .filter(|decision| decision.outcome == RetentionOutcome::Retain)
            .filter_map(|decision| decision.group.clone())
            .collect();
        for decision in &mut decisions {
            if decision.outcome == RetentionOutcome::Remove {
                decision.retained_groups = retained.iter().cloned().collect();
            }
        }
        decisions
    }

    fn classify(&self, candidate: &RetentionCandidate, now: Option<i64>) -> (RetentionOutcome, Option<&'static str>) {
        if let Some(rule) = self.keep.iter().find(|rule| rule.matches(candidate, now)) {
            return (RetentionOutcome::Retain, Some(rule.name()));
        }
        if let Some(rule) = self.expire.iter().find(|rule| rule.matches(candidate, now)) {
            return (RetentionOutcome::Remove, Some(rule.name()));
        }
        (RetentionOutcome::Retain, None)
    }
}

fn normalize_selectors(selectors: &[RetentionSelector], normalize: &impl Fn(&str) -> String) -> Vec<RetentionSelector> {
    selectors
        .iter()
        .map(|selector| match selector {
            RetentionSelector::ResourcePrefix { prefix } => RetentionSelector::ResourcePrefix {
                prefix: normalize(prefix),
            },
            selector => selector.clone(),
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionOutcome {
    Retain,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetentionDecision {
    pub resource: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub artifact: String,
    pub digest: String,
    pub class: RetentionClass,
    pub visibility: RetentionVisibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub bytes: u64,
    pub outcome: RetentionOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub retained_groups: Vec<String>,
}

/// Metadata generations used to reject stale plans.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionFrontier {
    pub repository: u64,
    pub catalog: u64,
    pub policy: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionSummary {
    pub policy_version: u64,
    pub frontier: RetentionFrontier,
}

fn policy_version(config: &RetentionConfig) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    encode_group(&mut hash, 0, &config.keep);
    encode_group(&mut hash, 1, &config.expire);
    hash
}

fn encode_group(hash: &mut u64, tag: u8, selectors: &[RetentionSelector]) {
    fnv1a(hash, &[tag]);
    fnv1a(hash, &(selectors.len() as u64).to_le_bytes());
    for selector in selectors {
        encode_selector(hash, selector);
    }
}

fn encode_selector(hash: &mut u64, selector: &RetentionSelector) {
    match selector {
        RetentionSelector::Age { older_than_seconds } => {
            fnv1a(hash, &[0]);
            fnv1a(hash, &older_than_seconds.to_le_bytes());
        }
        RetentionSelector::Source { name } => {
            fnv1a(hash, &[1]);
            encode_string(hash, name);
        }
        RetentionSelector::ResourcePrefix { prefix } => {
            fnv1a(hash, &[2]);
            encode_string(hash, prefix);
        }
        RetentionSelector::KeepLatestGroups { count } => {
            fnv1a(hash, &[3]);
            fnv1a(hash, &count.to_le_bytes());
        }
        RetentionSelector::Cached => fnv1a(hash, &[4]),
        RetentionSelector::Trash => fnv1a(hash, &[5]),
        RetentionSelector::Orphan => fnv1a(hash, &[6]),
        RetentionSelector::Visibility { state } => fnv1a(hash, &[7, *state as u8]),
    }
}

fn encode_string(hash: &mut u64, value: &str) {
    fnv1a(hash, &(value.len() as u64).to_le_bytes());
    fnv1a(hash, value.as_bytes());
}

fn fnv1a(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

#[cfg(test)]
#[path = "../tests/unit/retention/tests.rs"]
mod tests;
