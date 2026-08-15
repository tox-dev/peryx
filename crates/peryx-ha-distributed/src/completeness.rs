pub use peryx_ha::Completeness;
use peryx_ha::{AuthorityEpoch, ProducerId};

/// A higher epoch ranks above every sequence from a lower epoch.
type Position = (AuthorityEpoch, u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerCoverage {
    pub producer: ProducerId,
    pub accepted: Option<Position>,
    pub required: Position,
}

/// Fails closed for missing producers or frontiers and treats the accepted boundary as inclusive.
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
