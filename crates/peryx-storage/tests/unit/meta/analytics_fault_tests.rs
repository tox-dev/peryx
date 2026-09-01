use crate::meta::{
    ANALYTICS, ANALYTICS_DAILY_KEY, ANALYTICS_KEY, AnalyticsCheckpoint, AnalyticsDelta, ArtifactUsageKey,
    DailyUsageKey, UsageTotals, fault,
};

fn row(artifact: &str, reads: u64) -> (ArtifactUsageKey, UsageTotals) {
    (
        ArtifactUsageKey {
            repository: "alpha".to_owned(),
            resource: "demo".to_owned(),
            artifact: artifact.to_owned(),
        },
        totals(reads),
    )
}

fn bucket(day: i64, reads: u64) -> (DailyUsageKey, UsageTotals) {
    (
        DailyUsageKey {
            day,
            repository: "alpha".to_owned(),
            resource: "demo".to_owned(),
            group: "1.0".to_owned(),
            source: "upstream".to_owned(),
        },
        totals(reads),
    )
}

const fn totals(reads: u64) -> UsageTotals {
    UsageTotals {
        reads,
        bytes: reads * 10,
    }
}

/// Both views move together, so a torn commit has to be visible in either as the same generation.
fn generation(reads: u64) -> AnalyticsDelta {
    AnalyticsDelta {
        lifetime: vec![row("demo-1.0.bin", reads), row("demo-2.0.bin", reads)],
        daily: vec![bucket(19_000, reads), bucket(19_001, reads)],
        ..AnalyticsDelta::default()
    }
}

type Views = (Vec<(ArtifactUsageKey, UsageTotals)>, Vec<(DailyUsageKey, UsageTotals)>);

fn rows(store: &crate::meta::MetaStore) -> Views {
    let AnalyticsCheckpoint { lifetime, daily, .. } = store.analytics().load_checkpoint().unwrap();
    (lifetime, daily)
}

#[test]
fn test_analytics_checkpoint_recovers_storage_failures_without_partial_rows() {
    let (first, next) = (generation(1), generation(2));
    let mut failures = 0;
    for fail_after in 0..256 {
        let (store, inner, fault) = fault::initialized();
        store.analytics().commit_checkpoint(&first).unwrap();
        drop(store);
        let store = fault::reopen(&inner, &fault);
        fault.arm(fail_after);
        if store.analytics().commit_checkpoint(&next).is_err() {
            failures += 1;
            fault.disable();
            drop(store);
            let store = fault::reopen(&inner, &fault);
            let observed = rows(&store);
            assert!(
                [
                    (first.lifetime.clone(), first.daily.clone()),
                    (next.lifetime.clone(), next.daily.clone()),
                ]
                .contains(&observed)
            );
            store.analytics().commit_checkpoint(&next).unwrap();
            drop(store);
            assert_eq!(
                rows(&fault::reopen(&inner, &fault)),
                (next.lifetime.clone(), next.daily.clone())
            );
        }
    }
    assert!(failures > 0);
}

#[test]
fn test_a_failed_adoption_leaves_the_migrated_values_for_the_next_attempt() {
    let mut failures = 0;
    for fail_after in 0..256 {
        let (store, inner, fault) = fault::initialized();
        fault::corrupt(&store, ANALYTICS, ANALYTICS_KEY, b"migrated lifetime");
        fault::corrupt(&store, ANALYTICS, ANALYTICS_DAILY_KEY, b"migrated daily");
        drop(store);
        let store = fault::reopen(&inner, &fault);
        let adoption = AnalyticsDelta {
            lifetime: vec![row("demo-1.0.bin", 4)],
            clear_migrated: true,
            ..AnalyticsDelta::default()
        };
        fault.arm(fail_after);
        if store.analytics().commit_checkpoint(&adoption).is_err() {
            failures += 1;
            fault.disable();
            drop(store);
            let store = fault::reopen(&inner, &fault);
            let observed = store.analytics().load_checkpoint().unwrap();
            assert_eq!(
                observed.migrated_lifetime.is_some(),
                observed.lifetime.is_empty(),
                "adoption committed rows without clearing the migrated values"
            );
            store.analytics().commit_checkpoint(&adoption).unwrap();
            drop(store);
            let settled = fault::reopen(&inner, &fault).analytics().load_checkpoint().unwrap();
            assert_eq!(
                (settled.lifetime, settled.migrated_lifetime, settled.migrated_daily),
                (adoption.lifetime.clone(), None, None)
            );
        }
    }
    assert!(failures > 0);
}
