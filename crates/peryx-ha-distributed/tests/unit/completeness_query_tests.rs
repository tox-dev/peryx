use crate::{
    AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, AnalyticsReceiver, AuthorityEpoch, Completeness,
    CompletenessQuery, DEFAULT_APPLY_LIMITS, DayBucket, ExpectedProducer, IntervalId, ProducerId, assess_completeness,
};

fn batch(producer: &str, epoch: u64, day: i64, rows: &[(&str, &str, u64, u64)]) -> AnalyticsBatch {
    AnalyticsBatch {
        interval: IntervalId {
            producer: ProducerId(producer.to_owned()),
            epoch: AuthorityEpoch(epoch),
            sequence: u64::try_from(day).unwrap(),
        },
        rows: rows
            .iter()
            .map(|(repository, project, downloads, bytes)| AggregateRow {
                key: AggregateKey {
                    day,
                    repository: (*repository).to_owned(),
                    project: (*project).to_owned(),
                    version: String::new(),
                    source: String::new(),
                },
                delta: AggregateDelta {
                    downloads: *downloads,
                    bytes: *bytes,
                },
            })
            .collect(),
    }
}

fn receiver(batches: &[AnalyticsBatch]) -> AnalyticsReceiver {
    let mut receiver = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);
    for batch in batches {
        receiver.apply(batch).unwrap();
    }
    receiver
}

fn expected(members: &[(&str, &str)]) -> Vec<ExpectedProducer> {
    members
        .iter()
        .map(|(producer, dc)| ExpectedProducer {
            producer: ProducerId((*producer).to_owned()),
            dc: (*dc).to_owned(),
        })
        .collect()
}

fn query(from_day: i64, to_day: i64, now: i64, repository: Option<&str>) -> CompletenessQuery {
    CompletenessQuery {
        from_day,
        to_day,
        today: now,
        repository: repository.map(str::to_owned),
    }
}

fn producer_states(report: &crate::CompletenessReport) -> Vec<(String, Completeness)> {
    report
        .producers
        .iter()
        .map(|producer| (producer.producer.0.clone(), producer.state))
        .collect()
}

#[test]
fn test_every_producer_at_the_frontier_is_complete() {
    let batches = [
        batch("east", 1, 9, &[("alpha", "flask", 2, 20)]),
        batch("east", 1, 10, &[("alpha", "flask", 3, 30)]),
        batch("west", 1, 10, &[("alpha", "numpy", 5, 50)]),
    ];
    let report = assess_completeness(
        &receiver(&batches),
        &expected(&[("east", "east-dc"), ("west", "west-dc")]),
        &query(0, 10, 11, None),
    );

    assert_eq!(report.completeness, Completeness::Complete);
    assert_eq!(report.frontier_day, Some(10));
    assert_eq!(report.required_day, Some(10));
    assert_eq!(report.lag_days, Some(1));
}

#[test]
fn test_complete_totals_equal_the_sum_of_accepted_aggregates() {
    let batches = [
        batch("east", 1, 9, &[("alpha", "flask", 2, 20)]),
        batch("east", 1, 10, &[("alpha", "flask", 3, 30)]),
        batch("west", 1, 10, &[("alpha", "numpy", 5, 50)]),
    ];
    let report = assess_completeness(
        &receiver(&batches),
        &expected(&[("east", "east-dc"), ("west", "west-dc")]),
        &query(0, 10, 11, None),
    );

    assert_eq!(
        report.totals,
        AggregateDelta {
            downloads: 10,
            bytes: 100
        }
    );
    assert_eq!(
        report.buckets,
        vec![
            DayBucket {
                day: 9,
                downloads: 2,
                bytes: 20
            },
            DayBucket {
                day: 10,
                downloads: 8,
                bytes: 80
            },
        ]
    );
}

#[test]
fn test_a_trailing_producer_is_delayed() {
    let batches = [
        batch("east", 1, 10, &[("alpha", "flask", 3, 30)]),
        batch("west", 1, 8, &[("alpha", "numpy", 5, 50)]),
    ];
    let report = assess_completeness(
        &receiver(&batches),
        &expected(&[("east", "east-dc"), ("west", "west-dc")]),
        &query(0, 10, 11, None),
    );

    assert_eq!(report.completeness, Completeness::Delayed);
    assert_eq!(report.required_day, Some(10));
    assert_eq!(
        producer_states(&report),
        vec![
            ("east".to_owned(), Completeness::Complete),
            ("west".to_owned(), Completeness::Delayed),
        ]
    );
}

#[test]
fn test_an_expected_producer_with_no_frontier_is_unavailable() {
    let batches = [
        batch("east", 1, 10, &[("alpha", "flask", 3, 30)]),
        batch("west", 1, 10, &[("alpha", "numpy", 5, 50)]),
    ];
    let report = assess_completeness(
        &receiver(&batches),
        &expected(&[("east", "east-dc"), ("west", "west-dc"), ("south", "south-dc")]),
        &query(0, 10, 11, None),
    );

    assert_eq!(report.completeness, Completeness::Unavailable);
    assert_eq!(
        producer_states(&report),
        vec![
            ("east".to_owned(), Completeness::Complete),
            ("west".to_owned(), Completeness::Complete),
            ("south".to_owned(), Completeness::Unavailable),
        ]
    );
    let south = report
        .producers
        .iter()
        .find(|report| report.producer.0 == "south")
        .unwrap();
    assert_eq!(south.accepted, None);
    assert_eq!(south.dc, "south-dc");
}

#[test]
fn test_a_missing_producer_outranks_a_delayed_one() {
    let batches = [
        batch("east", 1, 10, &[("alpha", "flask", 3, 30)]),
        batch("west", 1, 8, &[("alpha", "numpy", 5, 50)]),
    ];
    let report = assess_completeness(
        &receiver(&batches),
        &expected(&[("east", "east-dc"), ("west", "west-dc"), ("south", "south-dc")]),
        &query(0, 10, 11, None),
    );

    assert_eq!(report.completeness, Completeness::Unavailable);
}

#[test]
fn test_an_empty_expected_set_is_unavailable() {
    let report = assess_completeness(
        &receiver(&[batch("east", 1, 10, &[("alpha", "flask", 3, 30)])]),
        &expected(&[]),
        &query(0, 10, 11, None),
    );

    assert_eq!(report.completeness, Completeness::Unavailable);
    assert_eq!(report.frontier_day, None);
    assert_eq!(report.required_day, None);
    assert_eq!(report.lag_days, None);
    assert!(report.producers.is_empty());
    assert_eq!(
        report.totals,
        AggregateDelta {
            downloads: 3,
            bytes: 30
        }
    );
}

#[test]
fn test_no_producer_has_delivered_leaves_the_frontier_absent() {
    let report = assess_completeness(
        &receiver(&[]),
        &expected(&[("east", "east-dc")]),
        &query(0, 10, 11, None),
    );

    assert_eq!(report.completeness, Completeness::Unavailable);
    assert_eq!(report.frontier_day, None);
    assert_eq!(report.required_day, None);
    assert_eq!(report.lag_days, None);
    assert!(report.buckets.is_empty());
    assert_eq!(report.totals, AggregateDelta::default());
}

#[test]
fn test_a_historical_range_below_the_frontier_covers_a_laggard() {
    let batches = [
        batch("east", 1, 20, &[("alpha", "flask", 3, 30)]),
        batch("west", 1, 10, &[("alpha", "numpy", 5, 50)]),
    ];
    let report = assess_completeness(
        &receiver(&batches),
        &expected(&[("east", "east-dc"), ("west", "west-dc")]),
        &query(0, 8, 21, None),
    );

    assert_eq!(report.completeness, Completeness::Complete);
    assert_eq!(report.frontier_day, Some(20));
    assert_eq!(report.required_day, Some(8));
    assert_eq!(report.lag_days, Some(1));
}

#[test]
fn test_repository_scope_confines_the_totals() {
    let batches = [batch(
        "east",
        1,
        10,
        &[("alpha", "flask", 3, 30), ("other", "numpy", 5, 50)],
    )];
    let report = assess_completeness(
        &receiver(&batches),
        &expected(&[("east", "east-dc")]),
        &query(0, 10, 11, Some("alpha")),
    );

    assert_eq!(
        report.totals,
        AggregateDelta {
            downloads: 3,
            bytes: 30
        }
    );
    assert_eq!(
        report.buckets,
        vec![DayBucket {
            day: 10,
            downloads: 3,
            bytes: 30
        }]
    );
}

#[test]
fn test_the_day_range_bounds_the_buckets() {
    let batches = [
        batch("east", 1, 5, &[("alpha", "flask", 1, 10)]),
        batch("east", 1, 10, &[("alpha", "flask", 2, 20)]),
        batch("east", 1, 15, &[("alpha", "flask", 4, 40)]),
    ];
    let report = assess_completeness(
        &receiver(&batches),
        &expected(&[("east", "east-dc")]),
        &query(8, 12, 16, None),
    );

    assert_eq!(
        report.buckets,
        vec![DayBucket {
            day: 10,
            downloads: 2,
            bytes: 20
        }]
    );
    assert_eq!(
        report.totals,
        AggregateDelta {
            downloads: 2,
            bytes: 20
        }
    );
}

#[test]
fn test_a_restart_under_a_higher_epoch_leads_the_frontier() {
    let batches = [
        batch("east", 1, 10, &[("alpha", "flask", 3, 30)]),
        batch("east", 2, 4, &[("alpha", "flask", 1, 10)]),
    ];
    let report = assess_completeness(
        &receiver(&batches),
        &expected(&[("east", "east-dc")]),
        &query(0, 4, 5, None),
    );

    assert_eq!(report.producers[0].accepted, Some((AuthorityEpoch(2), 4)));
    assert_eq!(report.completeness, Completeness::Complete);
}

#[test]
fn test_an_unexpected_producer_is_left_out_of_the_totals() {
    let batches = [
        batch("east", 1, 10, &[("alpha", "flask", 10, 100)]),
        batch("rogue", 1, 10, &[("alpha", "flask", 1_000, 10_000)]),
    ];
    let report = assess_completeness(
        &receiver(&batches),
        &expected(&[("east", "east-dc")]),
        &query(0, 10, 11, None),
    );

    assert_eq!(report.completeness, Completeness::Complete);
    assert_eq!(
        report.totals,
        AggregateDelta {
            downloads: 10,
            bytes: 100
        }
    );
    assert_eq!(
        report.buckets,
        vec![DayBucket {
            day: 10,
            downloads: 10,
            bytes: 100
        }]
    );
    assert_eq!(
        producer_states(&report),
        vec![("east".to_owned(), Completeness::Complete)]
    );
}

#[test]
fn test_a_duplicate_delivery_folds_the_totals_once() {
    let delivery = batch("east", 1, 10, &[("alpha", "flask", 3, 30)]);
    let mut receiver = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);
    receiver.apply(&delivery).unwrap();
    receiver.apply(&delivery).unwrap();

    let report = assess_completeness(&receiver, &expected(&[("east", "east-dc")]), &query(0, 10, 11, None));

    assert_eq!(
        report.totals,
        AggregateDelta {
            downloads: 3,
            bytes: 30
        }
    );
}

#[test]
fn test_a_restored_receiver_still_excludes_an_unexpected_producer() {
    let batches = [
        batch("east", 1, 10, &[("alpha", "flask", 10, 100)]),
        batch("rogue", 1, 10, &[("alpha", "flask", 1_000, 10_000)]),
    ];
    let members = expected(&[("east", "east-dc")]);
    let query = query(0, 10, 11, None);
    let restored = AnalyticsReceiver::restore(&receiver(&batches).encode(), DEFAULT_APPLY_LIMITS).unwrap();

    assert_eq!(
        assess_completeness(&restored, &members, &query).totals,
        AggregateDelta {
            downloads: 10,
            bytes: 100
        }
    );
}

#[test]
fn test_a_restored_receiver_reports_the_same_completeness() {
    let batches = [
        batch("east", 1, 10, &[("alpha", "flask", 3, 30)]),
        batch("west", 1, 8, &[("alpha", "numpy", 5, 50)]),
    ];
    let members = expected(&[("east", "east-dc"), ("west", "west-dc")]);
    let query = query(0, 10, 11, None);
    let original = receiver(&batches);
    let restored = AnalyticsReceiver::restore(&original.encode(), DEFAULT_APPLY_LIMITS).unwrap();

    assert_eq!(
        assess_completeness(&restored, &members, &query),
        assess_completeness(&original, &members, &query),
    );
}
