//! Ecosystem-neutral retention-plan evaluation.
//!
//! A retention policy names two ordered rule groups: `keep` rules protect artifacts, `expire` rules
//! mark them for removal. The engine evaluates one repository's [`RetentionCandidate`] stream against
//! the compiled policy and returns a deterministic [`RetentionDecision`] per artifact without touching
//! storage or blobs. An ecosystem crate adapts its own records into
//! neutral candidates and hands them here one project at a time, so a large repository never
//! materializes as one in-memory plan.
//!
//! A keep rule always wins over an expire rule (the precedence
//! [Google Artifact Registry cleanup policies](https://cloud.google.com/artifact-registry/docs/repositories/cleanup-policy)
//! define), and the decision names the rule that decided it. Version ordering is the caller's:
//! candidates carry a [`rank`](RetentionCandidate::rank) an ecosystem assigns through
//! version syntax), so this crate names no package format.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The storage lifecycle a candidate belongs to, mirroring the storage accounting classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClass {
    Hosted,
    Cached,
    Generated,
    Trash,
}

/// A candidate's logical visibility, recorded on the decision so an operator sees what a removal hides.
///
/// A [`RetentionSelector::Visibility`] rule also matches on it, so a policy can name one of these states
/// directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionVisibility {
    Active,
    Yanked,
    Hidden,
}

/// One retention rule.
///
/// A rule appears in a policy's `keep` or `expire` group and matches a candidate by exactly one
/// dimension. The engine never assigns a rule its group's meaning: the same rule protects in `keep`
/// and removes in `expire`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "selector", rename_all = "kebab-case")]
pub enum RetentionSelector {
    /// Match a candidate whose publish time is at least `older_than_seconds` before the evaluation
    /// clock. A candidate with no publish time, or an evaluation with no clock, never matches, so the
    /// engine ages nothing it cannot date.
    Age { older_than_seconds: u64 },
    /// Match a candidate routed from the named source.
    Source { name: String },
    /// Match a candidate whose project name begins with `prefix`.
    ProjectPrefix { prefix: String },
    /// Match a candidate among the newest `count` versions of its project, by the caller's rank.
    KeepLatest { count: u64 },
    /// Match a cached candidate.
    Cached,
    /// Match a soft-deleted (trash) candidate.
    Trash,
    /// Match a candidate whose content no live reference reaches.
    Orphan,
    /// Match a candidate in the named logical [visibility](RetentionVisibility) state: a yanked release
    /// an author retracted, a hidden artifact, or an active one. This reads the same visibility a
    /// decision reports, so an `expire` rule can reclaim retracted releases a `keep-latest` count would
    /// otherwise pin.
    Visibility { state: RetentionVisibility },
}

impl RetentionSelector {
    /// The stable rule name a decision records.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Age { .. } => "age",
            Self::Source { .. } => "source",
            Self::ProjectPrefix { .. } => "project-prefix",
            Self::KeepLatest { .. } => "keep-latest",
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
            Self::ProjectPrefix { prefix } => candidate.project.starts_with(prefix.as_str()),
            Self::KeepLatest { count } => candidate.rank < *count,
            Self::Cached => candidate.class == RetentionClass::Cached,
            Self::Trash => candidate.class == RetentionClass::Trash,
            Self::Orphan => candidate.orphan,
            Self::Visibility { state } => candidate.visibility == *state,
        }
    }
}

/// One artifact an ecosystem adapts from its own records for evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionCandidate {
    pub project: String,
    pub version: Option<String>,
    /// The artifact identity within its version.
    pub artifact: String,
    pub digest: String,
    pub class: RetentionClass,
    pub visibility: RetentionVisibility,
    /// The routed source, when the record names one; matched by [`RetentionSelector::Source`].
    pub source: Option<String>,
    /// The candidate's estimated physical bytes.
    pub bytes: u64,
    pub upload_time_unix: Option<i64>,
    /// The candidate's version position within its project, newest first (`0` is newest). The
    /// ecosystem assigns it; [`RetentionSelector::KeepLatest`] and the output order read it.
    pub rank: u64,
    pub orphan: bool,
}

/// The operator's retention rules, as loaded from configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    pub keep: Vec<RetentionSelector>,
    pub expire: Vec<RetentionSelector>,
}

/// The compiled retention policy. Compiling records a content [`version`](RetentionPolicy::version) so
/// two runs of the same rules produce the same identity on their plans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPolicy {
    keep: Vec<RetentionSelector>,
    expire: Vec<RetentionSelector>,
    version: u64,
}

impl RetentionPolicy {
    /// Compile the operator's rules once.
    #[must_use]
    pub fn compile(config: &RetentionConfig) -> Self {
        Self {
            version: policy_version(config),
            keep: config.keep.clone(),
            expire: config.expire.clone(),
        }
    }

    /// The policy's content identity: equal rules compile to an equal version through a stable FNV-1a
    /// hash of the rules' canonical form.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Whether the policy has no rules, so evaluation would retain everything.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.keep.is_empty() && self.expire.is_empty()
    }

    /// Evaluate one project's candidates against the policy.
    ///
    /// The returned decisions are ordered deterministically (newest version first by rank, then
    /// artifact, then digest), so repeating a call over the same candidates yields byte-identical
    /// output. Each removal decision records the surviving versions of the same project as its
    /// retained alternatives. `now` is the evaluation clock an age rule ages against.
    #[must_use]
    pub fn plan_project(&self, now: Option<i64>, mut candidates: Vec<RetentionCandidate>) -> Vec<RetentionDecision> {
        candidates.sort_by(|left, right| {
            (left.rank, &left.artifact, &left.digest).cmp(&(right.rank, &right.artifact, &right.digest))
        });
        let mut decisions: Vec<RetentionDecision> = candidates
            .into_iter()
            .map(|candidate| {
                let (outcome, rule) = self.classify(&candidate, now);
                RetentionDecision {
                    project: candidate.project,
                    version: candidate.version,
                    artifact: candidate.artifact,
                    digest: candidate.digest,
                    class: candidate.class,
                    visibility: candidate.visibility,
                    source: candidate.source,
                    bytes: candidate.bytes,
                    outcome,
                    rule,
                    retained_alternatives: Vec::new(),
                }
            })
            .collect();
        let retained: BTreeSet<String> = decisions
            .iter()
            .filter(|decision| decision.outcome == RetentionOutcome::Retain)
            .filter_map(|decision| decision.version.clone())
            .collect();
        for decision in &mut decisions {
            if decision.outcome == RetentionOutcome::Remove {
                decision.retained_alternatives = retained
                    .iter()
                    .filter(|version| Some(version.as_str()) != decision.version.as_deref())
                    .cloned()
                    .collect();
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

/// Whether a candidate is retained or eligible for removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionOutcome {
    Retain,
    Remove,
}

/// One artifact's evaluated outcome. A removal decision estimates the reclaimable `bytes` and lists
/// the project versions a caller would still serve after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RetentionDecision {
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
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
    pub retained_alternatives: Vec<String>,
}

/// The metadata snapshot a plan evaluated, so stale input is rejectable later.
///
/// It mirrors the storage policy-input generation an adapter reads, and round-trips through a plan
/// cursor, so a resumed export carries the frontier it must still match.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionFrontier {
    pub repository: u64,
    pub catalog: u64,
    pub policy: u64,
}

/// A plan's identity header: the policy that produced it and the metadata snapshot it read.
///
/// An adapter emits it once alongside the streamed decisions, and a resumed export presents it back to
/// prove its inputs have not shifted.
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
        RetentionSelector::ProjectPrefix { prefix } => {
            fnv1a(hash, &[2]);
            encode_string(hash, prefix);
        }
        RetentionSelector::KeepLatest { count } => {
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
