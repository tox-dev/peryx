//! Marking a distributed analytics query complete, delayed, or unavailable.
//!
//! A query over analytics totals spans several producers, one per source DC, and a reader wants to know
//! whether the answer covers every producer for the requested range. Each producer has an accepted
//! frontier, and the query carries the frontier position the range requires of it. This classifies the
//! range against those positions: an expected producer with no accepted frontier leaves the answer
//! [`Unavailable`](Completeness::Unavailable); one whose frontier trails the required position leaves it
//! [`Delayed`](Completeness::Delayed); and a range every producer covers is [`Complete`](Completeness::Complete).
//!
//! The classification is pure and takes derived positions as input. Reading each producer's accepted
//! frontier, mapping the requested time range onto per-producer required positions, filtering topology
//! and actor dimensions, and authorizing the response are the query's wiring, deferred to later work.

pub use peryx_ha::Completeness;
use peryx_ha::{AuthorityEpoch, ProducerId};

/// A point in a producer's accepted stream: the authority epoch and the sequence within it. A higher
/// epoch always sits ahead of any sequence from a lower epoch.
type Position = (AuthorityEpoch, u64);

/// One expected producer's coverage of the requested range: its accepted frontier, or `None` when it
/// has reported none, against the `required` position the range needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerCoverage {
    pub producer: ProducerId,
    pub accepted: Option<Position>,
    pub required: Position,
}

/// Classify how fully `producers` cover the requested range.
///
/// An empty expected set makes the range [`Unavailable`](Completeness::Unavailable), not
/// [`Complete`](Completeness::Complete): vouching for a range against zero consulted producers cannot be
/// told apart from a filter or lookup that silently narrowed to nothing, so the fail-closed answer is
/// the safe one. A producer with no accepted frontier is [`Unavailable`](Completeness::Unavailable) for
/// the same reason, and it outranks everything else. Otherwise a producer whose accepted frontier is
/// below its required position leaves the range [`Delayed`](Completeness::Delayed), and a non-empty
/// range every producer covers is [`Complete`](Completeness::Complete). The required boundary is
/// inclusive: an accepted frontier equal to the required position covers it.
#[must_use]
pub fn assess(producers: &[ProducerCoverage]) -> Completeness {
    if producers.is_empty() {
        return Completeness::Unavailable;
    }
    let mut delayed = false;
    for coverage in producers {
        match coverage.accepted {
            None => return Completeness::Unavailable,
            Some(accepted) if accepted < coverage.required => delayed = true,
            Some(_) => {}
        }
    }
    if delayed {
        Completeness::Delayed
    } else {
        Completeness::Complete
    }
}
