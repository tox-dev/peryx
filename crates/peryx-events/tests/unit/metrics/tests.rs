use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use peryx_ha::{
    AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, AnalyticsBatchSource as _, AuthorityEpoch, IntervalId,
    ProducerId,
};
use peryx_storage::meta::{AnalyticsHandle, MetaStore};
use rstest::rstest;

use super::{
    Aggregator, Clock, DailyUsage, FlushState, GroupUsage, MAX_BATCH_MESSAGES, Message, MetricFamily, MetricKind,
    Metrics, MetricsError, MetricsStore, Observation, ResourceUsage, SourceUsage, TimelineBucket, UnusedResource,
    UsageInterval, step,
};

const SECONDS_PER_DAY: i64 = 86_400;

const EXTENSION_RUNS: MetricFamily = MetricFamily {
    key: "extension_runs",
    prom_name: "peryx_extension_runs_total",
    help: "Extension runs.",
    ui_label: "Extension runs",
    roles: &[peryx_core::Role::Cached],
    json_name: Some("extension_runs"),
    kind: MetricKind::Counter,
};
const EXTENSION_SIZE: MetricFamily = MetricFamily {
    key: "extension_size",
    prom_name: "peryx_extension_size",
    help: "Extension size.",
    ui_label: "Extension size",
    roles: &[peryx_core::Role::Cached],
    json_name: Some("extension_size"),
    kind: MetricKind::Gauge,
};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn clock_on_day(day: i64) -> Clock {
    Arc::new(move || day * SECONDS_PER_DAY + SECONDS_PER_DAY / 2)
}

fn steppable_clock() -> (std::sync::Arc<std::sync::atomic::AtomicI64>, Clock) {
    let day = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let handle = std::sync::Arc::clone(&day);
    let clock: Clock =
        Arc::new(move || handle.load(std::sync::atomic::Ordering::SeqCst) * SECONDS_PER_DAY + SECONDS_PER_DAY / 2);
    (day, clock)
}

fn checkpoint_clock() -> (Clock, Receiver<()>, SyncSender<()>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let (checkpointing, checkpointed) = sync_channel(0);
    let (resume, resumed) = sync_channel(0);
    let resumed = Arc::new(Mutex::new(resumed));
    (
        Arc::new(move || {
            if calls.fetch_add(1, Ordering::SeqCst) == 2 {
                checkpointing.send(()).unwrap();
                resumed.lock().unwrap().recv().unwrap();
            }
            2 * SECONDS_PER_DAY
        }),
        checkpointed,
        resume,
    )
}

fn pausing_aggregator_clock(unix_secs: i64) -> (Arc<AtomicI64>, Clock, Receiver<()>, SyncSender<()>) {
    let now = Arc::new(AtomicI64::new(unix_secs));
    let clock_now = Arc::clone(&now);
    let paused = AtomicBool::new(false);
    let (pausing, paused_at) = sync_channel(0);
    let (resume, resumed) = sync_channel(0);
    let resumed = Mutex::new(resumed);
    (
        now,
        Arc::new(move || {
            if std::thread::current().name() == Some("peryx-metrics") && !paused.swap(true, Ordering::SeqCst) {
                pausing.send(()).unwrap();
                resumed.lock().unwrap().recv().unwrap();
            }
            clock_now.load(Ordering::SeqCst)
        }),
        paused_at,
        resume,
    )
}

fn flush_and_assert(metrics: &Metrics, done: impl Fn() -> bool) {
    metrics.flush().unwrap();
    assert!(done(), "metrics flushed an unexpected state");
}

fn persisted_reads(store: &AnalyticsHandle) -> Option<u64> {
    let bytes = store.load().unwrap()?;
    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    Some(
        snapshot["artifacts"]
            .as_array()?
            .iter()
            .map(|artifact| artifact["reads"].as_u64().unwrap())
            .sum(),
    )
}

fn persisted_snapshot(snapshot: Option<Vec<u8>>) -> serde_json::Value {
    serde_json::from_slice(&snapshot.expect("metrics flush did not persist a snapshot")).unwrap()
}

struct RejectingDailyStore {
    checkpointed: std::sync::mpsc::Sender<()>,
}

impl MetricsStore for RejectingDailyStore {
    fn load(&self) -> Result<Option<Vec<u8>>, MetricsError> {
        Ok(None)
    }

    fn save(&self, _snapshot: &[u8]) -> Result<(), MetricsError> {
        Ok(())
    }

    fn load_daily(&self) -> Result<Option<Vec<u8>>, MetricsError> {
        Ok(None)
    }

    fn save_daily(&self, _snapshot: &[u8]) -> Result<(), MetricsError> {
        self.checkpointed.send(()).ok();
        Err(MetricsError::Persistence("read-only store".to_owned()))
    }
}

struct CheckpointStore {
    store: AnalyticsHandle,
    checkpointed: SyncSender<()>,
}

struct PausingCheckpointStore {
    store: AnalyticsHandle,
    checkpointed: SyncSender<()>,
    resumed: Mutex<Receiver<()>>,
}

impl MetricsStore for PausingCheckpointStore {
    fn load(&self) -> Result<Option<Vec<u8>>, MetricsError> {
        MetricsStore::load(&self.store)
    }

    fn save(&self, snapshot: &[u8]) -> Result<(), MetricsError> {
        MetricsStore::save(&self.store, snapshot)
    }

    fn load_daily(&self) -> Result<Option<Vec<u8>>, MetricsError> {
        MetricsStore::load_daily(&self.store)
    }

    fn save_daily(&self, snapshot: &[u8]) -> Result<(), MetricsError> {
        MetricsStore::save_daily(&self.store, snapshot)?;
        self.checkpointed.send(()).unwrap();
        self.resumed.lock().unwrap().recv().unwrap();
        Ok(())
    }
}

impl MetricsStore for CheckpointStore {
    fn load(&self) -> Result<Option<Vec<u8>>, MetricsError> {
        MetricsStore::load(&self.store)
    }

    fn save(&self, snapshot: &[u8]) -> Result<(), MetricsError> {
        MetricsStore::save(&self.store, snapshot)
    }

    fn load_daily(&self) -> Result<Option<Vec<u8>>, MetricsError> {
        MetricsStore::load_daily(&self.store)
    }

    fn save_daily(&self, snapshot: &[u8]) -> Result<(), MetricsError> {
        MetricsStore::save_daily(&self.store, snapshot)?;
        self.checkpointed.send(()).unwrap();
        Ok(())
    }
}

fn read(repository: &str, resource: &str, artifact: &str, bytes: u64) -> Observation {
    Observation::Read {
        repository: repository.into(),
        resource: resource.into(),
        artifact: artifact.into(),
        group: None,
        source: None,
        bytes,
    }
}

fn grouped_read(repository: &str, resource: &str, group: &str, source: Option<&str>, bytes: u64) -> Observation {
    Observation::Read {
        repository: repository.into(),
        resource: resource.into(),
        artifact: format!("{resource}-{group}.bin"),
        group: Some(group.into()),
        source: source.map(Into::into),
        bytes,
    }
}

#[test]
fn test_record_bounds_the_queue_and_counts_drops_under_overload() {
    let log = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::WARN)
        .with_writer(log.reopen().unwrap())
        .finish();
    let (_now, clock, aggregating, resume) = pausing_aggregator_clock(0);
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), Some(7), clock).unwrap();
    metrics.record(read("alpha", "resource-a", "resource-a-1.bin", 1));
    aggregating.recv().unwrap();

    let mut sends = 1;
    tracing::subscriber::with_default(subscriber, || {
        while metrics.dropped() < 1_024 {
            metrics.record(read("alpha", "resource-a", "resource-a-1.bin", 1));
            sends += 1;
        }
    });
    assert_eq!(metrics.dropped(), 1_024);
    let output = std::fs::read_to_string(log.path()).unwrap();
    assert_eq!(output.matches("dropping metrics event").count(), 2);
    assert!(output.contains("reason=\"full\""), "{output}");
    assert!(output.contains("dropped=1"), "{output}");
    assert!(output.contains("dropped=1024"), "{output}");
    let processed = sends - metrics.dropped();
    resume.send(()).unwrap();
    metrics.flush().unwrap();
    let aggregated: u64 = metrics
        .index_totals()
        .values()
        .map(|counters| counters.base.reads)
        .sum();
    assert_eq!(aggregated, processed, "accepted observations were lost");
}

#[test]
fn test_neutral_daily_snapshot_seeds_a_producer_offline() {
    let (_dir, meta) = store();
    let seeded = DailyUsage {
        day: 19_000,
        repository: "hosted".to_owned(),
        resource: "veloxdemo".to_owned(),
        group: "1.0.0".to_owned(),
        source: String::new(),
        reads: 7,
        bytes: 4096,
    };
    meta.analytics()
        .save_daily(
            &serde_json::to_vec(&serde_json::json!({
                "schema": 1,
                "buckets": [seeded],
            }))
            .unwrap(),
        )
        .unwrap();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(seeded.day + 1)).unwrap();
    assert_eq!(metrics.daily_usage(), [seeded]);
}

#[test]
fn test_snapshots_emit_neutral_wire_keys() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(4)).unwrap();
    metrics.record(grouped_read("alpha", "demo", "1", Some("upstream"), 3));
    metrics.flush().unwrap();
    assert_eq!(
        persisted_snapshot(meta.analytics().load().unwrap()),
        serde_json::json!({
            "artifacts": [{
                "repository": "alpha",
                "resource": "demo",
                "artifact": "demo-1.bin",
                "reads": 1,
                "bytes": 3,
            }],
        })
    );
    assert_eq!(
        persisted_snapshot(meta.analytics().load_daily().unwrap()),
        serde_json::json!({
            "schema": 1,
            "buckets": [{
                "day": 4,
                "repository": "alpha",
                "resource": "demo",
                "group": "1",
                "source": "upstream",
                "reads": 1,
                "bytes": 3,
            }],
        })
    );
}

#[test]
fn test_durable_reads_survive_a_restart() {
    let (_dir, meta) = store();
    let artifact = "resource-d-3.0.bin";
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(0)).unwrap();
    metrics.record(Observation::Page {
        repository: "root/alpha".into(),
        resource: "resource-d".into(),
    });
    metrics.record(read("root/alpha", "resource-d", artifact, 100));
    metrics.record(read("root/alpha", "resource-d", artifact, 50));
    flush_and_assert(&metrics, || persisted_reads(&meta.analytics()) == Some(2));
    assert_eq!(metrics.index_totals()["root/alpha"].base.bytes, 150);
    drop(metrics);

    let restarted = Metrics::start_durable(meta.analytics(), None, clock_on_day(0)).unwrap();
    let totals = restarted.index_totals();
    let index = &totals["root/alpha"];
    assert_eq!(index.base.reads, 2);
    assert_eq!(index.base.bytes, 150);
    let artifacts = restarted.drill(Some("root/alpha"), Some("resource-d"));
    assert_eq!(artifacts["totals"]["base"]["bytes"], 150);
    assert_eq!(artifacts["artifacts"][artifact]["reads"], 2);
    assert_eq!(artifacts["artifacts"][artifact]["bytes"], 150);
}

#[test]
fn test_usage_totals_reports_lifetime_by_repository() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(0)).unwrap();
    metrics.record(read("alpha", "resource-a", "resource-a-1.bin", 100));
    metrics.record(read("alpha", "resource-a", "resource-a-1.bin", 100));
    metrics.record(read("alpha", "resource-c", "resource-c-1.bin", 50));
    metrics.record(read("alpha", "resource-b", "resource-b-1.bin", 40));
    metrics.record(read("other", "django", "django-1.bin", 30));
    flush_and_assert(&metrics, || metrics.usage_totals(None).len() == 4);

    assert_eq!(
        metrics.usage_totals(None),
        [
            ResourceUsage {
                repository: "alpha".into(),
                resource: "resource-a".into(),
                reads: 2,
                bytes: 200,
            },
            ResourceUsage {
                repository: "alpha".into(),
                resource: "resource-b".into(),
                reads: 1,
                bytes: 40,
            },
            ResourceUsage {
                repository: "alpha".into(),
                resource: "resource-c".into(),
                reads: 1,
                bytes: 50,
            },
            ResourceUsage {
                repository: "other".into(),
                resource: "django".into(),
                reads: 1,
                bytes: 30,
            },
        ]
    );
    assert_eq!(
        metrics.usage_totals(Some("other")),
        [ResourceUsage {
            repository: "other".into(),
            resource: "django".into(),
            reads: 1,
            bytes: 30,
        }]
    );
    assert!(metrics.usage_totals(Some("missing")).is_empty());
}

#[test]
fn test_drill_returns_repository_and_global_summaries() {
    let metrics = Metrics::start();
    metrics.record(read("alpha", "demo", "demo-1.bin", 10));
    flush_and_assert(&metrics, || metrics.index_totals().contains_key("alpha"));

    let repository = metrics.drill(Some("alpha"), None);
    assert_eq!(repository["totals"]["base"]["reads"], 1);
    assert_eq!(repository["resources"]["demo"]["base"]["reads"], 1);
    assert_eq!(metrics.drill(None, None)["alpha"]["base"]["reads"], 1);
    assert_eq!(metrics.drill(Some("missing"), None), serde_json::json!({}));
    assert_eq!(metrics.drill(Some("alpha"), Some("missing")), serde_json::json!({}));
}

#[test]
fn test_operational_events_update_each_counter_family() {
    assert_eq!(MetricKind::Counter.as_str(), "counter");
    assert_eq!(MetricKind::Gauge.as_str(), "gauge");
    let metrics = Metrics::start();
    for event in [
        Observation::Ecosystem {
            repository: "alpha".to_owned(),
            resource: "demo".to_owned(),
            artifact: Some("demo.bin".to_owned()),
            family: "metadata",
        },
        Observation::Ecosystem {
            repository: "alpha".to_owned(),
            resource: "demo".to_owned(),
            artifact: None,
            family: "metadata",
        },
        Observation::Write {
            repository: "alpha".to_owned(),
            resource: "demo".to_owned(),
        },
        Observation::Refresh {
            repository: "alpha".to_owned(),
            resource: "demo".to_owned(),
            changed: true,
        },
        Observation::Refresh {
            repository: "alpha".to_owned(),
            resource: "demo".to_owned(),
            changed: false,
        },
        Observation::StaleServed {
            repository: "alpha".to_owned(),
            resource: "demo".to_owned(),
        },
        Observation::UpstreamError {
            repository: "alpha".to_owned(),
            resource: "demo".to_owned(),
        },
        Observation::BlobRejected {
            repository: "alpha".to_owned(),
            resource: "demo".to_owned(),
        },
    ] {
        metrics.record(event);
    }
    metrics.increment("alpha", &EXTENSION_RUNS, 1);
    metrics.increment("alpha", &EXTENSION_RUNS, 2);
    metrics.set("alpha", &EXTENSION_SIZE, 12);
    flush_and_assert(&metrics, || metrics.index_totals().contains_key("alpha"));

    assert_eq!(
        metrics.drill(Some("alpha"), None)["totals"],
        serde_json::json!({
            "base": {
                "pages": 0,
                "reads": 0,
                "bytes": 0,
                "rejected": 1,
            },
            "cached": {
                "refreshes": 2,
                "changed": 1,
                "stale_served": 1,
                "upstream_errors": 1,
            },
            "hosted": {"writes": 1},
            "ecosystem": {"metadata": 2},
        })
    );
    assert_eq!(
        metrics.index_totals()["alpha"].extensions,
        [("extension_runs", 3), ("extension_size", 12)].into()
    );
    assert_eq!(
        metrics.drill(Some("alpha"), Some("demo"))["totals"],
        serde_json::json!({
            "base": {
                "pages": 0,
                "reads": 0,
                "bytes": 0,
                "rejected": 1,
            },
            "cached": {
                "refreshes": 2,
                "changed": 1,
                "stale_served": 1,
                "upstream_errors": 1,
            },
            "hosted": {"writes": 1},
            "ecosystem": {"metadata": 2},
        })
    );
    assert_eq!(
        metrics.drill(Some("alpha"), Some("demo"))["artifacts"]["demo.bin"]["ecosystem"]["metadata"],
        1
    );
}

#[test]
fn test_batches_without_a_read_persist_nothing() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(0)).unwrap();
    metrics.record(Observation::Page {
        repository: "alpha".into(),
        resource: "resource-b".into(),
    });
    flush_and_assert(&metrics, || {
        metrics
            .index_totals()
            .get("alpha")
            .is_some_and(|totals| totals.base.pages == 1)
    });
    assert_eq!(
        metrics.drill(Some("alpha"), Some("resource-b"))["totals"]["base"]["pages"],
        1
    );
    assert_eq!(persisted_reads(&meta.analytics()), None);
    assert!(meta.analytics().load_daily().unwrap().is_none());
}

#[test]
fn test_daily_buckets_split_by_group_source_and_day() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(20_000)).unwrap();
    metrics.record(grouped_read("alpha", "resource-b", "3.0", Some("upstream"), 10));
    metrics.record(grouped_read("alpha", "resource-b", "3.0", Some("upstream"), 40));
    metrics.record(grouped_read("alpha", "resource-b", "2.0", Some("upstream"), 5));
    metrics.record(grouped_read("alpha", "resource-b", "3.0", None, 7));
    flush_and_assert(&metrics, || metrics.daily_usage().len() == 3);

    assert_eq!(
        metrics.daily_usage(),
        [
            DailyUsage {
                day: 20_000,
                repository: "alpha".into(),
                resource: "resource-b".into(),
                group: "2.0".into(),
                source: "upstream".into(),
                reads: 1,
                bytes: 5,
            },
            DailyUsage {
                day: 20_000,
                repository: "alpha".into(),
                resource: "resource-b".into(),
                group: "3.0".into(),
                source: String::new(),
                reads: 1,
                bytes: 7,
            },
            DailyUsage {
                day: 20_000,
                repository: "alpha".into(),
                resource: "resource-b".into(),
                group: "3.0".into(),
                source: "upstream".into(),
                reads: 2,
                bytes: 50,
            },
        ]
    );
}

#[test]
fn test_daily_buckets_use_record_time_across_worker_delay() {
    let (now, clock, aggregating, resume) = pausing_aggregator_clock(SECONDS_PER_DAY - 1);
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), Some(7), clock).unwrap();
    metrics.record(grouped_read("alpha", "resource-b", "1.0", None, 3));
    aggregating.recv().unwrap();
    now.store(SECONDS_PER_DAY, Ordering::SeqCst);
    resume.send(()).unwrap();
    metrics.flush().unwrap();
    metrics.record(grouped_read("alpha", "resource-b", "1.0", None, 5));
    metrics.flush().unwrap();

    assert_eq!(
        metrics.daily_usage(),
        [
            DailyUsage {
                day: 0,
                repository: "alpha".into(),
                resource: "resource-b".into(),
                group: "1.0".into(),
                source: String::new(),
                reads: 1,
                bytes: 3,
            },
            DailyUsage {
                day: 1,
                repository: "alpha".into(),
                resource: "resource-b".into(),
                group: "1.0".into(),
                source: String::new(),
                reads: 1,
                bytes: 5,
            },
        ]
    );
}

#[test]
fn test_retention_drops_expired_days_and_keeps_retained_totals() {
    let (_dir, meta) = store();
    let old = Metrics::start_durable(meta.analytics(), Some(7), clock_on_day(100)).unwrap();
    old.record(grouped_read("alpha", "resource-b", "1.0", Some("up"), 3));
    flush_and_assert(&old, || old.daily_usage().len() == 1);
    drop(old);

    let metrics = Metrics::start_durable(meta.analytics(), Some(7), clock_on_day(110)).unwrap();
    metrics.record(grouped_read("alpha", "resource-b", "2.0", Some("up"), 9));
    flush_and_assert(&metrics, || metrics.daily_usage().iter().any(|row| row.day == 110));

    assert_eq!(
        metrics.daily_usage(),
        [DailyUsage {
            day: 110,
            repository: "alpha".into(),
            resource: "resource-b".into(),
            group: "2.0".into(),
            source: "up".into(),
            reads: 1,
            bytes: 9,
        }]
    );
}

#[test]
fn test_the_running_aggregator_expires_a_bucket_that_ages_past_retention() {
    use std::sync::atomic::Ordering;

    let (_dir, meta) = store();
    let (day, clock) = steppable_clock();
    let metrics = Metrics::start_durable(meta.analytics(), Some(2), clock).unwrap();

    metrics.record(grouped_read("alpha", "resource-b", "1.0", Some("up"), 3));
    flush_and_assert(&metrics, || metrics.daily_usage().iter().any(|row| row.day == 0));

    day.store(5, Ordering::SeqCst);
    metrics.record(grouped_read("alpha", "resource-b", "2.0", Some("up"), 9));
    flush_and_assert(&metrics, || metrics.daily_usage().iter().any(|row| row.day == 5));

    assert_eq!(
        metrics.daily_usage().iter().map(|row| row.day).collect::<Vec<_>>(),
        [5],
        "the aged day-0 bucket expired during aggregation, leaving only day 5"
    );
}

#[test]
fn test_idle_retention_expires_memory_exports_and_snapshot() {
    let (_dir, meta) = store();
    let (day, clock) = steppable_clock();
    let seeded = Metrics::start_durable(meta.analytics(), Some(2), clock.clone()).unwrap();
    seeded.record(grouped_read("alpha", "resource-b", "1.0", Some("up"), 3));
    seeded.flush().unwrap();
    drop(seeded);

    let (checkpointed, checkpoint) = sync_channel(0);
    let metrics = Metrics::spawn(
        Some(Arc::new(CheckpointStore {
            store: meta.analytics(),
            checkpointed,
        })),
        Some(2),
        clock,
        super::EVENT_QUEUE_CAPACITY,
        Duration::ZERO,
    )
    .unwrap();
    day.store(5, Ordering::SeqCst);
    checkpoint.recv().unwrap();

    assert!(metrics.daily_usage().is_empty());
    assert!(
        metrics
            .export_sealed_day_batches(&ProducerId("east".to_owned()), AuthorityEpoch(1), -1)
            .is_empty()
    );
    assert_eq!(
        persisted_snapshot(meta.analytics().load_daily().unwrap()),
        serde_json::json!({"schema": 1, "buckets": []})
    );
}

#[test]
fn test_flush_drains_ephemeral_observations() {
    let metrics = Metrics::start();
    metrics.record(read("alpha", "resource-a", "resource-a-1.bin", 8));
    metrics.flush().unwrap();

    assert_eq!(metrics.index_totals()["alpha"].base.reads, 1);
}

#[test]
fn test_batch_limit_releases_readers_and_checkpoints_before_continuing() {
    let (_dir, meta) = store();
    let (checkpointed, checkpoint) = sync_channel(0);
    let (resume, resumed) = sync_channel(0);
    let metrics = Metrics::spawn(
        Some(Arc::new(PausingCheckpointStore {
            store: meta.analytics(),
            checkpointed,
            resumed: Mutex::new(resumed),
        })),
        None,
        clock_on_day(2),
        super::EVENT_QUEUE_CAPACITY,
        Duration::ZERO,
    )
    .unwrap();
    metrics.record(read("alpha", "resource-a", "first", 1));
    checkpoint.recv().unwrap();
    assert_eq!(metrics.index_totals()["alpha"].base.reads, 1);
    assert_eq!(persisted_reads(&meta.analytics()), Some(1));

    for artifact in 0..=MAX_BATCH_MESSAGES {
        metrics.record(read("alpha", "resource-a", &artifact.to_string(), 1));
    }
    assert_eq!(metrics.dropped(), 0);
    resume.send(()).unwrap();
    checkpoint.recv().unwrap();

    assert_eq!(
        metrics.index_totals()["alpha"].base.reads,
        (MAX_BATCH_MESSAGES + 1) as u64
    );
    assert_eq!(
        persisted_reads(&meta.analytics()),
        Some((MAX_BATCH_MESSAGES + 1) as u64)
    );
    resume.send(()).unwrap();
    checkpoint.recv().unwrap();

    assert_eq!(
        metrics.index_totals()["alpha"].base.reads,
        (MAX_BATCH_MESSAGES + 2) as u64
    );
    assert_eq!(
        persisted_reads(&meta.analytics()),
        Some((MAX_BATCH_MESSAGES + 2) as u64)
    );
    resume.send(()).unwrap();
    metrics.shutdown().unwrap();
    assert_eq!(
        persisted_reads(&meta.analytics()),
        Some((MAX_BATCH_MESSAGES + 2) as u64)
    );
}

#[test]
fn test_flush_excludes_events_queued_after_its_control_message() {
    let (_dir, meta) = store();
    let (_now, clock, aggregating, resume) = pausing_aggregator_clock(2 * SECONDS_PER_DAY);
    let metrics = Metrics::start_durable(meta.analytics(), Some(7), clock).unwrap();
    metrics.record(read("alpha", "resource-a", "before", 1));
    aggregating.recv().unwrap();
    let (completion, done) = channel();
    metrics.sender.send(Message::Flush(completion)).unwrap();
    metrics.record(read("alpha", "resource-a", "after", 1));

    resume.send(()).unwrap();
    done.recv().unwrap().unwrap();

    assert_eq!(persisted_reads(&meta.analytics()), Some(1));
    metrics.flush().unwrap();
    assert_eq!(persisted_reads(&meta.analytics()), Some(2));
}

#[test]
fn test_drain_applies_without_persisting_observations() {
    let (_dir, meta) = store();
    let (clock, checkpointing, resume) = checkpoint_clock();
    let metrics = Metrics::spawn(
        Some(Arc::new(meta.analytics()) as Arc<dyn MetricsStore>),
        Some(7),
        clock,
        super::EVENT_QUEUE_CAPACITY,
        Duration::from_secs(10),
    )
    .unwrap();
    metrics.record(read("alpha", "resource-a", "resource-a-1.bin", 8));
    checkpointing.recv().unwrap();
    resume.send(()).unwrap();
    metrics.drain().unwrap();

    assert_eq!(metrics.index_totals()["alpha"].base.reads, 1);
    assert_eq!(persisted_reads(&meta.analytics()), None);
}

#[test]
fn test_zero_interval_persists_a_completed_batch() {
    let (_dir, meta) = store();
    let analytics = meta.analytics();
    let tree = RwLock::new(HashMap::default());
    let daily = RwLock::new(BTreeMap::default());
    let clock = clock_on_day(2);
    let context = Aggregator {
        tree: &tree,
        daily: &daily,
        store: Some(&analytics),
        retention_days: None,
        clock: &clock,
    };
    let (sender, receiver) = sync_channel(1);
    sender
        .send(Message::Observation {
            event: grouped_read("alpha", "resource-a", "1", None, 8),
            recorded_at: 2 * SECONDS_PER_DAY,
        })
        .unwrap();
    let mut state = FlushState::durable(true, Arc::new(RwLock::new(None)));

    assert!(step(&receiver, &context, Duration::ZERO, &mut state));

    assert_eq!(persisted_reads(&meta.analytics()), Some(1));
}

#[test]
fn test_idle_interval_reports_checkpoint_failure() {
    let (checkpointed, checkpoint) = std::sync::mpsc::channel();
    let metrics = Metrics::spawn(
        Some(Arc::new(RejectingDailyStore { checkpointed })),
        None,
        clock_on_day(2),
        1,
        Duration::ZERO,
    )
    .unwrap();
    metrics.record(grouped_read("alpha", "resource-a", "1", None, 8));
    checkpoint.recv().unwrap();
    checkpoint.recv().unwrap();
    drop(checkpoint);

    assert!(matches!(metrics.flush(), Err(MetricsError::Persistence(_))));
    assert_eq!(
        metrics.durability_failure().as_deref(),
        Some("metrics persistence failed: read-only store")
    );
}

#[test]
fn test_flush_persists_pending_observations() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(2)).unwrap();
    metrics.record(grouped_read("alpha", "resource-a", "1", None, 8));
    metrics.flush().unwrap();

    assert_eq!(persisted_reads(&meta.analytics()), Some(1));
}

#[test]
fn test_shutdown_drains_and_persists_pending_observations() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(2)).unwrap();
    metrics.record(grouped_read("alpha", "resource-a", "1", None, 8));
    metrics.shutdown().unwrap();

    assert_eq!(persisted_reads(&meta.analytics()), Some(1));
}

#[test]
fn test_lifecycle_commands_report_a_stopped_aggregator() {
    let metrics = Metrics::start();
    let stopped = metrics.clone();
    let stopped_drain = metrics.clone();
    let stopped_shutdown = metrics.clone();
    metrics.shutdown().unwrap();

    assert!(matches!(stopped.flush(), Err(MetricsError::Stopped)));
    assert!(matches!(stopped_drain.drain(), Err(MetricsError::Stopped)));
    assert!(matches!(stopped_shutdown.shutdown(), Err(MetricsError::Stopped)));
}

#[test]
fn test_drain_reports_an_aggregator_that_stops_after_accepting_the_request() {
    let metrics = Metrics::start();
    let tree = Arc::clone(&metrics.tree);
    let poisoner = std::thread::spawn(move || {
        let _guard = tree.write().unwrap();
        panic!("poison metrics tree");
    });
    assert!(poisoner.join().is_err());

    assert!(matches!(metrics.drain(), Err(MetricsError::Stopped)));
}

#[test]
fn test_daily_usage_survives_a_restart() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(42)).unwrap();
    metrics.record(grouped_read("alpha", "resource-b", "3.0", Some("up"), 12));
    flush_and_assert(&metrics, || meta.analytics().load_daily().unwrap().is_some());
    drop(metrics);

    let restarted = Metrics::start_durable(meta.analytics(), None, clock_on_day(42)).unwrap();
    assert_eq!(
        restarted.daily_usage(),
        [DailyUsage {
            day: 42,
            repository: "alpha".into(),
            resource: "resource-b".into(),
            group: "3.0".into(),
            source: "up".into(),
            reads: 1,
            bytes: 12,
        }]
    );
}

#[test]
fn test_exported_daily_batch_contains_aggregated_usage() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(20_000)).unwrap();
    metrics.record(grouped_read("alpha", "resource-b", "3.0", Some("upstream"), 40));
    metrics.record(grouped_read("alpha", "resource-b", "3.0", Some("upstream"), 10));
    flush_and_assert(&metrics, || metrics.daily_usage().len() == 1);

    assert_eq!(
        metrics.export_daily_batch(IntervalId {
            producer: ProducerId("east".into()),
            epoch: AuthorityEpoch(1),
            sequence: 1,
        }),
        AnalyticsBatch {
            interval: IntervalId {
                producer: ProducerId("east".into()),
                epoch: AuthorityEpoch(1),
                sequence: 1,
            },
            rows: vec![AggregateRow {
                key: AggregateKey {
                    day: 20_000,
                    repository: "alpha".into(),
                    resource: "resource-b".into(),
                    group: "3.0".into(),
                    source: "upstream".into(),
                },
                delta: AggregateDelta {
                    downloads: 2,
                    bytes: 50,
                },
            }],
        }
    );
}

#[test]
fn test_export_sealed_day_batches_emits_one_batch_per_completed_day() {
    use std::sync::atomic::Ordering::SeqCst;

    let (_dir, meta) = store();
    let (day, clock) = steppable_clock();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock).unwrap();

    day.store(10, SeqCst);
    metrics.record(grouped_read("alpha", "resource-b", "1.0", Some("up"), 100));
    flush_and_assert(&metrics, || metrics.daily_usage().iter().any(|usage| usage.day == 10));
    day.store(11, SeqCst);
    metrics.record(grouped_read("alpha", "resource-b", "1.0", Some("up"), 200));
    flush_and_assert(&metrics, || metrics.daily_usage().iter().any(|usage| usage.day == 11));
    day.store(12, SeqCst);
    metrics.record(grouped_read("alpha", "resource-b", "1.0", Some("up"), 5));
    flush_and_assert(&metrics, || metrics.daily_usage().iter().any(|usage| usage.day == 12));

    let producer = ProducerId("east".to_owned());
    let expected = vec![
        AnalyticsBatch {
            interval: IntervalId {
                producer: producer.clone(),
                epoch: AuthorityEpoch(1),
                sequence: 10,
            },
            rows: vec![AggregateRow {
                key: AggregateKey {
                    day: 10,
                    repository: "alpha".into(),
                    resource: "resource-b".into(),
                    group: "1.0".into(),
                    source: "up".into(),
                },
                delta: AggregateDelta {
                    downloads: 1,
                    bytes: 100,
                },
            }],
        },
        AnalyticsBatch {
            interval: IntervalId {
                producer: producer.clone(),
                epoch: AuthorityEpoch(1),
                sequence: 11,
            },
            rows: vec![AggregateRow {
                key: AggregateKey {
                    day: 11,
                    repository: "alpha".into(),
                    resource: "resource-b".into(),
                    group: "1.0".into(),
                    source: "up".into(),
                },
                delta: AggregateDelta {
                    downloads: 1,
                    bytes: 200,
                },
            }],
        },
    ];
    assert_eq!(metrics.sealed_batches(&producer, AuthorityEpoch(1), -1), expected);
    assert_eq!(
        metrics.export_sealed_day_batches(&producer, AuthorityEpoch(1), 10),
        expected[1..]
    );
}

#[test]
fn test_export_sealed_day_batches_skips_pre_epoch_days() {
    use std::sync::atomic::Ordering::SeqCst;

    let (_dir, meta) = store();
    let (day, clock) = steppable_clock();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock).unwrap();
    for recorded_day in [-1, 0] {
        day.store(recorded_day, SeqCst);
        metrics.record(grouped_read("alpha", "resource-b", "1.0", Some("up"), 1));
        flush_and_assert(&metrics, || {
            metrics.daily_usage().iter().any(|usage| usage.day == recorded_day)
        });
    }
    day.store(1, SeqCst);

    assert_eq!(
        metrics
            .sealed_batches(&ProducerId("east".to_owned()), AuthorityEpoch(1), i64::MIN)
            .into_iter()
            .map(|batch| (batch.interval.sequence, batch.rows[0].key.day))
            .collect::<Vec<_>>(),
        [(0, 0)]
    );
}

#[test]
fn test_missing_dimensions_restore_as_empty_labels() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(3)).unwrap();
    metrics.record(read("alpha", "resource-b", "resource-b-3.0.bin", 8));
    flush_and_assert(&metrics, || meta.analytics().load_daily().unwrap().is_some());
    drop(metrics);

    let restarted = Metrics::start_durable(meta.analytics(), None, clock_on_day(3)).unwrap();
    assert_eq!(
        restarted.daily_usage(),
        [DailyUsage {
            day: 3,
            repository: "alpha".into(),
            resource: "resource-b".into(),
            group: String::new(),
            source: String::new(),
            reads: 1,
            bytes: 8,
        }]
    );
}

#[test]
fn test_totals_for_routes_preserves_order_without_returning_keys() {
    let metrics = Metrics::start();
    metrics.record(Observation::Page {
        repository: "credential-bearing-repository".into(),
        resource: "actor-token".into(),
    });
    flush_and_assert(&metrics, || {
        metrics.index_totals().contains_key("credential-bearing-repository")
    });

    let totals = metrics.totals_for_routes(["missing", "credential-bearing-repository"]);

    assert_eq!(totals.len(), 2);
    assert_eq!(totals[0].base.pages, 0);
    assert_eq!(totals[1].base.pages, 1);
}

fn durable_on(day: i64, retention: Option<u32>) -> (tempfile::TempDir, MetaStore, Metrics) {
    let (dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), retention, clock_on_day(day)).unwrap();
    (dir, meta, metrics)
}

#[test]
fn test_current_day_reads_the_query_clock() {
    let (_dir, _meta, metrics) = durable_on(1_000, None);
    assert_eq!(metrics.current_day(), 1_000);
}

#[rstest]
#[case::trailing_month(
    None,
    None,
    None,
    UsageInterval {
        from_day: 971,
        to_day: 1_000,
        retained_from_day: None,
        window_clamped_to_retention: false,
    }
)]
#[case::explicit_bounds(
    None,
    Some(950 * SECONDS_PER_DAY),
    Some(3_000 * SECONDS_PER_DAY),
    UsageInterval {
        from_day: 950,
        to_day: 1_000,
        retained_from_day: None,
        window_clamped_to_retention: false,
    }
)]
#[case::maximum_span(
    None,
    Some(0),
    None,
    UsageInterval {
        from_day: 635,
        to_day: 1_000,
        retained_from_day: None,
        window_clamped_to_retention: false,
    }
)]
#[case::retention_floor(
    Some(7),
    Some(0),
    None,
    UsageInterval {
        from_day: 993,
        to_day: 1_000,
        retained_from_day: Some(993),
        window_clamped_to_retention: true,
    }
)]
#[case::retention_boundary(
    Some(7),
    Some(993 * SECONDS_PER_DAY),
    None,
    UsageInterval {
        from_day: 993,
        to_day: 1_000,
        retained_from_day: Some(993),
        window_clamped_to_retention: false,
    }
)]
fn test_resolve_usage_interval(
    #[case] retention: Option<u32>,
    #[case] from: Option<i64>,
    #[case] to: Option<i64>,
    #[case] expected: UsageInterval,
) {
    let (_dir, _meta, metrics) = durable_on(1_000, retention);
    assert_eq!(metrics.resolve_usage_interval(from, to), expected);
}

#[test]
fn test_usage_top_ranks_window_reads_and_scopes_by_repository() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(grouped_read("a", "resource-b", "1.0", None, 10));
    metrics.record(grouped_read("a", "resource-b", "2.0", None, 30));
    metrics.record(grouped_read("a", "django", "5.0", None, 5));
    metrics.record(grouped_read("b", "resource-a", "1.0", None, 99));
    flush_and_assert(&metrics, || metrics.daily_usage().len() == 4);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics.usage_top(None, &interval),
        [
            ResourceUsage {
                repository: "a".into(),
                resource: "resource-b".into(),
                reads: 2,
                bytes: 40,
            },
            ResourceUsage {
                repository: "b".into(),
                resource: "resource-a".into(),
                reads: 1,
                bytes: 99,
            },
            ResourceUsage {
                repository: "a".into(),
                resource: "django".into(),
                reads: 1,
                bytes: 5,
            },
        ]
    );
    assert_eq!(
        metrics.usage_top(Some("a"), &interval),
        [
            ResourceUsage {
                repository: "a".into(),
                resource: "resource-b".into(),
                reads: 2,
                bytes: 40,
            },
            ResourceUsage {
                repository: "a".into(),
                resource: "django".into(),
                reads: 1,
                bytes: 5,
            },
        ]
    );
}

#[test]
fn test_usage_top_is_empty_when_the_window_predates_every_bucket() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(grouped_read("a", "resource-b", "1.0", None, 10));
    flush_and_assert(&metrics, || metrics.daily_usage().len() == 1);
    let interval = metrics.resolve_usage_interval(Some(100 * SECONDS_PER_DAY), Some(200 * SECONDS_PER_DAY));

    assert!(metrics.usage_top(None, &interval).is_empty());
}

#[test]
fn test_usage_top_is_empty_when_interval_is_reversed() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(grouped_read("a", "resource-b", "1.0", None, 10));
    flush_and_assert(&metrics, || metrics.daily_usage().len() == 1);

    assert!(
        metrics
            .usage_top(
                None,
                &UsageInterval {
                    from_day: 501,
                    to_day: 500,
                    retained_from_day: None,
                    window_clamped_to_retention: false,
                },
            )
            .is_empty()
    );
}

#[test]
fn test_usage_window_includes_its_first_day() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(grouped_read("a", "resource-b", "1.0", None, 10));
    flush_and_assert(&metrics, || metrics.daily_usage().len() == 1);

    assert_eq!(
        metrics.usage_top(
            None,
            &UsageInterval {
                from_day: 500,
                to_day: 500,
                retained_from_day: None,
                window_clamped_to_retention: false,
            },
        ),
        [ResourceUsage {
            repository: "a".into(),
            resource: "resource-b".into(),
            reads: 1,
            bytes: 10,
        }]
    );
}

#[test]
fn test_usage_window_excludes_adjacent_days() {
    let (_dir, meta) = store();
    for day in 499..=501 {
        let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(day)).unwrap();
        metrics.record(grouped_read("a", "resource-b", "1.0", None, 10));
        flush_and_assert(&metrics, || meta.analytics().load_daily().unwrap().is_some());
    }
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(501)).unwrap();

    assert_eq!(
        metrics.usage_timeline(
            None,
            &UsageInterval {
                from_day: 500,
                to_day: 500,
                retained_from_day: None,
                window_clamped_to_retention: false,
            },
        ),
        [TimelineBucket {
            day: 500,
            start_unix: 500 * SECONDS_PER_DAY,
            end_unix: 501 * SECONDS_PER_DAY,
            reads: 1,
            bytes: 10,
        }]
    );
}

#[test]
fn test_usage_groups_splits_by_group_and_labels_absent_as_null() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(grouped_read("a", "resource-b", "3.0", None, 10));
    metrics.record(grouped_read("a", "resource-b", "3.0", None, 10));
    metrics.record(read("a", "resource-b", "resource-b.bin", 5));
    flush_and_assert(&metrics, || metrics.daily_usage().len() == 2);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics.usage_groups(None, &interval),
        [
            GroupUsage {
                repository: "a".into(),
                resource: "resource-b".into(),
                group: Some("3.0".into()),
                reads: 2,
                bytes: 20,
            },
            GroupUsage {
                repository: "a".into(),
                resource: "resource-b".into(),
                group: None,
                reads: 1,
                bytes: 5,
            },
        ]
    );
}

#[test]
fn test_usage_sources_splits_by_source_and_labels_local_as_null() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(grouped_read("a", "resource-b", "1.0", Some("alpha"), 10));
    metrics.record(grouped_read("a", "resource-b", "1.0", None, 5));
    flush_and_assert(&metrics, || metrics.daily_usage().len() == 2);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics.usage_sources(None, &interval),
        [
            SourceUsage {
                repository: "a".into(),
                resource: "resource-b".into(),
                source: Some("alpha".into()),
                reads: 1,
                bytes: 10,
            },
            SourceUsage {
                repository: "a".into(),
                resource: "resource-b".into(),
                source: None,
                reads: 1,
                bytes: 5,
            },
        ]
    );
}

#[test]
fn test_usage_timeline_buckets_reads_by_ascending_day() {
    let (_dir, meta) = store();
    let earlier = Metrics::start_durable(meta.analytics(), None, clock_on_day(500)).unwrap();
    earlier.record(grouped_read("a", "resource-b", "1.0", None, 10));
    flush_and_assert(&earlier, || meta.analytics().load_daily().unwrap().is_some());
    drop(earlier);

    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(501)).unwrap();
    metrics.record(grouped_read("a", "resource-b", "1.0", None, 20));
    metrics.record(grouped_read("a", "django", "1.0", None, 3));
    flush_and_assert(&metrics, || metrics.daily_usage().len() == 3);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics.usage_timeline(None, &interval),
        [
            TimelineBucket {
                day: 500,
                start_unix: 500 * SECONDS_PER_DAY,
                end_unix: 501 * SECONDS_PER_DAY,
                reads: 1,
                bytes: 10,
            },
            TimelineBucket {
                day: 501,
                start_unix: 501 * SECONDS_PER_DAY,
                end_unix: 502 * SECONDS_PER_DAY,
                reads: 2,
                bytes: 23,
            },
        ]
    );
}

#[test]
fn test_usage_unused_distinguishes_idle_resources_from_active_and_page_only() {
    let (_dir, meta) = store();
    let past = Metrics::start_durable(meta.analytics(), None, clock_on_day(100)).unwrap();
    past.record(read("a", "old", "old.bin", 7));
    past.record(read("a", "old", "old.bin", 7));
    flush_and_assert(&past, || persisted_reads(&meta.analytics()) == Some(2));
    drop(past);

    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(500)).unwrap();
    metrics.record(read("a", "resource-b", "resource-b.bin", 10));
    metrics.record(Observation::Page {
        repository: "a".into(),
        resource: "page-only".into(),
    });
    let interval = metrics.resolve_usage_interval(None, None);
    flush_and_assert(&metrics, || metrics.usage_top(None, &interval).len() == 1);

    assert_eq!(
        metrics.usage_unused(None, &interval),
        [UnusedResource {
            repository: "a".into(),
            resource: "old".into(),
            lifetime_reads: 2,
        }]
    );
    assert!(metrics.usage_unused(Some("other"), &interval).is_empty());
}

#[test]
fn test_usage_top_breaks_ties_by_repository_then_resource() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(grouped_read("a", "alpha", "1.0", None, 10));
    metrics.record(grouped_read("a", "beta", "1.0", None, 10));
    metrics.record(grouped_read("b", "alpha", "1.0", None, 10));
    flush_and_assert(&metrics, || metrics.daily_usage().len() == 3);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics
            .usage_top(None, &interval)
            .into_iter()
            .map(|row| (row.repository, row.resource))
            .collect::<Vec<_>>(),
        [
            ("a".to_owned(), "alpha".to_owned()),
            ("a".to_owned(), "beta".to_owned()),
            ("b".to_owned(), "alpha".to_owned()),
        ]
    );
}

#[test]
fn test_usage_groups_breaks_ties_by_group() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(grouped_read("a", "resource-b", "2.0", None, 10));
    metrics.record(grouped_read("a", "resource-b", "1.0", None, 10));
    flush_and_assert(&metrics, || metrics.daily_usage().len() == 2);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics
            .usage_groups(None, &interval)
            .into_iter()
            .map(|row| row.group)
            .collect::<Vec<_>>(),
        [Some("1.0".to_owned()), Some("2.0".to_owned())]
    );
}

#[test]
fn test_usage_sources_breaks_ties_by_source() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(grouped_read("a", "resource-b", "1.0", Some("beta"), 10));
    metrics.record(grouped_read("a", "resource-b", "1.0", Some("alpha"), 10));
    flush_and_assert(&metrics, || metrics.daily_usage().len() == 2);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics
            .usage_sources(None, &interval)
            .into_iter()
            .map(|row| row.source)
            .collect::<Vec<_>>(),
        [Some("alpha".to_owned()), Some("beta".to_owned())]
    );
}

#[test]
fn test_usage_unused_breaks_ties_by_repository_then_resource() {
    let (_dir, meta) = store();
    let past = Metrics::start_durable(meta.analytics(), None, clock_on_day(100)).unwrap();
    for (repository, resource) in [("a", "alpha"), ("a", "beta"), ("b", "alpha")] {
        past.record(read(repository, resource, "file.bin", 5));
    }
    flush_and_assert(&past, || persisted_reads(&meta.analytics()) == Some(3));
    drop(past);

    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(500)).unwrap();
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics
            .usage_unused(None, &interval)
            .into_iter()
            .map(|row| (row.repository, row.resource, row.lifetime_reads))
            .collect::<Vec<_>>(),
        [
            ("a".to_owned(), "alpha".to_owned(), 1),
            ("a".to_owned(), "beta".to_owned(), 1),
            ("b".to_owned(), "alpha".to_owned(), 1),
        ]
    );
}
