//! Which sources of a virtual repository may contribute to one project's view.
//!
//! Resolution serves the answer, the search indexer describes it, and the shadow view explains it.
//! When they compute it apart they drift, and search advertises names, versions, and metadata the
//! route refuses to serve, so all three go through [`SourceSelection`].
//!
//! The rule ranks leaves, never containers. A virtual member has no source of its own: it stands for
//! the cached and hosted indexes beneath it, and classifying the container instead lets a cache two
//! levels down shadow a hosted sibling.

use peryx_index::Index;
use peryx_policy::PolicyDenial;

use crate::cache::cached_denial;
use crate::policy::{FallbackMode, PypiPolicy as _};
use crate::shadow::ShadowReason;

/// A virtual repository's source policy, evaluated once for one project.
pub struct SourceSelection {
    mode: FallbackMode,
    cached_denial: Option<PolicyDenial>,
    enclosing_refuses_cached: bool,
}

impl SourceSelection {
    pub fn new(index: &Index, project: &str) -> Self {
        Self {
            mode: index.policy.fallback_mode(),
            cached_denial: cached_denial(index, project),
            enclosing_refuses_cached: false,
        }
    }

    /// The same policy read inside a repository that already refuses cached content, so a nested
    /// view never reaches upstream for a leaf the enclosing view would discard anyway.
    #[must_use]
    pub const fn under_cached_refusal(mut self, refused: bool) -> Self {
        self.enclosing_refuses_cached = refused;
        self
    }

    pub const fn mode(&self) -> FallbackMode {
        self.mode
    }

    /// Whether a cached leaf may contribute at all, decided from the project name alone.
    pub const fn consults_cached(&self) -> bool {
        !self.enclosing_refuses_cached && !matches!(self.mode, FallbackMode::NoFallback) && self.cached_denial.is_none()
    }

    /// Why every cached leaf was excluded, so a caller left with no source at all can report the
    /// denial rather than a bare miss.
    pub fn into_cached_denial(self) -> Option<PolicyDenial> {
        self.cached_denial
    }

    /// The members worth consulting, in configured order. A member drops out only when every leaf it
    /// reaches is cached and this repository refuses cached content: a member that also reaches a
    /// hosted leaf stays in, and the refusal travels down to it instead.
    pub fn members(&self, indexes: &[Index], layers: &[usize]) -> Vec<usize> {
        let consult_cached = self.consults_cached();
        layers
            .iter()
            .copied()
            .filter(|&position| consult_cached || peryx_index::layers_include_hosted(indexes, &[position]))
            .collect()
    }

    /// Rank resolved leaves and narrow them to the ones that reach the merged view.
    ///
    /// Ranking is stable hosted-before-cached, the order a merge resolves a shared filename in. Then
    /// every mode but plain fallback drops a leaf that produced no file, and `private-first` drops
    /// the cached leaves a hosted leaf shadows.
    ///
    /// Returns whether a cached leaf lost that race, which resolution logs as a collision.
    pub fn select<T>(&self, indexes: &[Index], leaves: &mut Vec<(usize, T)>, has_files: impl Fn(&T) -> bool) -> bool {
        let cached = |position: usize| peryx_index::reaches_cached(indexes, position);
        leaves.sort_by_key(|&(position, _)| cached(position));
        if self.mode != FallbackMode::Fallback {
            leaves.retain(|(_, leaf)| has_files(leaf));
        }
        if self.mode != FallbackMode::PrivateFirst || !leaves.iter().any(|&(position, _)| !cached(position)) {
            return false;
        }
        let shadowed = leaves.iter().any(|&(position, _)| cached(position));
        leaves.retain(|&(position, _)| !cached(position));
        shadowed
    }

    /// Why this repository drops its cached leaves for the project, if it does, given whether a
    /// hosted leaf contributed. [`select`](Self::select) applies this rule; the shadow view records
    /// it against each candidate instead.
    #[must_use]
    pub const fn cached_exclusion(&self, hosted_found: bool) -> Option<ShadowReason> {
        if self.cached_denial.is_some() {
            return Some(ShadowReason::Protected);
        }
        if !self.consults_cached() {
            return Some(ShadowReason::Fallback);
        }
        match self.mode {
            FallbackMode::PrivateFirst if hosted_found => Some(ShadowReason::Fallback),
            _ => None,
        }
    }
}
