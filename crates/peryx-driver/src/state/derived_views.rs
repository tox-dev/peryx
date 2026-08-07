//! The readable-frontier calculation gating replica reads on derived views.
//!
//! A replica applies authoritative metadata ahead of the views it derives from it - the search
//! index, the rendered-page cache, the protocol responses. Serving a record before every required
//! view reflects it would mix new metadata with a stale view, so a replica must expose metadata only up
//! to the *readable frontier*: the lowest serial every required view has applied. Each view records
//! how far it has caught up through [`set_view_frontier`](peryx_storage::meta::MetaStore::set_view_frontier);
//! this module folds those durable frontiers and the authoritative serial into one readable serial
//! and names the view holding it back.
//!
//! This module computes the frontier and the replica loop exports it as
//! `peryx_ha_distributed_readable_serial`. Each ecosystem holds a response until every required view
//! reflects the serial it carries, whether
//! served from a hosted index directly or surfaced through a virtual index that layers one.
//!
//! The registry is ecosystem-neutral: a view is a stable name, and an ecosystem crate wires its own
//! invalidation and rebuild into the frontier it advances without this module learning its format.

use std::collections::BTreeMap;

pub use peryx_search::SEARCH_VIEW;

/// The views a replica must have caught up before it exposes a serial.
///
/// An ecosystem view an ecosystem crate rebuilds still advances its own frontier; this list is the
/// set readability waits on regardless of ecosystem.
pub const REQUIRED_VIEWS: &[&str] = &[SEARCH_VIEW];

/// A required view a replica could not rebuild while applying a page, naming the view that holds the
/// readable frontier back.
///
/// A driver returns it from
/// [`apply_replicated_changes`](crate::serving::EcosystemDriver::apply_replicated_changes) so the neutral
/// apply path leaves the frontier where it was and reports the view rather than advancing past a serial a
/// stale view does not reflect. The lazy full-index refresh a later search runs is the recovery path: it
/// re-derives every document and advances the frontier once the failing input is readable again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewBlock {
    /// The name of the required view whose rebuild failed.
    pub view: String,
}

/// The highest metadata serial a replica may expose, and the view holding it back.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadableFrontier {
    /// The serial safe to serve: metadata at or below it is reflected by every required view.
    pub serial: u64,
    /// The required view whose lag pins `serial` below the authoritative frontier, or `None` when
    /// every required view has caught up to it.
    pub blocking: Option<String>,
}

/// The readable frontier for an `authority` serial given each required view's applied `frontiers`.
///
/// A view absent from `frontiers` has applied nothing, so it pins readability to zero. The result is
/// the minimum of `authority` and every required view's frontier, with the view that owns that
/// minimum named as the one to catch up next.
#[must_use]
pub fn readable_frontier(authority: u64, frontiers: &BTreeMap<String, u64>, required: &[&str]) -> ReadableFrontier {
    let mut serial = authority;
    let mut blocking = None;
    for &view in required {
        let applied = frontiers.get(view).copied().unwrap_or(0);
        if applied < serial {
            serial = applied;
            blocking = Some(view.to_owned());
        }
    }
    ReadableFrontier { serial, blocking }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{REQUIRED_VIEWS, ReadableFrontier, SEARCH_VIEW, readable_frontier};

    #[test]
    fn test_no_required_views_expose_the_whole_authority_frontier() {
        assert_eq!(
            readable_frontier(9, &BTreeMap::new(), &[]),
            ReadableFrontier {
                serial: 9,
                blocking: None,
            }
        );
    }

    #[test]
    fn test_a_missing_view_pins_readability_to_zero_and_names_itself() {
        assert_eq!(
            readable_frontier(5, &BTreeMap::new(), REQUIRED_VIEWS),
            ReadableFrontier {
                serial: 0,
                blocking: Some(SEARCH_VIEW.to_owned()),
            }
        );
    }

    #[test]
    fn test_a_caught_up_view_exposes_the_authority_frontier() {
        let frontiers = BTreeMap::from([(SEARCH_VIEW.to_owned(), 5)]);
        assert_eq!(
            readable_frontier(5, &frontiers, REQUIRED_VIEWS),
            ReadableFrontier {
                serial: 5,
                blocking: None,
            }
        );
    }

    #[test]
    fn test_the_lagging_view_owns_the_minimum_and_holds_the_frontier() {
        let frontiers = BTreeMap::from([("search".to_owned(), 2), ("cache".to_owned(), 4)]);
        assert_eq!(
            readable_frontier(6, &frontiers, &["cache", "search"]),
            ReadableFrontier {
                serial: 2,
                blocking: Some("search".to_owned()),
            }
        );
    }

    #[test]
    fn test_a_view_ahead_of_the_authority_never_lifts_readability_above_it() {
        let frontiers = BTreeMap::from([(SEARCH_VIEW.to_owned(), 8)]);
        assert_eq!(
            readable_frontier(3, &frontiers, REQUIRED_VIEWS),
            ReadableFrontier {
                serial: 3,
                blocking: None,
            }
        );
    }
}
