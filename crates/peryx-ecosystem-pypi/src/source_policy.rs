//! Which members of a virtual repository may contribute to one project's view.
//!
//! Resolution serves the answer and the search indexer describes it. When the two compute it apart
//! they drift, and search advertises names, versions, and metadata the route refuses to serve, so
//! both go through [`SourceSelection`].

use peryx_index::Index;
use peryx_policy::PolicyDenial;

use crate::cache::cached_denial;
use crate::policy::{FallbackMode, PypiPolicy as _};

/// A virtual repository's source policy, evaluated once for one project.
pub struct SourceSelection {
    mode: FallbackMode,
    cached_denial: Option<PolicyDenial>,
}

impl SourceSelection {
    pub fn new(index: &Index, project: &str) -> Self {
        Self {
            mode: index.policy.fallback_mode(),
            cached_denial: cached_denial(index, project),
        }
    }

    pub const fn mode(&self) -> FallbackMode {
        self.mode
    }

    /// Why every cached-reaching member was excluded, so a caller left with no member at all can
    /// report the denial rather than a bare miss.
    pub fn into_cached_denial(self) -> Option<PolicyDenial> {
        self.cached_denial
    }

    /// The members worth consulting, in shadow order. A member that reaches a cached index is out
    /// when the fallback mode forbids upstream content or the protected-name rule denies the
    /// project, both decided from the project name alone.
    pub fn candidates(&self, indexes: &[Index], layers: &[usize]) -> Vec<usize> {
        let consult_cached = self.mode != FallbackMode::NoFallback && self.cached_denial.is_none();
        peryx_index::shadow_order(indexes, layers)
            .into_iter()
            .filter(|&position| consult_cached || !peryx_index::reaches_cached(indexes, position))
            .collect()
    }

    /// Narrow the consulted members to the ones that reach the merged view, now that each member's
    /// files are known: every mode but plain fallback drops a member that produced none, and
    /// `private-first` then drops the cached-reaching members a hosted member shadows.
    ///
    /// Returns whether a cached-reaching member lost that race, which resolution logs as a
    /// collision.
    pub fn retain_selected<T>(
        &self,
        indexes: &[Index],
        members: &mut Vec<(usize, T)>,
        has_files: impl Fn(&T) -> bool,
    ) -> bool {
        if self.mode != FallbackMode::Fallback {
            members.retain(|(_, member)| has_files(member));
        }
        if self.mode != FallbackMode::PrivateFirst {
            return false;
        }
        let cached = |position: usize| peryx_index::reaches_cached(indexes, position);
        if !members.iter().any(|&(position, _)| !cached(position)) {
            return false;
        }
        let shadowed = members.iter().any(|&(position, _)| cached(position));
        members.retain(|&(position, _)| !cached(position));
        shadowed
    }
}
