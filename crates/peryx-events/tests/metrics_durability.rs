use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use peryx_events::metrics::{Clock, Metrics, MetricsError, MetricsStore, Observation};
use rstest::rstest;

struct TestStore {
    reads: Option<Vec<u8>>,
    daily: Option<Vec<u8>>,
    read_failure: Option<ReadFailure>,
    failing_write: bool,
    failing_daily_write: bool,
    writes: Arc<AtomicUsize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadFailure {
    Lifetime,
    Daily,
}

impl MetricsStore for TestStore {
    fn load(&self) -> Result<Option<Vec<u8>>, MetricsError> {
        if self.read_failure == Some(ReadFailure::Lifetime) {
            return Err(MetricsError::Persistence("lifetime read failed".to_owned()));
        }
        Ok(self.reads.clone())
    }

    fn save(&self, _snapshot: &[u8]) -> Result<(), MetricsError> {
        Self::fail_write(self.failing_write)?;
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn load_daily(&self) -> Result<Option<Vec<u8>>, MetricsError> {
        if self.read_failure == Some(ReadFailure::Daily) {
            return Err(MetricsError::Persistence("daily read failed".to_owned()));
        }
        Ok(self.daily.clone())
    }

    fn save_daily(&self, _snapshot: &[u8]) -> Result<(), MetricsError> {
        Self::fail_write(self.failing_daily_write)?;
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl TestStore {
    fn new(reads: Option<Vec<u8>>, daily: Option<Vec<u8>>) -> Self {
        Self {
            reads,
            daily,
            read_failure: None,
            failing_write: false,
            failing_daily_write: false,
            writes: Arc::default(),
        }
    }

    fn failing() -> Self {
        Self {
            failing_write: true,
            ..Self::new(None, None)
        }
    }

    fn failing_daily() -> Self {
        Self {
            failing_daily_write: true,
            ..Self::new(None, None)
        }
    }

    fn failing_read(failure: ReadFailure, writes: Arc<AtomicUsize>) -> Self {
        Self {
            reads: Some(br#"{"artifacts":[]}"#.to_vec()),
            daily: Some(br#"{"schema":1,"buckets":[]}"#.to_vec()),
            read_failure: Some(failure),
            failing_write: false,
            failing_daily_write: false,
            writes,
        }
    }

    fn tracking(writes: Arc<AtomicUsize>) -> Self {
        Self {
            writes,
            ..Self::new(None, None)
        }
    }

    fn fail_write(failing: bool) -> Result<(), MetricsError> {
        if !failing {
            return Ok(());
        }
        Err(MetricsError::Persistence("read-only store".to_owned()))
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
#[case::reads(TestStore::failing())]
#[case::daily(TestStore::failing_daily())]
fn test_checkpoint_failure_is_returned_and_observable(#[case] store: TestStore) {
    let metrics = Metrics::start_durable(store, None, clock()).unwrap();
    metrics.record(observation());

    assert!(matches!(metrics.flush(), Err(MetricsError::Persistence(_))));
    assert_eq!(
        metrics.durability_failure().as_deref(),
        Some("metrics persistence failed: read-only store")
    );
    assert!(matches!(metrics.shutdown(), Err(MetricsError::Persistence(_))));
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

    assert_eq!(writes.load(Ordering::Relaxed), 2);
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
