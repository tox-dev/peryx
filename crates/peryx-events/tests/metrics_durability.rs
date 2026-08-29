use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use peryx_events::metrics::{Clock, DailyUsage, Metrics, MetricsError, MetricsStore, Observation};
use peryx_storage::meta::AnalyticsCheckpoint;
use rstest::rstest;

#[derive(Clone)]
struct TestStore {
    checkpoint: Arc<Mutex<AnalyticsCheckpoint>>,
    read_failure: Option<ReadFailure>,
    write_failure: Arc<Mutex<Option<WriteFailure>>>,
    writes: Arc<AtomicUsize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadFailure {
    Lifetime,
    Daily,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteFailure {
    Lifetime,
    Daily,
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

    fn save_checkpoint(&self, lifetime: &[u8], daily: &[u8]) -> Result<(), MetricsError> {
        let failure = self.write_failure.lock().unwrap().take();
        if failure == Some(WriteFailure::Lifetime) {
            return Err(Self::write_error("lifetime"));
        }
        let lifetime = lifetime.to_vec();
        if failure == Some(WriteFailure::Daily) {
            return Err(Self::write_error("daily"));
        }
        *self.checkpoint.lock().unwrap() = AnalyticsCheckpoint {
            lifetime: Some(lifetime),
            daily: Some(daily.to_vec()),
        };
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl TestStore {
    fn new(reads: Option<Vec<u8>>, daily: Option<Vec<u8>>) -> Self {
        Self {
            checkpoint: Arc::new(Mutex::new(AnalyticsCheckpoint { lifetime: reads, daily })),
            read_failure: None,
            write_failure: Arc::new(Mutex::new(None)),
            writes: Arc::default(),
        }
    }

    fn failing(failure: WriteFailure) -> Self {
        let store = Self::new(None, None);
        *store.write_failure.lock().unwrap() = Some(failure);
        store
    }

    fn failing_read(failure: ReadFailure, writes: Arc<AtomicUsize>) -> Self {
        Self {
            checkpoint: Arc::new(Mutex::new(AnalyticsCheckpoint {
                lifetime: Some(br#"{"artifacts":[]}"#.to_vec()),
                daily: Some(br#"{"schema":1,"buckets":[]}"#.to_vec()),
            })),
            read_failure: Some(failure),
            write_failure: Arc::new(Mutex::new(None)),
            writes,
        }
    }

    fn tracking(writes: Arc<AtomicUsize>) -> Self {
        Self {
            writes,
            ..Self::new(None, None)
        }
    }

    fn write_error(snapshot: &str) -> MetricsError {
        MetricsError::Persistence(format!("{snapshot} write failed"))
    }
}

fn clock() -> Clock {
    Arc::new(|| 0)
}

#[test]
fn test_corrupt_read_snapshot_stops_startup() {
    let result = Metrics::start_durable(TestStore::new(Some(b"{".to_vec()), None), None, clock());

    assert!(matches!(result, Err(MetricsError::ReadSnapshot(_))));
}

#[test]
fn test_corrupt_daily_snapshot_stops_startup() {
    let result = Metrics::start_durable(TestStore::new(None, Some(b"{".to_vec())), None, clock());

    assert!(matches!(result, Err(MetricsError::DailySnapshot(_))));
}

#[test]
fn test_unsupported_daily_schema_stops_startup() {
    let result = Metrics::start_durable(
        TestStore::new(None, Some(br#"{"schema":2,"buckets":[]}"#.to_vec())),
        None,
        clock(),
    );

    assert!(matches!(result, Err(MetricsError::DailySchema(2))));
}

#[rstest]
#[case::lifetime(WriteFailure::Lifetime, "metrics persistence failed: lifetime write failed")]
#[case::daily(WriteFailure::Daily, "metrics persistence failed: daily write failed")]
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
    let writes = Arc::new(AtomicUsize::new(0));
    let metrics =
        Metrics::start_durable_or_degraded(TestStore::failing_read(failure, Arc::clone(&writes)), None, clock());

    assert_eq!(metrics.durability_failure().as_deref(), Some(expected));
    assert!(matches!(metrics.flush(), Err(MetricsError::Stopped)));
    assert_eq!(writes.load(Ordering::Relaxed), 0);
}

#[test]
fn test_missing_snapshots_allow_a_checkpoint() {
    let writes = Arc::new(AtomicUsize::new(0));
    let metrics = Metrics::start_durable(TestStore::tracking(Arc::clone(&writes)), None, clock()).unwrap();
    metrics.record(observation());

    metrics.flush().unwrap();

    assert_eq!(writes.load(Ordering::Relaxed), 1);
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
    Metrics::start_durable_or_degraded(TestStore::new(Some(b"{".to_vec()), None), None, clock())
}
