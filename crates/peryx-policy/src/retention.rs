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

    /// Classifies one resource in rank, artifact, and digest order for deterministic plans.
    ///
    /// The returned plan holds compact state, not decisions: expanding every decision up front costs
    /// the product of removals and surviving groups, because each removal repeats the whole surviving
    /// set. [`RetentionPlan::decisions`] expands one decision at a time instead.
    #[must_use]
    pub fn plan_resource(&self, now: Option<i64>, mut candidates: Vec<RetentionCandidate>) -> RetentionPlan {
        candidates.sort_by(|left, right| {
            (left.rank, &left.artifact, &left.digest).cmp(&(right.rank, &right.artifact, &right.digest))
        });
        let mut verdicts: Vec<Verdict> = candidates
            .iter()
            .map(|candidate| self.classify(candidate, now))
            .collect();
        let retained: Vec<String> = candidates
            .iter()
            .zip(&verdicts)
            .filter(|(_, verdict)| verdict.outcome == RetentionOutcome::Retain)
            .filter_map(|(candidate, _)| candidate.group.as_deref())
            .collect::<BTreeSet<&str>>()
            .into_iter()
            .map(str::to_owned)
            .collect();
        charge_each_digest_once(&candidates, &mut verdicts);
        RetentionPlan {
            candidates,
            verdicts,
            retained,
        }
    }

    fn classify(&self, candidate: &RetentionCandidate, now: Option<i64>) -> Verdict {
        let (outcome, rule) = self.decide(candidate, now);
        Verdict {
            outcome,
            rule,
            bytes: candidate.bytes,
        }
    }

    fn decide(&self, candidate: &RetentionCandidate, now: Option<i64>) -> (RetentionOutcome, Option<&'static str>) {
        if let Some(rule) = self.keep.iter().find(|rule| rule.matches(candidate, now)) {
            return (RetentionOutcome::Retain, Some(rule.name()));
        }
        if let Some(rule) = self.expire.iter().find(|rule| rule.matches(candidate, now)) {
            return (RetentionOutcome::Remove, Some(rule.name()));
        }
        (RetentionOutcome::Retain, None)
    }
}

/// Charge each digest to at most one of the removals that reference it, before any decision expands.
///
/// Content is stored by digest, so several artifacts that share one digest share one stored blob.
/// Deleting all of them frees that blob once, and deleting some while another keeps it live frees
/// nothing. A removal that repeats its artifact's own size therefore overstates what the plan frees, by
/// the whole size of every reference after the first.
///
/// The charge belongs here rather than in [`RetentionPlan::decisions`]: expansion sees one row at a
/// time and cannot know whether an earlier or later row shares the digest, which is precisely how the
/// double count arises. This changes only what the removals sum to — never which rows the plan returns,
/// their order, or the size a retained row reports, since two artifacts sharing a digest are still two
/// artifacts.
///
/// The resource under evaluation bounds what is visible: a reference held by another resource is not in
/// `candidates`, so its digest is charged as if this plan held the last references to it.
///
/// A candidate with no digest names no content and can never be shown to share a blob, so it keeps its
/// own size.
fn charge_each_digest_once(candidates: &[RetentionCandidate], verdicts: &mut [Verdict]) {
    let live: BTreeSet<&str> = candidates
        .iter()
        .zip(&*verdicts)
        .filter(|(_, verdict)| verdict.outcome == RetentionOutcome::Retain)
        .map(|(candidate, _)| candidate.digest.as_str())
        .collect();
    let mut charged: BTreeSet<&str> = BTreeSet::new();
    for (candidate, verdict) in candidates.iter().zip(verdicts) {
        let digest = candidate.digest.as_str();
        if verdict.outcome == RetentionOutcome::Remove
            && !digest.is_empty()
            && (live.contains(digest) || !charged.insert(digest))
        {
            verdict.bytes = 0;
        }
    }
}

/// What a policy decided about one candidate, before that decision is expanded: the outcome, the rule
/// that produced it, and the bytes this row is charged for. Under forty bytes per candidate, so a whole
/// project's verdicts stay negligible beside its candidates.
struct Verdict {
    outcome: RetentionOutcome,
    rule: Option<&'static str>,
    bytes: u64,
}

/// One resource classified against a policy, ready to stream.
///
/// Every removal decision repeats the surviving groups, so a fully materialized plan costs removals
/// times survivors in owned strings — a project with ten thousand versions, half of them retained,
/// reaches twenty-five million. This holds the survivors once and expands them into a decision only as
/// [`decisions`](Self::decisions) yields it, so the caller's live set is the candidates, the verdicts,
/// the surviving-group index, and one decision.
pub struct RetentionPlan {
    candidates: Vec<RetentionCandidate>,
    verdicts: Vec<Verdict>,
    retained: Vec<String>,
}

impl RetentionPlan {
    /// The heap this plan holds while it streams, over and above the candidates it was handed: the
    /// verdicts, the surviving-group index, and the one expanded decision in flight.
    ///
    /// A caller that budgets a resource's peak footprint adds this to what it counted for the
    /// candidates, before it starts streaming. The plan's total output stays unbounded by design —
    /// the contract repeats the surviving groups in every removal — so a budget must not be read as a
    /// cap on the response.
    #[must_use]
    pub fn live_bytes(&self) -> usize {
        let index: usize = self
            .retained
            .iter()
            .map(|group| size_of::<String>() + group.len())
            .sum();
        // One expanded decision clones the whole index, which is why it counts twice.
        self.verdicts.len() * size_of::<Verdict>() + index * 2
    }

    /// Expands each decision as it is yielded, in the plan's deterministic order. A consumer that
    /// stops early leaves the rest of the resource unexpanded.
    #[must_use]
    pub fn decisions(self) -> impl ExactSizeIterator<Item = RetentionDecision> {
        let retained = self.retained;
        self.candidates
            .into_iter()
            .zip(self.verdicts)
            .map(move |(candidate, verdict)| RetentionDecision {
                resource: candidate.resource,
                group: candidate.group,
                artifact: candidate.artifact,
                digest: candidate.digest,
                class: candidate.class,
                visibility: candidate.visibility,
                source: candidate.source,
                bytes: verdict.bytes,
                outcome: verdict.outcome,
                rule: verdict.rule,
                retained_groups: match verdict.outcome {
                    RetentionOutcome::Remove => retained.clone(),
                    RetentionOutcome::Retain => Vec::new(),
                },
            })
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
    /// On a removal, the capacity the plan makes eligible for reclamation: the stored size charged to
    /// one removal per digest, and zero on every further reference to that digest and on any digest a
    /// retained decision keeps live. On a retained decision, the artifact's own size.
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
