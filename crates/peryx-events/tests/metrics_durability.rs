use std::sync::Arc;

use peryx_events::metrics::{Clock, Metrics, MetricsError, MetricsStore, Observation};
use rstest::rstest;

struct TestStore {
    reads: Option<Vec<u8>>,
    daily: Option<Vec<u8>>,
    failing_write: bool,
    failing_daily_write: bool,
}

impl MetricsStore for TestStore {
    fn load(&self) -> Result<Option<Vec<u8>>, MetricsError> {
        Ok(self.reads.clone())
    }

    fn save(&self, _snapshot: &[u8]) -> Result<(), MetricsError> {
        Self::fail_write(self.failing_write)
    }

    fn load_daily(&self) -> Result<Option<Vec<u8>>, MetricsError> {
        Ok(self.daily.clone())
    }

    fn save_daily(&self, _snapshot: &[u8]) -> Result<(), MetricsError> {
        Self::fail_write(self.failing_daily_write)
    }
}

impl TestStore {
    const fn failing() -> Self {
        Self {
            reads: None,
            daily: None,
            failing_write: true,
            failing_daily_write: false,
        }
    }

    const fn failing_daily() -> Self {
        Self {
            reads: None,
            daily: None,
            failing_write: false,
            failing_daily_write: true,
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
    let result = Metrics::start_durable(
        TestStore {
            reads: Some(b"{".to_vec()),
            daily: None,
            failing_write: false,
            failing_daily_write: false,
        },
        None,
        clock(),
    );

    assert!(matches!(result, Err(MetricsError::ReadSnapshot(_))));
}

#[test]
fn test_corrupt_daily_snapshot_stops_startup() {
    let result = Metrics::start_durable(
        TestStore {
            reads: None,
            daily: Some(b"{".to_vec()),
            failing_write: false,
            failing_daily_write: false,
        },
        None,
        clock(),
    );

    assert!(matches!(result, Err(MetricsError::DailySnapshot(_))));
}

#[test]
fn test_unsupported_daily_schema_stops_startup() {
    let result = Metrics::start_durable(
        TestStore {
            reads: None,
            daily: Some(br#"{"schema":2,"buckets":[]}"#.to_vec()),
            failing_write: false,
            failing_daily_write: false,
        },
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
    metrics.record(Observation::Read {
        repository: "hosted".to_owned(),
        resource: "demo".to_owned(),
        artifact: "demo-1.0.tar.gz".to_owned(),
        group: Some("1.0".to_owned()),
        source: None,
        bytes: 1,
    });

    assert!(matches!(metrics.flush(), Err(MetricsError::Persistence(_))));
    assert_eq!(
        metrics.durability_failure().as_deref(),
        Some("metrics persistence failed: read-only store")
    );
    assert!(matches!(metrics.shutdown(), Err(MetricsError::Persistence(_))));
}

#[test]
fn test_failed_durable_startup_degrades_with_an_observable_error() {
    let metrics = Metrics::start_durable_or_degraded(
        TestStore {
            reads: Some(b"{".to_vec()),
            daily: None,
            failing_write: false,
            failing_daily_write: false,
        },
        None,
        clock(),
    );

    assert!(
        metrics
            .durability_failure()
            .is_some_and(|error| error.starts_with("invalid metrics snapshot:"))
    );
    assert!(matches!(metrics.flush(), Err(MetricsError::Stopped)));
}
