use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use peryx_events::metrics::{Clock, DailyUsage, Metrics, MetricsError, MetricsStore, Observation};
use peryx_storage::meta::{AnalyticsCheckpoint, AnalyticsDelta, ArtifactUsageKey, DailyUsageKey, UsageTotals};
use rstest::rstest;

#[derive(Clone)]
struct TestStore {
    checkpoint: Arc<Mutex<AnalyticsCheckpoint>>,
    read_failure: Option<ReadFailure>,
    write_failure: Arc<Mutex<Option<WriteFailure>>>,
    commits: Arc<AtomicUsize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadFailure {
    Lifetime,
    Daily,
}

/// The two points a redb checkpoint can fail: before it touches a row, and once the rows are
/// prepared but the transaction has not committed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteFailure {
    BeforeRows,
    AfterRows,
}

impl MetricsStore for TestStore {
    fn load_checkpoint(&self) -> Result<AnalyticsCheckpoint, MetricsError> {
        match self.read_failure {
            Some(ReadFailure::Lifetime) => {
                return Err(MetricsError::Persistence("lifetime read failed".to_owned()));
            }
            Some(ReadFailure::Daily) => return Err(MetricsError::Persistence("daily read failed".to_owned())),
            None => {}
        }
        Ok(self.checkpoint.lock().unwrap().clone())
    }

    fn commit_checkpoint(&self, delta: &AnalyticsDelta) -> Result<(), MetricsError> {
        let failure = self.write_failure.lock().unwrap().take();
        if failure == Some(WriteFailure::BeforeRows) {
            return Err(Self::write_error("before rows"));
        }
        let mut checkpoint = self.checkpoint.lock().unwrap();
        let mut next = checkpoint.clone();
        apply(&mut next, delta);
        if failure == Some(WriteFailure::AfterRows) {
            return Err(Self::write_error("after rows"));
        }
        *checkpoint = next;
        drop(checkpoint);
        self.commits.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn apply(checkpoint: &mut AnalyticsCheckpoint, delta: &AnalyticsDelta) {
    assert_eq!(delta.expire_daily_before, None, "no case here configures retention");
    upsert(&mut checkpoint.lifetime, &delta.lifetime);
    upsert(&mut checkpoint.daily, &delta.daily);
    if delta.clear_migrated {
        checkpoint.migrated_lifetime = None;
        checkpoint.migrated_daily = None;
    }
}

/// Merging through a map replaces the rows a delta names and orders the result by key, which is what
/// a reader gets back from the keyed tables.
fn upsert<K: Clone + Ord>(rows: &mut Vec<(K, UsageTotals)>, changed: &[(K, UsageTotals)]) {
    let mut merged: BTreeMap<K, UsageTotals> = std::mem::take(rows).into_iter().collect();
    merged.extend(changed.iter().cloned());
    rows.extend(merged);
}

impl TestStore {
    fn empty() -> Self {
        Self {
            checkpoint: Arc::default(),
            read_failure: None,
            write_failure: Arc::new(Mutex::new(None)),
            commits: Arc::default(),
        }
    }

    fn migrated(lifetime: Option<Vec<u8>>, daily: Option<Vec<u8>>) -> Self {
        let store = Self::empty();
        *store.checkpoint.lock().unwrap() = AnalyticsCheckpoint {
            migrated_lifetime: lifetime,
            migrated_daily: daily,
            ..AnalyticsCheckpoint::default()
        };
        store
    }

    fn failing(failure: WriteFailure) -> Self {
        let store = Self::empty();
        *store.write_failure.lock().unwrap() = Some(failure);
        store
    }

    fn failing_read(failure: ReadFailure, commits: Arc<AtomicUsize>) -> Self {
        Self {
            read_failure: Some(failure),
            commits,
            ..Self::migrated(
                Some(br#"{"artifacts":[]}"#.to_vec()),
                Some(br#"{"schema":1,"buckets":[]}"#.to_vec()),
            )
        }
    }

    fn tracking(commits: Arc<AtomicUsize>) -> Self {
        Self {
            commits,
            ..Self::empty()
        }
    }

    fn arm(&self, failure: WriteFailure) {
        *self.write_failure.lock().unwrap() = Some(failure);
    }

    fn write_error(stage: &str) -> MetricsError {
        MetricsError::Persistence(format!("{stage} write failed"))
    }
}

fn clock() -> Clock {
    Arc::new(|| 0)
}

fn migrated_lifetime() -> Vec<u8> {
    br#"{"artifacts":[{"repository":"hosted","resource":"demo","artifact":"demo-1.0.tar.gz","reads":4,"bytes":40}]}"#
        .to_vec()
}

fn migrated_daily() -> Vec<u8> {
    br#"{"schema":1,"buckets":[{"day":0,"repository":"hosted","resource":"demo","group":"1.0","source":"","reads":4,"bytes":40}]}"#
        .to_vec()
}

fn adopted_lifetime() -> Vec<(ArtifactUsageKey, UsageTotals)> {
    vec![(
        ArtifactUsageKey {
            repository: "hosted".to_owned(),
            resource: "demo".to_owned(),
            artifact: "demo-1.0.tar.gz".to_owned(),
        },
        UsageTotals { reads: 4, bytes: 40 },
    )]
}

fn adopted_daily() -> Vec<(DailyUsageKey, UsageTotals)> {
    vec![(
        DailyUsageKey {
            day: 0,
            repository: "hosted".to_owned(),
            resource: "demo".to_owned(),
            group: "1.0".to_owned(),
            source: String::new(),
        },
        UsageTotals { reads: 4, bytes: 40 },
    )]
}

#[test]
fn test_corrupt_migrated_lifetime_value_stops_startup() {
    let result = Metrics::start_durable(TestStore::migrated(Some(b"{".to_vec()), None), None, clock());

    assert!(matches!(result, Err(MetricsError::ReadSnapshot(_))));
}

#[test]
fn test_corrupt_migrated_daily_value_stops_startup() {
    let result = Metrics::start_durable(TestStore::migrated(None, Some(b"{".to_vec())), None, clock());

    assert!(matches!(result, Err(MetricsError::DailySnapshot(_))));
}

#[test]
fn test_unsupported_migrated_daily_schema_stops_startup() {
    let result = Metrics::start_durable(
        TestStore::migrated(None, Some(br#"{"schema":2,"buckets":[]}"#.to_vec())),
        None,
        clock(),
    );

    assert!(matches!(result, Err(MetricsError::DailySchema(2))));
}

#[test]
fn test_migrated_values_become_rows_and_are_cleared_in_one_commit() {
    let store = TestStore::migrated(Some(migrated_lifetime()), Some(migrated_daily()));
    let persisted = store.clone();

    Metrics::start_durable(store, None, clock())
        .unwrap()
        .shutdown()
        .unwrap();

    assert_eq!(
        persisted.checkpoint.lock().unwrap().clone(),
        AnalyticsCheckpoint {
            lifetime: adopted_lifetime(),
            daily: adopted_daily(),
            migrated_lifetime: None,
            migrated_daily: None,
        }
    );
}

#[rstest]
#[case::before_rows(WriteFailure::BeforeRows)]
#[case::after_rows(WriteFailure::AfterRows)]
fn test_a_failed_adoption_leaves_the_migrated_values_and_writes_no_rows(#[case] failure: WriteFailure) {
    let store = TestStore::migrated(Some(migrated_lifetime()), Some(migrated_daily()));
    store.arm(failure);
    let persisted = store.clone();

    assert!(matches!(
        Metrics::start_durable(store, None, clock()),
        Err(MetricsError::Persistence(_))
    ));
    assert_eq!(
        persisted.checkpoint.lock().unwrap().clone(),
        AnalyticsCheckpoint {
            lifetime: Vec::new(),
            daily: Vec::new(),
            migrated_lifetime: Some(migrated_lifetime()),
            migrated_daily: Some(migrated_daily()),
        }
    );
}

#[test]
fn test_retrying_a_failed_adoption_does_not_double_count_rows() {
    let store = TestStore::migrated(Some(migrated_lifetime()), Some(migrated_daily()));
    store.arm(WriteFailure::AfterRows);
    let persisted = store.clone();
    assert!(Metrics::start_durable(store, None, clock()).is_err());

    let retried = Metrics::start_durable(persisted.clone(), None, clock()).unwrap();

    let settled = persisted.checkpoint.lock().unwrap().clone();
    assert_eq!((settled.lifetime, settled.daily), (adopted_lifetime(), adopted_daily()));
    assert_eq!(retried.index_totals()["hosted"].base.reads, 4);
    retried.shutdown().unwrap();
}

#[test]
fn test_a_restart_without_migrated_values_commits_nothing_at_startup() {
    let commits = Arc::new(AtomicUsize::new(0));
    let store = TestStore::tracking(Arc::clone(&commits));
    let metrics = Metrics::start_durable(store.clone(), None, clock()).unwrap();
    metrics.record(observation());
    metrics.flush().unwrap();
    metrics.shutdown().unwrap();

    Metrics::start_durable(store, None, clock())
        .unwrap()
        .shutdown()
        .unwrap();

    assert_eq!(commits.load(Ordering::Relaxed), 1);
}

#[rstest]
#[case::before_rows(WriteFailure::BeforeRows, "metrics persistence failed: before rows write failed")]
#[case::after_rows(WriteFailure::AfterRows, "metrics persistence failed: after rows write failed")]
fn test_checkpoint_failure_stays_pending_until_retry(#[case] failure: WriteFailure, #[case] expected: &str) {
    let store = TestStore::failing(failure);
    let persisted = store.clone();
    let metrics = Metrics::start_durable(store, None, clock()).unwrap();
    metrics.record(observation());

    assert!(matches!(metrics.flush(), Err(MetricsError::Persistence(_))));
    assert_eq!(metrics.durability_failure().as_deref(), Some(expected));
    let before_retry = Metrics::start_durable(persisted.clone(), None, clock()).unwrap();
    assert_eq!(
        (before_retry.index_totals().len(), before_retry.daily_usage().len()),
        (0, 0)
    );
    before_retry.shutdown().unwrap();

    metrics.flush().unwrap();
    assert_eq!(metrics.durability_failure(), None);
    metrics.shutdown().unwrap();
    let restarted = Metrics::start_durable(persisted, None, clock()).unwrap();
    assert_eq!(restarted.index_totals()["hosted"].base.reads, 1);
    assert_eq!(
        restarted.daily_usage(),
        [DailyUsage {
            day: 0,
            repository: "hosted".to_owned(),
            resource: "demo".to_owned(),
            group: "1.0".to_owned(),
            source: String::new(),
            reads: 1,
            bytes: 1,
        }]
    );
    restarted.shutdown().unwrap();
}

#[test]
fn test_failed_durable_startup_degrades_with_an_observable_error() {
    let metrics = degraded_metrics();

    assert!(
        metrics
            .durability_failure()
            .is_some_and(|error| error.starts_with("invalid metrics snapshot:"))
    );
    assert!(matches!(metrics.flush(), Err(MetricsError::Stopped)));
}

#[test]
fn test_degraded_metrics_count_dropped_observations() {
    let metrics = degraded_metrics();

    metrics.record(observation());

    assert_eq!(metrics.dropped(), 1);
}

#[rstest]
#[case::lifetime(ReadFailure::Lifetime, "metrics persistence failed: lifetime read failed")]
#[case::daily(ReadFailure::Daily, "metrics persistence failed: daily read failed")]
fn test_snapshot_read_failure_is_observable_without_overwriting(#[case] failure: ReadFailure, #[case] expected: &str) {
    let commits = Arc::new(AtomicUsize::new(0));
    let metrics =
        Metrics::start_durable_or_degraded(TestStore::failing_read(failure, Arc::clone(&commits)), None, clock());

    assert_eq!(metrics.durability_failure().as_deref(), Some(expected));
    assert!(matches!(metrics.flush(), Err(MetricsError::Stopped)));
    assert_eq!(commits.load(Ordering::Relaxed), 0);
}

#[test]
fn test_missing_snapshots_allow_a_checkpoint() {
    let commits = Arc::new(AtomicUsize::new(0));
    let metrics = Metrics::start_durable(TestStore::tracking(Arc::clone(&commits)), None, clock()).unwrap();
    metrics.record(observation());

    metrics.flush().unwrap();

    assert_eq!(commits.load(Ordering::Relaxed), 1);
    metrics.shutdown().unwrap();
}

fn observation() -> Observation {
    Observation::Read {
        repository: "hosted".to_owned(),
        resource: "demo".to_owned(),
        artifact: "demo-1.0.tar.gz".to_owned(),
        group: Some("1.0".to_owned()),
        source: None,
        bytes: 1,
    }
}

fn degraded_metrics() -> Metrics {
    Metrics::start_durable_or_degraded(TestStore::migrated(Some(b"{".to_vec()), None), None, clock())
}
