//! Shadowed candidates joined with the policy decision that governs each filename.
//!
//! [`AppState::query_shadowed`](crate::shadow) explains a virtual repository's resolution: which
//! member won each filename and which lost. Operators also need to know whether policy would let a
//! caller have a candidate at all, so this layers the recorded allow, deny, or wait outcome on top of
//! that page. The decisions come from one bounded read of the policy-decision store keyed by
//! repository and project, never a lookup per candidate, and the join keeps the shadow page's order
//! and cursor untouched. Only serve and cache-fill decisions join a candidate; an upload-time outcome
//! governs publishing, not what a virtual repository resolves.

use std::collections::{HashMap, HashSet};

use peryx_core::ShadowCandidate;
use peryx_policy::{PolicyAction, PolicyDecisionState};
use peryx_storage::meta::PolicyDecisionQuery;

use crate::shadow::{ShadowPage, ShadowQuery, ShadowQueryError};
use crate::state::AppState;

/// One bounded read covers the decisions for a page's filenames; a project with more current serve
/// decisions than this shows the join for the ones the read returns.
const DECISION_READ_LIMIT: usize = 100;

/// The recorded policy outcome that governs one candidate's filename in its repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateDecision {
    /// Whether policy allows the filename, denies it, or holds a caller until a retry window opens.
    pub state: PolicyDecisionState,
    /// The rule that decided the outcome, when one is named.
    pub rule: Option<String>,
    /// The recorded reason, already sanitized of any upstream URL or credential when it was stored.
    pub reason: Option<String>,
    /// When the decision was evaluated, in Unix seconds.
    pub evaluated_at_unix: i64,
    /// The earliest retry time for a `wait` outcome, in Unix seconds.
    pub next_eligible_at_unix: Option<i64>,
    /// Whether the decision still reflects the repository's current policy inputs.
    pub fresh: bool,
}

/// One resolution candidate with the decision that currently governs its filename, when one exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedCandidate {
    /// The selected or shadowed candidate, exactly as the shadow query returned it.
    pub candidate: ShadowCandidate,
    /// The recorded policy outcome for its filename, or `None` when policy never evaluated it.
    pub decision: Option<CandidateDecision>,
}

/// A page of inspected candidates, ordered and cursored identically to the underlying shadow page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowInspection {
    pub candidates: Vec<InspectedCandidate>,
    pub next_cursor: Option<String>,
}

impl AppState {
    /// Page a virtual repository's shadowed candidates and annotate each with its filename's decision.
    ///
    /// # Errors
    /// Returns the same validation and store errors as
    /// [`query_shadowed`](AppState::query_shadowed), plus a store error when the decision read fails.
    pub fn inspect_shadowed(&self, query: &ShadowQuery) -> Result<ShadowInspection, ShadowQueryError> {
        let page = self.query_shadowed(query)?;
        let decisions = self.filename_decisions(&query.repository, &query.project, &page)?;
        let mut candidates = Vec::with_capacity(page.candidates.len());
        for candidate in page.candidates {
            let decision = decisions.get(candidate.filename.as_str()).cloned();
            candidates.push(InspectedCandidate { candidate, decision });
        }
        Ok(ShadowInspection {
            candidates,
            next_cursor: page.next_cursor,
        })
    }

    /// The serve or cache-fill decision recorded for each filename on the page, read once for the
    /// whole project rather than per candidate.
    fn filename_decisions(
        &self,
        repository: &str,
        project: &str,
        page: &ShadowPage,
    ) -> Result<HashMap<String, CandidateDecision>, ShadowQueryError> {
        let wanted: HashSet<&str> = page
            .candidates
            .iter()
            .map(|candidate| candidate.filename.as_str())
            .collect();
        if wanted.is_empty() {
            return Ok(HashMap::new());
        }
        let query = PolicyDecisionQuery {
            repository: Some(repository.to_owned()),
            project: Some(project.to_owned()),
            limit: DECISION_READ_LIMIT,
            ..PolicyDecisionQuery::default()
        };
        // The shadow page was already read from this store, and the query is built from validated
        // inputs, so the only failure left is a store read the caller surfaces as its own error. An
        // inline conversion keeps that path out of a closure the tests could never reach.
        let found = match self.meta.query_policy_decisions(&query) {
            Ok(found) => found,
            Err(error) => return Err(ShadowQueryError::Store(error.to_string())),
        };
        let mut decisions: HashMap<String, CandidateDecision> = HashMap::new();
        for item in found.decisions {
            let record = item.record;
            if !matches!(record.action, PolicyAction::Serve | PolicyAction::Cached) {
                continue;
            }
            let Some(filename) = record.filename.clone() else {
                continue;
            };
            if !wanted.contains(filename.as_str()) {
                continue;
            }
            if decisions.contains_key(&filename) {
                continue;
            }
            decisions.insert(
                filename,
                CandidateDecision {
                    state: record.state,
                    rule: record.rule,
                    reason: record.reason,
                    evaluated_at_unix: record.evaluated_at_unix,
                    next_eligible_at_unix: record.next_eligible_at_unix,
                    fresh: item.fresh,
                },
            );
        }
        Ok(decisions)
    }
}

#[cfg(test)]
#[path = "../tests/unit/shadow_inspect/tests.rs"]
mod tests;
