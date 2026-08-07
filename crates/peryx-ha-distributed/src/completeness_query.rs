//! The bounded query that reports how complete a replica's distributed analytics picture is.
//!
//! A reader asks for the accepted download totals over a day range, scoped to one repository or across
//! all of them, and wants to know whether the answer covers every producer it should. This folds the
//! converged [`AnalyticsReceiver`] totals into per-day buckets and classifies the range against the
//! expected producers with [`assess`](crate::completeness::assess). Only the expected producers' rows
//! are folded, so a decommissioned or rogue producer outside the topology never inflates the reported
//! totals while the range still reads complete.
//!
//! Completeness is measured against the cluster's own accepted frontier, not the wall clock. The
//! frontier is the highest sealed day any expected producer has been folded through; a producer that has
//! reached it covers the range, one that trails it leaves the range [`Delayed`](Completeness::Delayed),
//! and one with no accepted frontier at all leaves it [`Unavailable`](Completeness::Unavailable). How
//! stale that frontier is against today is reported separately as the lag, so a quiet day never reads as
//! a hole: an idle producer that has caught up to the frontier is complete, and the lag says how old the
//! newest sealed day is. A historical range whose end sits below the frontier requires only coverage
//! through its own end, so a producer still catching up to today can still be complete for the past.

use std::collections::{BTreeMap, BTreeSet};

use peryx_ha::{
    AggregateDelta, AnalyticsCompleteness, AuthorityEpoch, CompletenessError, CompletenessQuery, CompletenessReport,
    DayBucket, ExpectedProducer, ProducerId, ProducerReport,
};

use crate::analytics::{AnalyticsReceiver, DEFAULT_APPLY_LIMITS};
use crate::completeness::{ProducerCoverage, assess};

/// A bounded completeness query.
///
/// It carries the inclusive day range, today for the lag reading, and an optional repository scope. The
/// caller resolves and clamps the range and today off its clock before building this, so the assessment
/// stays a pure function of derived positions.
/// Assess how completely `receiver` covers `query` for the `expected` producers.
///
/// The verdict comes from [`assess`](crate::completeness::assess) over one [`ProducerCoverage`] per
/// expected producer, each required to reach the cluster frontier capped at the range end. An empty
/// expected set is [`Unavailable`](Completeness::Unavailable), the fail-closed answer, since a picture
/// vouched for against zero producers cannot be told apart from one a filter narrowed to nothing.
///
/// When the expected set is non-empty the buckets and totals fold only rows reported by an expected
/// producer, so a producer outside the topology set is left out of both the verdict and the sums rather
/// than counted toward a total the assessment never checked it against. An empty expected set has no
/// topology to exclude against, so it falls back to surfacing every accepted row's totals as a
/// best-effort reading under its [`Unavailable`](Completeness::Unavailable) verdict.
#[must_use]
pub fn assess_completeness(
    receiver: &AnalyticsReceiver,
    expected: &[ExpectedProducer],
    query: &CompletenessQuery,
) -> CompletenessReport {
    let frontier_day = expected
        .iter()
        .filter_map(|expected| receiver.accepted_frontier(&expected.producer))
        .map(|(_, sequence)| i64::try_from(sequence).unwrap_or(i64::MAX))
        .max();
    let required_day = frontier_day.map(|frontier| frontier.min(query.to_day));
    let lag_days = frontier_day.map(|frontier| query.today - frontier);

    let coverages: Vec<ProducerCoverage> = expected
        .iter()
        .map(|expected| {
            coverage(
                expected.producer.clone(),
                receiver.accepted_frontier(&expected.producer),
                required_day,
            )
        })
        .collect();
    let producers: Vec<ProducerReport> = expected
        .iter()
        .zip(&coverages)
        .map(|(expected, coverage)| ProducerReport {
            producer: expected.producer.clone(),
            dc: expected.dc.clone(),
            accepted: coverage.accepted,
            state: assess(std::slice::from_ref(coverage)),
        })
        .collect();

    let expected_producers: BTreeSet<&ProducerId> = expected.iter().map(|expected| &expected.producer).collect();
    let mut folded: BTreeMap<i64, (u64, u64)> = BTreeMap::new();
    let mut totals = AggregateDelta::default();
    for (producer, key, delta) in receiver.accepted_rows() {
        if (!expected_producers.is_empty() && !expected_producers.contains(producer))
            || key.day < query.from_day
            || key.day > query.to_day
            || query.repository.as_deref().is_some_and(|route| key.repository != route)
        {
            continue;
        }
        let bucket = folded.entry(key.day).or_default();
        bucket.0 = bucket.0.saturating_add(delta.downloads);
        bucket.1 = bucket.1.saturating_add(delta.bytes);
        totals = totals.saturating_add(*delta);
    }
    let buckets = folded
        .into_iter()
        .map(|(day, (downloads, bytes))| DayBucket { day, downloads, bytes })
        .collect();

    CompletenessReport {
        completeness: assess(&coverages),
        frontier_day,
        required_day,
        lag_days,
        producers,
        buckets,
        totals,
    }
}

/// Build one producer's coverage: it must reach `required_day` at its own accepted epoch. A producer
/// with no accepted frontier keeps `accepted` `None`, which [`assess`](crate::completeness::assess)
/// reads as [`Unavailable`](Completeness::Unavailable) whatever the required position is.
fn coverage(
    producer: ProducerId,
    accepted: Option<(AuthorityEpoch, u64)>,
    required_day: Option<i64>,
) -> ProducerCoverage {
    let required = match (accepted, required_day) {
        (Some((epoch, _)), Some(day)) => (epoch, u64::try_from(day).unwrap_or(0)),
        _ => (AuthorityEpoch(0), 0),
    };
    ProducerCoverage {
        producer,
        accepted,
        required,
    }
}

/// Completeness reader backed by the distributed analytics apply state.
#[derive(Debug, Default, Clone, Copy)]
pub struct DistributedAnalyticsCompleteness;

impl AnalyticsCompleteness for DistributedAnalyticsCompleteness {
    fn assess(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        expected: &[ExpectedProducer],
        query: &CompletenessQuery,
    ) -> Result<CompletenessReport, CompletenessError> {
        let receiver = match meta.analytics().load_apply().map_err(|_| CompletenessError)? {
            Some(bytes) => AnalyticsReceiver::restore(&bytes, DEFAULT_APPLY_LIMITS).map_err(|_| CompletenessError)?,
            None => AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS),
        };
        Ok(assess_completeness(&receiver, expected, query))
    }
}
