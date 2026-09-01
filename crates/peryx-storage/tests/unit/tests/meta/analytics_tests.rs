use super::store;
use crate::meta::{AnalyticsCheckpoint, AnalyticsDelta, ArtifactUsageKey, DailyUsageKey, UsageTotals};

fn artifact(resource: &str, artifact: &str) -> ArtifactUsageKey {
    ArtifactUsageKey {
        repository: "alpha".to_owned(),
        resource: resource.to_owned(),
        artifact: artifact.to_owned(),
    }
}

fn bucket(day: i64, resource: &str) -> DailyUsageKey {
    DailyUsageKey {
        day,
        repository: "alpha".to_owned(),
        resource: resource.to_owned(),
        group: "1.0".to_owned(),
        source: "upstream".to_owned(),
    }
}

const fn totals(reads: u64, bytes: u64) -> UsageTotals {
    UsageTotals { reads, bytes }
}

#[test]
fn test_analytics_checkpoint_is_empty_before_the_first_commit() {
    let (_dir, meta) = store();
    assert_eq!(
        meta.analytics().load_checkpoint().unwrap(),
        AnalyticsCheckpoint::default()
    );
}

#[test]
fn test_analytics_commit_then_load_round_trips_every_row() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    let delta = AnalyticsDelta {
        lifetime: vec![
            (artifact("demo", "demo-1.0.bin"), totals(2, 20)),
            (artifact("other", "other-1.0.bin"), totals(3, 30)),
        ],
        daily: vec![(bucket(19_000, "demo"), totals(2, 20))],
        ..AnalyticsDelta::default()
    };
    handle.commit_checkpoint(&delta).unwrap();
    assert_eq!(
        handle.load_checkpoint().unwrap(),
        AnalyticsCheckpoint {
            lifetime: delta.lifetime,
            daily: delta.daily,
            migrated_lifetime: None,
            migrated_daily: None,
        }
    );
}

#[test]
fn test_analytics_commit_replaces_only_the_rows_it_names() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    handle
        .commit_checkpoint(&AnalyticsDelta {
            lifetime: vec![
                (artifact("demo", "demo-1.0.bin"), totals(1, 10)),
                (artifact("other", "other-1.0.bin"), totals(5, 50)),
            ],
            daily: vec![
                (bucket(19_000, "demo"), totals(1, 10)),
                (bucket(19_000, "other"), totals(5, 50)),
            ],
            ..AnalyticsDelta::default()
        })
        .unwrap();
    handle
        .commit_checkpoint(&AnalyticsDelta {
            lifetime: vec![(artifact("demo", "demo-1.0.bin"), totals(2, 20))],
            daily: vec![(bucket(19_000, "demo"), totals(2, 20))],
            ..AnalyticsDelta::default()
        })
        .unwrap();
    assert_eq!(
        handle.load_checkpoint().unwrap(),
        AnalyticsCheckpoint {
            lifetime: vec![
                (artifact("demo", "demo-1.0.bin"), totals(2, 20)),
                (artifact("other", "other-1.0.bin"), totals(5, 50)),
            ],
            daily: vec![
                (bucket(19_000, "demo"), totals(2, 20)),
                (bucket(19_000, "other"), totals(5, 50)),
            ],
            migrated_lifetime: None,
            migrated_daily: None,
        }
    );
}

#[test]
fn test_analytics_commit_expires_the_daily_prefix_and_keeps_retained_buckets() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    handle
        .commit_checkpoint(&AnalyticsDelta {
            daily: vec![
                (bucket(18_998, "demo"), totals(1, 10)),
                (bucket(18_999, "demo"), totals(2, 20)),
                (bucket(19_000, "demo"), totals(3, 30)),
            ],
            ..AnalyticsDelta::default()
        })
        .unwrap();
    handle
        .commit_checkpoint(&AnalyticsDelta {
            daily: vec![(bucket(19_001, "demo"), totals(4, 40))],
            expire_daily_before: Some(19_000),
            ..AnalyticsDelta::default()
        })
        .unwrap();
    assert_eq!(
        handle.load_checkpoint().unwrap().daily,
        vec![
            (bucket(19_000, "demo"), totals(3, 30)),
            (bucket(19_001, "demo"), totals(4, 40)),
        ]
    );
}

#[test]
fn test_analytics_delta_is_empty_only_without_rows_pruning_or_migrated_values() {
    assert_eq!(
        (
            AnalyticsDelta::default().is_empty(),
            AnalyticsDelta {
                lifetime: vec![(artifact("demo", "demo-1.0.bin"), totals(1, 10))],
                ..AnalyticsDelta::default()
            }
            .is_empty(),
            AnalyticsDelta {
                daily: vec![(bucket(19_000, "demo"), totals(1, 10))],
                ..AnalyticsDelta::default()
            }
            .is_empty(),
            AnalyticsDelta {
                expire_daily_before: Some(19_000),
                ..AnalyticsDelta::default()
            }
            .is_empty(),
            AnalyticsDelta {
                clear_migrated: true,
                ..AnalyticsDelta::default()
            }
            .is_empty(),
        ),
        (true, false, false, false, false)
    );
}

#[test]
fn test_analytics_handle_is_a_noop_once_the_store_drops() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    handle.save_apply(b"retained apply").unwrap();
    handle.save_producer(b"retained producer").unwrap();
    assert_eq!(
        (handle.load_apply().unwrap(), handle.load_producer().unwrap()),
        (Some(b"retained apply".to_vec()), Some(b"retained producer".to_vec()))
    );
    drop(meta);
    handle
        .commit_checkpoint(&AnalyticsDelta {
            lifetime: vec![(artifact("demo", "demo-1.0.bin"), totals(1, 10))],
            ..AnalyticsDelta::default()
        })
        .unwrap();
    handle.save_apply(b"ignored").unwrap();
    handle.save_producer(b"ignored").unwrap();
    assert_eq!(handle.load_checkpoint().unwrap(), AnalyticsCheckpoint::default());
    assert_eq!(
        (handle.load_apply().unwrap(), handle.load_producer().unwrap()),
        (None, None)
    );
}

#[test]
fn test_analytics_apply_state_is_absent_before_first_save() {
    let (_dir, meta) = store();
    assert_eq!(meta.analytics().load_apply().unwrap(), None);
}

#[test]
fn test_analytics_apply_state_round_trips_under_its_own_key() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    handle.save_apply(b"apply").unwrap();
    assert_eq!(handle.load_apply().unwrap(), Some(b"apply".to_vec()));
    assert_eq!(handle.load_checkpoint().unwrap(), AnalyticsCheckpoint::default());
    assert_eq!(handle.load_producer().unwrap(), None);
}

#[test]
fn test_analytics_producer_record_is_absent_before_first_save() {
    let (_dir, meta) = store();
    assert_eq!(meta.analytics().load_producer().unwrap(), None);
}

#[test]
fn test_analytics_producer_record_round_trips_under_its_own_key() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    handle.save_producer(b"producer").unwrap();
    assert_eq!(handle.load_producer().unwrap(), Some(b"producer".to_vec()));
    assert_eq!(handle.load_apply().unwrap(), None);
}
