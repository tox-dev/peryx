//! Ecosystem-neutral records that explain virtual-repository shadowing.
//!
//! When a virtual repository resolves a project, one member wins each filename and the rest are
//! shadowed. Each ecosystem replays its own resolution to produce these neutral candidates; the
//! management query merges and paginates them without naming a format.

/// Where a resolution candidate came from within a virtual repository's members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowSource {
    /// A hosted member: an artifact uploaded to peryx.
    Hosted,
    /// A cached member: an artifact mirrored from an upstream index.
    Cached,
}

impl ShadowSource {
    /// The stable lowercase identifier used in the API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hosted => "hosted",
            Self::Cached => "cached",
        }
    }
}

/// Why a candidate lost to the selected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowReason {
    /// A higher-precedence member already supplied a candidate for this filename.
    Precedence,
    /// The repository's fallback policy excluded this member's cached candidate in favor of a hosted
    /// member.
    Fallback,
}

impl ShadowReason {
    /// The stable lowercase identifier used in the API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Precedence => "precedence",
            Self::Fallback => "fallback",
        }
    }
}

/// One virtual-repository resolution candidate: the member it came from, its filename and digest, and
/// whether it was selected or shadowed and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowCandidate {
    /// The virtual repository whose resolution produced this candidate.
    pub repository: String,
    /// The resolved project, in the ecosystem's normalized form.
    pub project: String,
    /// The configured member index this candidate came from.
    pub member: String,
    /// Whether the member is a hosted upload store or an upstream cache.
    pub source: ShadowSource,
    /// The distribution filename the candidate offers.
    pub filename: String,
    /// The candidate's content digest, when the ecosystem addresses it by one.
    pub digest: Option<String>,
    /// Whether this candidate is the one the virtual repository serves for its filename.
    pub selected: bool,
    /// Why the candidate was shadowed; absent for the selected candidate.
    pub reason: Option<ShadowReason>,
}

impl ShadowCandidate {
    /// A total-order pagination key within one repository/project query: filenames ascending, the
    /// selected candidate before the ones it shadows, then member name. A cursor is one candidate's
    /// key; the next page returns candidates whose key sorts strictly after it.
    #[must_use]
    pub fn cursor(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}",
            self.filename,
            u8::from(!self.selected),
            self.member,
        )
    }
}

#[cfg(test)]
#[path = "../tests/unit/shadow/tests.rs"]
mod tests;
