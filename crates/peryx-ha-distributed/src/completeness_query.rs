//! Completeness uses accepted producer frontiers. Totals exclude producers outside the expected
//! topology.
//!
//! Required coverage is the cluster frontier capped at the query end. Lag is a distinct frontier-age
//! value, so quiet days do not become holes and historical ranges do not require current coverage.

use std::collections::{BTreeMap, BTreeSet};

use peryx_ha::{
    AggregateDelta, AnalyticsCompleteness, AnalyticsSnapshotStore, AuthorityEpoch, CompletenessError,
    CompletenessQuery, CompletenessReport, DayBucket, ExpectedProducer, ProducerId, ProducerReport,
};

use crate::analytics::{AnalyticsReceiver, DEFAULT_APPLY_LIMITS};
use crate::completeness::{ProducerCoverage, assess};

/// Caps required producer coverage at the query end and excludes unexpected producers from totals.
///
/// An empty expected set reports [`Unavailable`](peryx_ha::Completeness::Unavailable) with all accepted totals.
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

#[derive(Debug, Default, Clone, Copy)]
pub struct DistributedAnalyticsCompleteness;

impl AnalyticsCompleteness for DistributedAnalyticsCompleteness {
    fn assess(
        &self,
        store: &dyn AnalyticsSnapshotStore,
        expected: &[ExpectedProducer],
        query: &CompletenessQuery,
    ) -> Result<CompletenessReport, CompletenessError> {
        let receiver = match store.load_analytics_snapshot().map_err(|_| CompletenessError)? {
            Some(bytes) => AnalyticsReceiver::restore(&bytes, DEFAULT_APPLY_LIMITS).map_err(|_| CompletenessError)?,
            None => AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS),
        };
        Ok(assess_completeness(&receiver, expected, query))
    }
}
