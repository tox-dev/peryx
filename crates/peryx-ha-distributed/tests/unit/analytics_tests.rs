use crate::{
    AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, AnalyticsReceiver, ApplyError, ApplyLimits,
    ApplyOutcome, ApplyState, AuthorityEpoch, DEFAULT_APPLY_LIMITS, Frontier, IntervalId, ProducerId, SnapshotError,
};

fn key(day: i64, version: &str, source: &str) -> AggregateKey {
    AggregateKey {
        day,
        repository: "alpha".to_owned(),
        project: "flask".to_owned(),
        version: version.to_owned(),
        source: source.to_owned(),
    }
}

fn interval(producer: &str, epoch: u64, sequence: u64) -> IntervalId {
    IntervalId {
        producer: ProducerId(producer.to_owned()),
        epoch: AuthorityEpoch(epoch),
        sequence,
    }
}

fn batch(interval: IntervalId, rows: &[(AggregateKey, u64, u64)]) -> AnalyticsBatch {
    AnalyticsBatch {
        interval,
        rows: rows
            .iter()
            .map(|(key, downloads, bytes)| AggregateRow {
                key: key.clone(),
                delta: AggregateDelta {
                    downloads: *downloads,
                    bytes: *bytes,
                },
            })
            .collect(),
    }
}

#[test]
fn test_apply_folds_additive_rows_into_accepted_totals() {
    let mut state = ApplyState::new(ApplyLimits::default());
    let dimension = key(20_000, "3.0", "upstream");
    let outcome = state
        .apply(&batch(interval("east", 1, 1), &[(dimension.clone(), 2, 50)]))
        .unwrap();

    assert_eq!(outcome, ApplyOutcome::Applied);
    assert_eq!(
        state.total(&dimension),
        AggregateDelta {
            downloads: 2,
            bytes: 50
        }
    );
    assert_eq!(state.retained_intervals(), 1);
}

#[test]
fn test_apply_sums_rows_across_distinct_intervals() {
    let mut state = ApplyState::new(ApplyLimits::default());
    let dimension = key(20_000, "3.0", "upstream");
    state
        .apply(&batch(interval("east", 1, 1), &[(dimension.clone(), 2, 50)]))
        .unwrap();
    state
        .apply(&batch(interval("east", 1, 2), &[(dimension.clone(), 3, 70)]))
        .unwrap();

    assert_eq!(
        state.total(&dimension),
        AggregateDelta {
            downloads: 5,
            bytes: 120
        }
    );
}

#[test]
fn test_total_sums_a_dimension_across_producers() {
    let mut state = ApplyState::new(ApplyLimits::default());
    let dimension = key(20_000, "3.0", "upstream");
    state
        .apply(&batch(interval("east", 1, 1), &[(dimension.clone(), 2, 50)]))
        .unwrap();
    state
        .apply(&batch(interval("west", 1, 1), &[(dimension.clone(), 3, 70)]))
        .unwrap();

    assert_eq!(
        state.total(&dimension),
        AggregateDelta {
            downloads: 5,
            bytes: 120
        }
    );
}

#[test]
fn test_apply_rejects_a_duplicate_interval_without_changing_totals() {
    let mut state = ApplyState::new(ApplyLimits::default());
    let dimension = key(20_000, "3.0", "upstream");
    state
        .apply(&batch(interval("east", 1, 1), &[(dimension.clone(), 2, 50)]))
        .unwrap();

    let replay = state
        .apply(&batch(interval("east", 1, 1), &[(dimension.clone(), 999, 999)]))
        .unwrap();

    assert_eq!(replay, ApplyOutcome::Duplicate);
    assert_eq!(
        state.total(&dimension),
        AggregateDelta {
            downloads: 2,
            bytes: 50
        }
    );
    assert_eq!(state.retained_intervals(), 1);
}

#[test]
fn test_reordered_delivery_converges_to_the_same_sum() {
    let dimension = key(20_000, "3.0", "upstream");
    let first = batch(interval("east", 1, 1), &[(dimension.clone(), 2, 50)]);
    let second = batch(interval("east", 1, 2), &[(dimension.clone(), 3, 70)]);

    let mut forward = ApplyState::new(ApplyLimits::default());
    forward.apply(&first).unwrap();
    forward.apply(&second).unwrap();

    let mut reversed = ApplyState::new(ApplyLimits::default());
    reversed.apply(&second).unwrap();
    reversed.apply(&first).unwrap();

    assert_eq!(forward.total(&dimension), reversed.total(&dimension));
}

#[test]
fn test_producer_restart_under_a_new_epoch_applies_as_a_distinct_interval() {
    let mut state = ApplyState::new(ApplyLimits::default());
    let dimension = key(20_000, "3.0", "upstream");
    state
        .apply(&batch(interval("east", 1, 1), &[(dimension.clone(), 2, 50)]))
        .unwrap();

    let after_restart = state
        .apply(&batch(interval("east", 2, 1), &[(dimension.clone(), 4, 90)]))
        .unwrap();

    assert_eq!(after_restart, ApplyOutcome::Applied);
    assert_eq!(
        state.total(&dimension),
        AggregateDelta {
            downloads: 6,
            bytes: 140
        }
    );
}

#[test]
fn test_producer_restart_replaying_the_same_interval_is_dropped() {
    let mut state = ApplyState::new(ApplyLimits::default());
    let dimension = key(20_000, "3.0", "upstream");
    let same = batch(interval("east", 1, 1), &[(dimension.clone(), 2, 50)]);
    state.apply(&same).unwrap();

    let after_restart = state.apply(&same).unwrap();

    assert_eq!(after_restart, ApplyOutcome::Duplicate);
    assert_eq!(
        state.total(&dimension),
        AggregateDelta {
            downloads: 2,
            bytes: 50
        }
    );
}

#[test]
fn test_apply_saturates_totals_at_u64_max() {
    let mut state = ApplyState::new(ApplyLimits::default());
    let dimension = key(20_000, "3.0", "upstream");
    state
        .apply(&batch(
            interval("east", 1, 1),
            &[(dimension.clone(), u64::MAX, u64::MAX - 1)],
        ))
        .unwrap();
    state
        .apply(&batch(interval("east", 1, 2), &[(dimension.clone(), 5, 5)]))
        .unwrap();

    assert_eq!(
        state.total(&dimension),
        AggregateDelta {
            downloads: u64::MAX,
            bytes: u64::MAX,
        }
    );
}

#[test]
fn test_apply_rejects_a_batch_over_the_row_limit() {
    let limits = ApplyLimits {
        max_rows_per_batch: 1,
        max_retained_intervals: 8,
    };
    let mut state = ApplyState::new(limits);
    let dimension = key(20_000, "3.0", "upstream");
    let wide = batch(
        interval("east", 1, 1),
        &[(dimension.clone(), 1, 1), (key(20_001, "3.0", "upstream"), 1, 1)],
    );

    let error = state.apply(&wide).unwrap_err();

    assert_eq!(error, ApplyError::BatchTooLarge { limit: 1, actual: 2 });
    assert_eq!(state.total(&dimension), AggregateDelta::default());
    assert_eq!(state.retained_intervals(), 0);
}

#[test]
fn test_batch_too_large_message_names_the_row_limit() {
    let error = ApplyError::BatchTooLarge { limit: 1, actual: 2 };

    assert_eq!(
        error.to_string(),
        "analytics batch carries 2 rows, over the 1 row apply limit"
    );
}

#[test]
fn test_apply_refuses_a_new_interval_when_the_replay_set_is_full() {
    let limits = ApplyLimits {
        max_rows_per_batch: 8,
        max_retained_intervals: 1,
    };
    let mut state = ApplyState::new(limits);
    let dimension = key(20_000, "3.0", "upstream");
    state
        .apply(&batch(interval("east", 1, 1), &[(dimension.clone(), 2, 50)]))
        .unwrap();

    let error = state
        .apply(&batch(interval("east", 1, 2), &[(dimension.clone(), 3, 70)]))
        .unwrap_err();

    assert_eq!(error, ApplyError::RetentionFull { limit: 1 });
    assert_eq!(
        state.total(&dimension),
        AggregateDelta {
            downloads: 2,
            bytes: 50
        }
    );
    assert_eq!(state.retained_intervals(), 1);
}

#[test]
fn test_retention_full_message_names_the_limit() {
    let error = ApplyError::RetentionFull { limit: 1 };

    assert_eq!(
        error.to_string(),
        "replay set holds 1 intervals; compact past the durable frontier before applying more"
    );
}

#[test]
fn test_compact_releases_intervals_the_frontier_covers() {
    let mut state = ApplyState::new(ApplyLimits::default());
    let dimension = key(20_000, "3.0", "upstream");
    state
        .apply(&batch(interval("east", 1, 1), &[(dimension.clone(), 2, 50)]))
        .unwrap();
    state
        .apply(&batch(interval("east", 1, 2), &[(dimension.clone(), 3, 70)]))
        .unwrap();

    let mut frontier = Frontier::default();
    frontier.acknowledge(ProducerId("east".to_owned()), AuthorityEpoch(1), 1);
    state.compact(&frontier);

    assert_eq!(state.retained_intervals(), 1);
    assert_eq!(
        state.total(&dimension),
        AggregateDelta {
            downloads: 5,
            bytes: 120
        }
    );

    let replayed = state
        .apply(&batch(interval("east", 1, 1), &[(dimension.clone(), 2, 50)]))
        .unwrap();
    assert_eq!(replayed, ApplyOutcome::Applied);
    assert_eq!(
        state.total(&dimension),
        AggregateDelta {
            downloads: 7,
            bytes: 170
        }
    );
}

#[test]
fn test_compact_keeps_intervals_above_the_frontier() {
    let mut state = ApplyState::new(ApplyLimits::default());
    let dimension = key(20_000, "3.0", "upstream");
    state
        .apply(&batch(interval("east", 1, 2), &[(dimension.clone(), 3, 70)]))
        .unwrap();

    let mut frontier = Frontier::default();
    frontier.acknowledge(ProducerId("east".to_owned()), AuthorityEpoch(1), 1);
    state.compact(&frontier);

    let replayed = state
        .apply(&batch(interval("east", 1, 2), &[(dimension.clone(), 3, 70)]))
        .unwrap();
    assert_eq!(replayed, ApplyOutcome::Duplicate);
    assert_eq!(
        state.total(&dimension),
        AggregateDelta {
            downloads: 3,
            bytes: 70
        }
    );
}

#[test]
fn test_compact_against_an_empty_frontier_keeps_every_interval() {
    let mut state = ApplyState::new(ApplyLimits::default());
    state
        .apply(&batch(
            interval("east", 1, 1),
            &[(key(20_000, "3.0", "upstream"), 2, 50)],
        ))
        .unwrap();

    state.compact(&Frontier::default());

    assert_eq!(state.retained_intervals(), 1);
}

#[test]
fn test_frontier_acknowledge_keeps_the_highest_sequence() {
    let mut state = ApplyState::new(ApplyLimits::default());
    state
        .apply(&batch(
            interval("east", 1, 3),
            &[(key(20_000, "3.0", "upstream"), 1, 1)],
        ))
        .unwrap();

    let mut frontier = Frontier::default();
    frontier.acknowledge(ProducerId("east".to_owned()), AuthorityEpoch(1), 5);
    frontier.acknowledge(ProducerId("east".to_owned()), AuthorityEpoch(1), 2);
    state.compact(&frontier);

    assert_eq!(state.retained_intervals(), 0);
}

#[test]
fn test_snapshot_round_trip_preserves_totals_and_replay_protection() {
    let mut state = ApplyState::new(ApplyLimits::default());
    let dimension = key(20_000, "3.0", "upstream");
    state
        .apply(&batch(interval("east", 1, 1), &[(dimension.clone(), 2, 50)]))
        .unwrap();

    let restored = ApplyState::restore(&state.encode(), ApplyLimits::default()).unwrap();

    assert_eq!(
        restored.total(&dimension),
        AggregateDelta {
            downloads: 2,
            bytes: 50
        }
    );
    let mut restored = restored;
    let replay = restored
        .apply(&batch(interval("east", 1, 1), &[(dimension.clone(), 2, 50)]))
        .unwrap();
    assert_eq!(replay, ApplyOutcome::Duplicate);
    assert_eq!(
        restored.total(&dimension),
        AggregateDelta {
            downloads: 2,
            bytes: 50
        }
    );
}

#[test]
fn test_restore_rejects_an_unknown_schema() {
    let snapshot = br#"{"schema":999,"totals":[],"applied":[]}"#;

    let error = ApplyState::restore(snapshot, ApplyLimits::default()).unwrap_err();

    assert!(matches!(
        error,
        SnapshotError::UnsupportedSchema {
            expected: 2,
            found: 999
        }
    ));
    assert_eq!(
        error.to_string(),
        "analytics apply snapshot schema 999 is not the 2 this build restores"
    );
}

#[test]
fn test_restore_rejects_malformed_bytes() {
    let error = ApplyState::restore(b"not json", ApplyLimits::default()).unwrap_err();

    assert!(matches!(error, SnapshotError::Malformed(_)));
    assert!(error.to_string().starts_with("analytics apply snapshot is malformed"));
}

#[test]
fn test_default_apply_limits_match_the_shared_bound() {
    let limits = ApplyLimits::default();

    assert_eq!(limits, DEFAULT_APPLY_LIMITS);
    assert_eq!(limits.max_rows_per_batch, 16_384);
    assert_eq!(limits.max_retained_intervals, 65_536);
}

#[test]
fn test_analytics_batch_round_trips_through_its_wire_form() {
    let original = batch(interval("east", 1, 1), &[(key(20_000, "3.0", "upstream"), 2, 50)]);

    let encoded = serde_json::to_vec(&original).unwrap();
    let decoded: AnalyticsBatch = serde_json::from_slice(&encoded).unwrap();

    assert_eq!(decoded, original);
}

#[test]
fn test_receiver_folds_a_new_interval_and_dedups_a_replay() {
    let mut receiver = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);
    let one = batch(interval("dc-a", 1, 5), &[(key(5, "1.0", ""), 3, 300)]);

    assert_eq!(receiver.apply(&one).unwrap(), ApplyOutcome::Applied);
    assert_eq!(receiver.apply(&one).unwrap(), ApplyOutcome::Duplicate);

    assert_eq!(
        receiver.total(&key(5, "1.0", "")),
        AggregateDelta {
            downloads: 3,
            bytes: 300
        }
    );
    assert_eq!(receiver.retained_intervals(), 1);
}

#[test]
fn test_receiver_cursor_tracks_the_highest_day_per_producer() {
    let mut receiver = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);
    assert_eq!(receiver.after_day(&ProducerId("dc-a".to_owned())), -1);

    receiver
        .apply(&batch(interval("dc-a", 1, 7), &[(key(7, "1.0", ""), 1, 10)]))
        .unwrap();
    receiver
        .apply(&batch(interval("dc-b", 1, 2), &[(key(2, "1.0", ""), 1, 10)]))
        .unwrap();

    assert_eq!(receiver.after_day(&ProducerId("dc-a".to_owned())), 7);
    assert_eq!(receiver.after_day(&ProducerId("dc-b".to_owned())), 2);
}

#[test]
fn test_receiver_accepted_frontier_keeps_the_highest_position_and_survives_a_restore() {
    let mut receiver = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);
    assert_eq!(receiver.accepted_frontier(&ProducerId("dc-a".to_owned())), None);

    receiver
        .apply(&batch(interval("dc-a", 1, 7), &[(key(7, "1.0", ""), 1, 10)]))
        .unwrap();
    // A duplicate never disturbs the frontier, and a restart under a higher epoch leads it even at a
    // lower sequence, since the epoch orders ahead of any sequence below it.
    receiver
        .apply(&batch(interval("dc-a", 1, 7), &[(key(7, "1.0", ""), 1, 10)]))
        .unwrap();
    receiver
        .apply(&batch(interval("dc-a", 2, 3), &[(key(3, "1.0", ""), 1, 10)]))
        .unwrap();

    assert_eq!(
        receiver.accepted_frontier(&ProducerId("dc-a".to_owned())),
        Some((AuthorityEpoch(2), 3))
    );
    let restored = AnalyticsReceiver::restore(&receiver.encode(), DEFAULT_APPLY_LIMITS).unwrap();
    assert_eq!(
        restored.accepted_frontier(&ProducerId("dc-a".to_owned())),
        Some((AuthorityEpoch(2), 3))
    );
}

#[test]
fn test_receiver_converges_under_reordered_delivery() {
    let mut in_order = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);
    let mut reordered = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);
    let day_one = batch(interval("dc-a", 1, 1), &[(key(1, "1.0", ""), 2, 20)]);
    let day_three = batch(interval("dc-a", 1, 3), &[(key(3, "1.0", ""), 4, 40)]);

    in_order.apply(&day_one).unwrap();
    in_order.apply(&day_three).unwrap();
    reordered.apply(&day_three).unwrap();
    reordered.apply(&day_one).unwrap();

    assert_eq!(reordered.total(&key(1, "1.0", "")), in_order.total(&key(1, "1.0", "")));
    assert_eq!(reordered.total(&key(3, "1.0", "")), in_order.total(&key(3, "1.0", "")));
    // The cursor is the highest accepted day, regardless of arrival order.
    assert_eq!(reordered.after_day(&ProducerId("dc-a".to_owned())), 3);
}

#[test]
fn test_receiver_compaction_releases_covered_keys_and_keeps_totals() {
    let mut receiver = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);
    receiver
        .apply(&batch(interval("dc-a", 1, 1), &[(key(1, "1.0", ""), 2, 20)]))
        .unwrap();
    receiver
        .apply(&batch(interval("dc-a", 1, 2), &[(key(2, "1.0", ""), 5, 50)]))
        .unwrap();

    receiver.acknowledge(ProducerId("dc-a".to_owned()), AuthorityEpoch(1), 1);
    receiver.compact();

    // Day 1's replay key is released; day 2's stays. Both totals survive.
    assert_eq!(receiver.retained_intervals(), 1);
    assert_eq!(
        receiver.total(&key(1, "1.0", "")),
        AggregateDelta {
            downloads: 2,
            bytes: 20
        }
    );
    assert_eq!(
        receiver.total(&key(2, "1.0", "")),
        AggregateDelta {
            downloads: 5,
            bytes: 50
        }
    );
}

#[test]
fn test_receiver_snapshot_round_trips_state_cursor_and_frontier() {
    let mut receiver = AnalyticsReceiver::new(DEFAULT_APPLY_LIMITS);
    receiver
        .apply(&batch(interval("dc-a", 1, 4), &[(key(4, "1.0", "alpha"), 6, 60)]))
        .unwrap();
    receiver.acknowledge(ProducerId("dc-a".to_owned()), AuthorityEpoch(1), 4);

    let restored = AnalyticsReceiver::restore(&receiver.encode(), DEFAULT_APPLY_LIMITS).unwrap();

    assert_eq!(
        restored.total(&key(4, "1.0", "alpha")),
        AggregateDelta {
            downloads: 6,
            bytes: 60
        }
    );
    assert_eq!(restored.after_day(&ProducerId("dc-a".to_owned())), 4);
    assert_eq!(restored.retained_intervals(), 1);
    // The restored frontier still covers the acknowledged interval, so compaction releases it.
    let mut restored = restored;
    restored.compact();
    assert_eq!(restored.retained_intervals(), 0);
}

#[test]
fn test_receiver_restore_rejects_a_foreign_schema() {
    let snapshot = br#"{"schema":999,"state":[],"cursors":[],"accepted":[],"frontier":[]}"#;
    let error = AnalyticsReceiver::restore(snapshot, DEFAULT_APPLY_LIMITS).unwrap_err();
    assert!(matches!(
        error,
        SnapshotError::UnsupportedSchema {
            expected: 2,
            found: 999
        }
    ));
}

#[test]
fn test_receiver_restore_rejects_malformed_bytes() {
    let error = AnalyticsReceiver::restore(b"not json", DEFAULT_APPLY_LIMITS).unwrap_err();
    assert!(matches!(error, SnapshotError::Malformed(_)));
}

#[test]
fn test_receiver_apply_error_leaves_the_cursor_unmoved() {
    let mut receiver = AnalyticsReceiver::new(ApplyLimits {
        max_rows_per_batch: 16_384,
        max_retained_intervals: 1,
    });
    receiver
        .apply(&batch(interval("dc-a", 1, 1), &[(key(1, "1.0", ""), 1, 10)]))
        .unwrap();

    let error = receiver
        .apply(&batch(interval("dc-a", 1, 2), &[(key(2, "1.0", ""), 1, 10)]))
        .unwrap_err();

    assert!(matches!(error, ApplyError::RetentionFull { limit: 1 }));
    // The rejected interval neither folded nor advanced the cursor past the accepted day 1.
    assert_eq!(receiver.after_day(&ProducerId("dc-a".to_owned())), 1);
    assert_eq!(receiver.total(&key(2, "1.0", "")), AggregateDelta::default());
}
