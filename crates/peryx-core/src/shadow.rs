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
mod tests {
    use super::{ShadowCandidate, ShadowReason, ShadowSource};

    fn candidate(filename: &str, member: &str, selected: bool) -> ShadowCandidate {
        ShadowCandidate {
            repository: "root/alpha".to_owned(),
            project: "flask".to_owned(),
            member: member.to_owned(),
            source: ShadowSource::Hosted,
            filename: filename.to_owned(),
            digest: Some("sha256:abc".to_owned()),
            selected,
            reason: (!selected).then_some(ShadowReason::Precedence),
        }
    }

    #[test]
    fn test_source_as_str_names_each_member_class() {
        assert_eq!(ShadowSource::Hosted.as_str(), "hosted");
        assert_eq!(ShadowSource::Cached.as_str(), "cached");
    }

    #[test]
    fn test_reason_as_str_names_each_exclusion() {
        assert_eq!(ShadowReason::Precedence.as_str(), "precedence");
        assert_eq!(ShadowReason::Fallback.as_str(), "fallback");
    }

    #[test]
    fn test_cursor_orders_the_selected_candidate_before_the_shadowed_one() {
        let selected = candidate("flask-1.0.bin", "hosted", true).cursor();
        let shadowed = candidate("flask-1.0.bin", "alpha", false).cursor();
        assert!(selected < shadowed, "selected {selected:?} shadowed {shadowed:?}");
        assert_eq!(selected, "flask-1.0.bin\u{1f}0\u{1f}hosted");
    }

    #[test]
    fn test_cursor_orders_by_filename_first() {
        let earlier = candidate("flask-1.0.bin", "z", true).cursor();
        let later = candidate("flask-2.0.bin", "a", true).cursor();
        assert!(earlier < later);
    }
}
