use std::sync::mpsc::channel;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use peryx_ha_distributed::{
    AggregateDelta, AggregateKey, ApplyLimits, ApplyOutcome, ApplyState, AuthorityEpoch, IntervalId, ProducerId,
};
use peryx_storage::meta::{AnalyticsHandle, MetaStore};

use super::{
    Aggregator, Clock, DailyBuckets, DailyKey, DailySnapshot, DailyTotals, DailyUsage, DownloadSnapshot, Event,
    FLUSH_INTERVAL, FlushPolicy, FlushState, Message, Metrics, PackageUsage, SECONDS_PER_DAY, SourceUsage, StatsTree,
    TimelineBucket, UnusedPackage, UsageInterval, VersionUsage, aggregate, daily_rows, encode_daily_snapshot,
    flush_due, fold_daily_batch, step,
};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

/// A clock frozen at `day`'s noon, so a test dates every download to one deterministic UTC bucket.
fn clock_on_day(day: i64) -> Clock {
    Arc::new(move || day * SECONDS_PER_DAY + SECONDS_PER_DAY / 2)
}

/// A clock the test advances between settled writes, so one running aggregator can date buckets on
/// different days and cross its own retention window without a restart.
fn steppable_clock() -> (std::sync::Arc<std::sync::atomic::AtomicI64>, Clock) {
    let day = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let handle = std::sync::Arc::clone(&day);
    let clock: Clock =
        Arc::new(move || handle.load(std::sync::atomic::Ordering::SeqCst) * SECONDS_PER_DAY + SECONDS_PER_DAY / 2);
    (day, clock)
}

fn settle_and_assert(metrics: &Metrics, done: impl Fn() -> bool) {
    // Drain the aggregator through its barrier, then assert the state it settled on.
    metrics.settle();
    assert!(done(), "metrics settled on an unexpected state");
}

fn persisted_downloads(store: &AnalyticsHandle) -> Option<u64> {
    let bytes = store.load().unwrap()?;
    let snapshot: DownloadSnapshot = serde_json::from_slice(&bytes).unwrap();
    Some(snapshot.files.iter().map(|file| file.downloads).sum())
}

fn download(route: &str, project: &str, filename: &str, bytes: u64) -> Event {
    Event::Download {
        route: route.into(),
        project: project.into(),
        filename: filename.into(),
        version: None,
        source: None,
        bytes,
    }
}

fn download_of(route: &str, project: &str, version: &str, source: Option<&str>, bytes: u64) -> Event {
    Event::Download {
        route: route.into(),
        project: project.into(),
        filename: format!("{project}-{version}.bin"),
        version: Some(version.into()),
        source: source.map(Into::into),
        bytes,
    }
}

#[test]
fn test_record_bounds_the_queue_and_counts_drops_under_overload() {
    // A stalled aggregator must cap retained work at the queue capacity rather than absorb overload
    // into unbounded memory. Hold the write lock the aggregator needs to apply an event, which parks
    // it after it has pulled at most one event off the channel, then flood the recorder well past
    // capacity: the buffer saturates, every further record is dropped, and the drops are counted.
    let capacity = 8;
    let metrics = Metrics::spawn(None, None, clock_on_day(0), capacity);
    let sends = (capacity * 4) as u64;
    let processed = {
        let _parked = metrics.tree.write().unwrap();
        for _ in 0..sends {
            metrics.record(download("alpha", "numpy", "numpy-1.bin", 1));
        }
        assert!(metrics.dropped() > 0, "overload was not reported");
        sends - metrics.dropped()
    };
    assert!(
        processed <= capacity as u64 + 1,
        "retained {processed} events, past the {capacity}-slot bound",
    );
    metrics.settle();
    let aggregated: u64 = metrics
        .index_totals()
        .values()
        .map(|counters| counters.base.downloads)
        .sum();
    assert_eq!(
        aggregated, processed,
        "every event that was not dropped must be aggregated"
    );
}

#[test]
fn test_encode_daily_snapshot_seeds_a_producer_offline() {
    let (_dir, meta) = store();
    let seeded = DailyUsage {
        day: 19_000,
        repository: "hosted".to_owned(),
        project: "veloxdemo".to_owned(),
        version: "1.0.0".to_owned(),
        source: String::new(),
        downloads: 7,
        bytes: 4096,
    };
    meta.analytics()
        .save_daily(&encode_daily_snapshot(vec![seeded.clone()]))
        .unwrap();
    // A durable aggregator restores the seeded bucket on boot through the same path a live download
    // would have written, so an offline seed becomes an exportable sealed day.
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(seeded.day + 1));
    assert_eq!(metrics.daily_usage(), [seeded]);
}

#[test]
fn test_durable_downloads_survive_a_restart() {
    let (_dir, meta) = store();
    let filename = "pandas-3.0-py3-none-any.bin";
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(0));
    metrics.record(Event::Page {
        route: "root/alpha".into(),
        project: "pandas".into(),
    });
    metrics.record(download("root/alpha", "pandas", filename, 100));
    metrics.record(download("root/alpha", "pandas", filename, 50));
    settle_and_assert(&metrics, || persisted_downloads(&meta.analytics()) == Some(2));
    drop(metrics);

    let restarted = Metrics::start_durable(meta.analytics(), None, clock_on_day(0));
    let totals = restarted.index_totals();
    let index = &totals["root/alpha"];
    assert_eq!(index.base.downloads, 2);
    assert_eq!(index.base.bytes, 150);
    let files = restarted.drill(Some("root/alpha"), Some("pandas"));
    assert_eq!(files["files"][filename]["downloads"], 2);
    assert_eq!(files["files"][filename]["bytes"], 150);
}

#[test]
fn test_usage_totals_reports_lifetime_by_repository() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(0));
    metrics.record(download("alpha", "numpy", "numpy-1.bin", 100));
    metrics.record(download("alpha", "numpy", "numpy-1.bin", 100));
    metrics.record(download("alpha", "scipy", "scipy-1.bin", 50));
    metrics.record(download("alpha", "flask", "flask-1.bin", 40));
    metrics.record(download("other", "django", "django-1.bin", 30));
    settle_and_assert(&metrics, || metrics.usage_totals(None).len() == 4);

    // flask and scipy tie on downloads within the same repository, so ordering falls through to the
    // project tiebreak; django ties on downloads but under a different repository, exercising the
    // repository tiebreak.
    assert_eq!(
        metrics.usage_totals(None),
        [
            PackageUsage {
                repository: "alpha".into(),
                project: "numpy".into(),
                downloads: 2,
                bytes: 200,
            },
            PackageUsage {
                repository: "alpha".into(),
                project: "flask".into(),
                downloads: 1,
                bytes: 40,
            },
            PackageUsage {
                repository: "alpha".into(),
                project: "scipy".into(),
                downloads: 1,
                bytes: 50,
            },
            PackageUsage {
                repository: "other".into(),
                project: "django".into(),
                downloads: 1,
                bytes: 30,
            },
        ]
    );
    assert_eq!(
        metrics.usage_totals(Some("other")),
        [PackageUsage {
            repository: "other".into(),
            project: "django".into(),
            downloads: 1,
            bytes: 30,
        }]
    );
    assert!(metrics.usage_totals(Some("missing")).is_empty());
}

#[test]
fn test_batches_without_a_download_persist_nothing() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(0));
    metrics.record(Event::Page {
        route: "alpha".into(),
        project: "flask".into(),
    });
    settle_and_assert(&metrics, || {
        metrics
            .index_totals()
            .get("alpha")
            .is_some_and(|totals| totals.base.pages == 1)
    });
    assert_eq!(persisted_downloads(&meta.analytics()), None);
    assert!(meta.analytics().load_daily().unwrap().is_none());
}

#[test]
fn test_daily_buckets_split_by_version_source_and_day() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(20_000));
    metrics.record(download_of("alpha", "flask", "3.0", Some("upstream"), 10));
    metrics.record(download_of("alpha", "flask", "3.0", Some("upstream"), 40));
    metrics.record(download_of("alpha", "flask", "2.0", Some("upstream"), 5));
    metrics.record(download_of("alpha", "flask", "3.0", None, 7));
    settle_and_assert(&metrics, || metrics.daily_usage().len() == 3);

    assert_eq!(
        metrics.daily_usage(),
        [
            DailyUsage {
                day: 20_000,
                repository: "alpha".into(),
                project: "flask".into(),
                version: "2.0".into(),
                source: "upstream".into(),
                downloads: 1,
                bytes: 5,
            },
            DailyUsage {
                day: 20_000,
                repository: "alpha".into(),
                project: "flask".into(),
                version: "3.0".into(),
                source: String::new(),
                downloads: 1,
                bytes: 7,
            },
            DailyUsage {
                day: 20_000,
                repository: "alpha".into(),
                project: "flask".into(),
                version: "3.0".into(),
                source: "upstream".into(),
                downloads: 2,
                bytes: 50,
            },
        ]
    );
}

#[test]
fn test_retention_drops_expired_days_and_keeps_retained_totals() {
    let (_dir, meta) = store();
    let old = Metrics::start_durable(meta.analytics(), Some(7), clock_on_day(100));
    old.record(download_of("alpha", "flask", "1.0", Some("up"), 3));
    settle_and_assert(&old, || old.daily_usage().len() == 1);
    drop(old);

    // Ten days later a fresh download lands; the day-100 bucket is now beyond the 7-day window.
    let metrics = Metrics::start_durable(meta.analytics(), Some(7), clock_on_day(110));
    metrics.record(download_of("alpha", "flask", "2.0", Some("up"), 9));
    settle_and_assert(&metrics, || metrics.daily_usage().iter().any(|row| row.day == 110));

    assert_eq!(
        metrics.daily_usage(),
        [DailyUsage {
            day: 110,
            repository: "alpha".into(),
            project: "flask".into(),
            version: "2.0".into(),
            source: "up".into(),
            downloads: 1,
            bytes: 9,
        }]
    );
}

#[test]
fn test_the_running_aggregator_expires_a_bucket_that_ages_past_retention() {
    use std::sync::atomic::Ordering;

    let (_dir, meta) = store();
    let (day, clock) = steppable_clock();
    let metrics = Metrics::start_durable(meta.analytics(), Some(2), clock);

    metrics.record(download_of("alpha", "flask", "1.0", Some("up"), 3));
    settle_and_assert(&metrics, || metrics.daily_usage().iter().any(|row| row.day == 0));

    // Advance five days so the day-0 bucket falls outside the two-day window, then record again:
    // the running aggregator applies retention to the live map after startup.
    day.store(5, Ordering::SeqCst);
    metrics.record(download_of("alpha", "flask", "2.0", Some("up"), 9));
    settle_and_assert(&metrics, || metrics.daily_usage().iter().any(|row| row.day == 5));

    let days: Vec<i64> = metrics.daily_usage().iter().map(|row| row.day).collect();
    assert_eq!(
        days,
        [5],
        "the aged day-0 bucket expired during aggregation, leaving only day 5"
    );
}

#[test]
fn test_aggregate_flushes_retention_within_the_batch() {
    // The running aggregator applies retention inside the download batch on its own thread, so the
    // settle-based tests reach that flush only when the batch wins the race with the coverage
    // snapshot. Drive the loop directly over a channel that closes after one batch: the retention
    // flush then runs on every architecture and run.
    let clock = clock_on_day(30);
    let stale = DailyKey {
        day: 10,
        repository: "alpha".into(),
        project: "flask".into(),
        version: "1.0".into(),
        source: "up".into(),
    };
    let mut seeded = DailyBuckets::new();
    seeded.insert(
        stale,
        DailyTotals {
            downloads: 5,
            bytes: 50,
        },
    );
    let daily = Arc::new(RwLock::new(seeded));
    let tree = Arc::new(RwLock::new(StatsTree::new()));

    let (sender, receiver) = channel();
    sender
        .send(Message::Event(download_of("alpha", "flask", "2.0", Some("up"), 9)))
        .unwrap();
    drop(sender);

    aggregate(&receiver, &tree, &daily, None, Some(7), &clock, FLUSH_INTERVAL);

    let rows = daily_rows(&daily.read().unwrap());
    assert_eq!(
        rows.len(),
        1,
        "the day-10 bucket outside the 7-day window survived: {rows:?}"
    );
    assert_eq!(rows[0].day, 30);
    assert_eq!(rows[0].downloads, 1);
    assert_eq!(rows[0].bytes, 9);
}

fn bucket(day: i64, version: &str) -> DailyKey {
    DailyKey {
        day,
        repository: "alpha".into(),
        project: "flask".into(),
        version: version.into(),
        source: "up".into(),
    }
}

fn seeded_daily(day: i64, version: &str, downloads: u64, bytes: u64) -> RwLock<DailyBuckets> {
    let mut seeded = DailyBuckets::new();
    seeded.insert(bucket(day, version), DailyTotals { downloads, bytes });
    RwLock::new(seeded)
}

#[test]
fn test_fold_daily_batch_expires_aged_buckets() {
    // Drive the retention flush on the test thread, where x86 coverage captures it, instead of only
    // on the aggregator's spawned thread.
    let daily = seeded_daily(10, "1.0", 5, 50);

    fold_daily_batch(vec![(bucket(30, "2.0"), 9)], &daily, Some(7), &clock_on_day(30));

    let rows = daily_rows(&daily.read().unwrap());
    assert_eq!(rows.len(), 1, "the aged day-10 bucket survived retention: {rows:?}");
    assert_eq!((rows[0].day, rows[0].downloads, rows[0].bytes), (30, 1, 9));
}

#[test]
fn test_fold_daily_batch_without_retention_keeps_every_bucket() {
    let daily = seeded_daily(10, "1.0", 5, 50);

    fold_daily_batch(vec![(bucket(30, "2.0"), 9)], &daily, None, &clock_on_day(30));

    // No retention window: both buckets survive untouched.
    assert_eq!(daily_rows(&daily.read().unwrap()).len(), 2);
}

#[test]
fn test_fold_daily_batch_ignores_an_empty_batch() {
    let daily = RwLock::new(DailyBuckets::new());
    fold_daily_batch(Vec::new(), &daily, Some(7), &clock_on_day(30));
    assert!(daily_rows(&daily.read().unwrap()).is_empty());
}

#[rstest::rstest]
#[case::clean_never_checkpoints(false, false, 0, false)]
#[case::pending_within_interval_waits(true, false, 4, false)]
#[case::pending_reaching_interval_flushes(true, false, 5, true)]
#[case::pending_forced_flushes_early(true, true, 0, true)]
fn test_flush_due(#[case] pending: bool, #[case] force: bool, #[case] elapsed: i64, #[case] expected: bool) {
    assert_eq!(flush_due(100 + elapsed, 100, pending, force, 5), expected);
}

/// A clock the test moves in whole seconds, so a checkpoint's elapsed-interval gate can be crossed
/// without waiting on the wall clock.
fn seconds_clock() -> (Arc<std::sync::atomic::AtomicI64>, Clock) {
    let secs = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let handle = Arc::clone(&secs);
    let clock: Clock = Arc::new(move || handle.load(std::sync::atomic::Ordering::SeqCst));
    (secs, clock)
}

fn context<'a>(
    tree: &'a RwLock<StatsTree>,
    daily: &'a RwLock<DailyBuckets>,
    store: &'a AnalyticsHandle,
    clock: &'a Clock,
) -> Aggregator<'a> {
    Aggregator {
        tree,
        daily,
        store: Some(store),
        retention_days: None,
        clock,
    }
}

#[test]
fn test_step_coalesces_isolated_downloads_until_forced() {
    // Each download arrives as its own batch under a frozen clock, so the coalescing interval never
    // elapses: the aggregator folds every one into the live buckets but writes nothing durable until
    // a barrier forces the checkpoint. This is the write-frequency claim the issue asks for, decoupled
    // from download count.
    let (_dir, meta) = store();
    let handle = meta.analytics();
    let (_secs, clock) = seconds_clock();
    let tree = RwLock::new(StatsTree::new());
    let daily = RwLock::new(DailyBuckets::new());
    let ctx = context(&tree, &daily, &handle, &clock);
    let policy = FlushPolicy {
        idle: Duration::from_millis(5),
        interval_secs: 5,
    };
    let mut state = FlushState::new((clock)(), true);

    let (sender, receiver) = channel();
    sender
        .send(Message::Event(download_of("alpha", "flask", "1.0", None, 10)))
        .unwrap();
    assert!(step(&receiver, &ctx, policy, &mut state));
    sender
        .send(Message::Event(download_of("alpha", "flask", "1.0", None, 20)))
        .unwrap();
    assert!(step(&receiver, &ctx, policy, &mut state));

    // Two folded downloads, nothing on disk: the interval swallowed both writes.
    assert_eq!(daily_rows(&daily.read().unwrap())[0].downloads, 2);
    assert!(
        handle.load_daily().unwrap().is_none(),
        "an isolated download wrote through"
    );
    assert!(handle.load().unwrap().is_none());
    assert!(state.pending());

    let (ack, done) = channel();
    sender.send(Message::Barrier(ack)).unwrap();
    assert!(step(&receiver, &ctx, policy, &mut state));
    done.recv().unwrap();

    // The forced checkpoint persists the coalesced totals once and clears the pending mark.
    assert_eq!(persisted_downloads(&handle), Some(2));
    assert_eq!(persisted_daily_downloads(&handle), Some(2));
    assert!(!state.pending());
}

fn persisted_daily_downloads(store: &AnalyticsHandle) -> Option<u64> {
    let bytes = store.load_daily().unwrap()?;
    let snapshot: DailySnapshot = serde_json::from_slice(&bytes).unwrap();
    Some(snapshot.buckets.iter().map(|bucket| bucket.downloads).sum())
}

#[test]
fn test_step_flushes_a_dirty_loop_when_the_interval_elapses_while_idle() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    let (secs, clock) = seconds_clock();
    let tree = RwLock::new(StatsTree::new());
    let daily = RwLock::new(DailyBuckets::new());
    let ctx = context(&tree, &daily, &handle, &clock);
    let policy = FlushPolicy {
        idle: Duration::from_millis(5),
        interval_secs: 5,
    };
    let mut state = FlushState::new((clock)(), true);

    let (sender, receiver) = channel();
    sender
        .send(Message::Event(download_of("alpha", "flask", "1.0", None, 8)))
        .unwrap();
    assert!(step(&receiver, &ctx, policy, &mut state));
    assert!(handle.load_daily().unwrap().is_none());

    // No further events: the idle timer wakes the loop, and by now the interval has elapsed, so the
    // pending download checkpoints without any traffic to trigger it.
    secs.store(5, std::sync::atomic::Ordering::SeqCst);
    assert!(step(&receiver, &ctx, policy, &mut state));

    assert_eq!(persisted_downloads(&handle), Some(1));
    assert!(!state.pending());
    drop(sender);
}

#[test]
fn test_step_flushes_a_pending_loop_on_shutdown() {
    let (_dir, meta) = store();
    let handle = meta.analytics();
    let (_secs, clock) = seconds_clock();
    let tree = RwLock::new(StatsTree::new());
    let daily = RwLock::new(DailyBuckets::new());
    let ctx = context(&tree, &daily, &handle, &clock);
    let policy = FlushPolicy {
        idle: Duration::from_millis(5),
        interval_secs: 5,
    };
    let mut state = FlushState::new((clock)(), true);

    let (sender, receiver) = channel();
    sender
        .send(Message::Event(download_of("alpha", "flask", "1.0", None, 4)))
        .unwrap();
    assert!(step(&receiver, &ctx, policy, &mut state));
    assert!(handle.load_daily().unwrap().is_none());

    // The senders drop; the closing channel drives one last checkpoint so the coalesced tail is
    // never lost to an orderly shutdown.
    drop(sender);
    assert!(!step(&receiver, &ctx, policy, &mut state));

    assert_eq!(persisted_downloads(&handle), Some(1));
    assert!(!state.pending());
}

#[test]
fn test_daily_usage_survives_a_restart() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(42));
    metrics.record(download_of("alpha", "flask", "3.0", Some("up"), 12));
    settle_and_assert(&metrics, || meta.analytics().load_daily().unwrap().is_some());
    drop(metrics);

    let restarted = Metrics::start_durable(meta.analytics(), None, clock_on_day(42));
    assert_eq!(
        restarted.daily_usage(),
        [DailyUsage {
            day: 42,
            repository: "alpha".into(),
            project: "flask".into(),
            version: "3.0".into(),
            source: "up".into(),
            downloads: 1,
            bytes: 12,
        }]
    );
}

#[test]
fn test_exported_daily_batch_applies_once_on_a_replica() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(20_000));
    metrics.record(download_of("alpha", "flask", "3.0", Some("upstream"), 40));
    metrics.record(download_of("alpha", "flask", "3.0", Some("upstream"), 10));
    settle_and_assert(&metrics, || metrics.daily_usage().len() == 1);

    let interval = IntervalId {
        producer: ProducerId("east".into()),
        epoch: AuthorityEpoch(1),
        sequence: 1,
    };
    let export = metrics.export_daily_batch(interval);
    assert_eq!(export.rows.len(), 1);

    let dimension = AggregateKey {
        day: 20_000,
        repository: "alpha".into(),
        project: "flask".into(),
        version: "3.0".into(),
        source: "upstream".into(),
    };
    let mut replica = ApplyState::new(ApplyLimits::default());
    assert_eq!(replica.apply(&export).unwrap(), ApplyOutcome::Applied);
    assert_eq!(
        replica.total(&dimension),
        AggregateDelta {
            downloads: 2,
            bytes: 50
        }
    );

    assert_eq!(replica.apply(&export).unwrap(), ApplyOutcome::Duplicate);
    assert_eq!(
        replica.total(&dimension),
        AggregateDelta {
            downloads: 2,
            bytes: 50
        }
    );
}

#[test]
fn test_export_sealed_day_batches_emits_one_batch_per_completed_day() {
    use std::sync::atomic::Ordering::SeqCst;

    let (_dir, meta) = store();
    let (day, clock) = steppable_clock();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock);

    day.store(10, SeqCst);
    metrics.record(download_of("alpha", "flask", "1.0", Some("up"), 100));
    settle_and_assert(&metrics, || metrics.daily_usage().iter().any(|usage| usage.day == 10));
    day.store(11, SeqCst);
    metrics.record(download_of("alpha", "flask", "1.0", Some("up"), 200));
    settle_and_assert(&metrics, || metrics.daily_usage().iter().any(|usage| usage.day == 11));
    // The current day has activity too, but it is not yet sealed and must be withheld.
    day.store(12, SeqCst);
    metrics.record(download_of("alpha", "flask", "1.0", Some("up"), 5));
    settle_and_assert(&metrics, || metrics.daily_usage().iter().any(|usage| usage.day == 12));

    let producer = ProducerId("east".to_owned());
    let batches = metrics.export_sealed_day_batches(&producer, AuthorityEpoch(1), -1);

    assert_eq!(
        batches.iter().map(|b| b.interval.sequence).collect::<Vec<_>>(),
        vec![10, 11]
    );
    assert_eq!(batches[0].interval.producer, producer);
    assert_eq!(batches[0].interval.epoch, AuthorityEpoch(1));
    assert_eq!(batches[0].rows[0].delta.downloads, 1);
    assert_eq!(batches[0].rows[0].delta.bytes, 100);

    // A watermark past day 10 withholds it and yields only day 11.
    let after = metrics.export_sealed_day_batches(&producer, AuthorityEpoch(1), 10);
    assert_eq!(after.iter().map(|b| b.interval.sequence).collect::<Vec<_>>(), vec![11]);
}

#[test]
fn test_malformed_daily_snapshot_rebuilds_without_blocking_startup() {
    let (_dir, meta) = store();
    meta.analytics().save_daily(b"{ not valid json").unwrap();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(7));
    assert!(metrics.daily_usage().is_empty());

    metrics.record(download_of("alpha", "flask", "3.0", Some("up"), 4));
    settle_and_assert(&metrics, || metrics.daily_usage().len() == 1);
    assert_eq!(metrics.daily_usage()[0].bytes, 4);
}

#[test]
fn test_unknown_daily_schema_rebuilds_from_zero() {
    let (_dir, meta) = store();
    let future = DailySnapshot {
        schema: super::DAILY_SCHEMA + 1,
        buckets: vec![DailyUsage {
            day: 1,
            repository: "alpha".into(),
            project: "flask".into(),
            version: "9.9".into(),
            source: "up".into(),
            downloads: 99,
            bytes: 99,
        }],
    };
    meta.analytics()
        .save_daily(&serde_json::to_vec(&future).unwrap())
        .unwrap();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(7));
    assert!(metrics.daily_usage().is_empty());
}

#[test]
fn test_missing_dimensions_restore_as_empty_labels() {
    let (_dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(3));
    metrics.record(download("alpha", "flask", "flask-3.0.bin", 8));
    settle_and_assert(&metrics, || meta.analytics().load_daily().unwrap().is_some());
    drop(metrics);

    let restarted = Metrics::start_durable(meta.analytics(), None, clock_on_day(3));
    assert_eq!(
        restarted.daily_usage(),
        [DailyUsage {
            day: 3,
            repository: "alpha".into(),
            project: "flask".into(),
            version: String::new(),
            source: String::new(),
            downloads: 1,
            bytes: 8,
        }]
    );
}

#[test]
fn test_totals_for_routes_preserves_order_without_returning_keys() {
    let metrics = Metrics::start();
    metrics.record(Event::Page {
        route: "credential-bearing-route".into(),
        project: "actor-token".into(),
    });
    settle_and_assert(&metrics, || {
        metrics.index_totals().contains_key("credential-bearing-route")
    });

    let totals = metrics.totals_for_routes(["missing", "credential-bearing-route"]);

    assert_eq!(totals.len(), 2);
    assert_eq!(totals[0].base.pages, 0);
    assert_eq!(totals[1].base.pages, 1);
}

fn durable_on(day: i64, retention: Option<u32>) -> (tempfile::TempDir, MetaStore, Metrics) {
    let (dir, meta) = store();
    let metrics = Metrics::start_durable(meta.analytics(), retention, clock_on_day(day));
    (dir, meta, metrics)
}

#[test]
fn test_current_day_reads_the_query_clock() {
    let (_dir, _meta, metrics) = durable_on(1_000, None);
    assert_eq!(metrics.current_day(), 1_000);
}

#[test]
fn test_resolve_usage_interval_defaults_to_the_trailing_month() {
    let (_dir, _meta, metrics) = durable_on(1_000, None);
    assert_eq!(
        metrics.resolve_usage_interval(None, None),
        UsageInterval {
            from_day: 971,
            to_day: 1_000,
            retained_from_day: None,
            window_clamped_to_retention: false,
        }
    );
}

#[test]
fn test_resolve_usage_interval_honors_explicit_bounds_and_never_runs_past_today() {
    let (_dir, _meta, metrics) = durable_on(1_000, None);
    let interval = metrics.resolve_usage_interval(Some(950 * SECONDS_PER_DAY), Some(3_000 * SECONDS_PER_DAY));
    assert_eq!(interval.from_day, 950);
    assert_eq!(interval.to_day, 1_000);
}

#[test]
fn test_resolve_usage_interval_caps_span_without_retention() {
    let (_dir, _meta, metrics) = durable_on(1_000, None);
    assert_eq!(
        metrics.resolve_usage_interval(Some(0), None),
        UsageInterval {
            from_day: 635,
            to_day: 1_000,
            retained_from_day: None,
            window_clamped_to_retention: false,
        }
    );
}

#[test]
fn test_resolve_usage_interval_clamps_to_retention_floor() {
    let (_dir, _meta, metrics) = durable_on(1_000, Some(7));
    assert_eq!(
        metrics.resolve_usage_interval(Some(0), None),
        UsageInterval {
            from_day: 993,
            to_day: 1_000,
            retained_from_day: Some(993),
            window_clamped_to_retention: true,
        }
    );
}

#[test]
fn test_usage_top_ranks_window_downloads_and_scopes_by_repository() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(download_of("a", "flask", "1.0", None, 10));
    metrics.record(download_of("a", "flask", "2.0", None, 30));
    metrics.record(download_of("a", "django", "5.0", None, 5));
    metrics.record(download_of("b", "numpy", "1.0", None, 99));
    settle_and_assert(&metrics, || metrics.daily_usage().len() == 4);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics.usage_top(None, &interval),
        [
            PackageUsage {
                repository: "a".into(),
                project: "flask".into(),
                downloads: 2,
                bytes: 40,
            },
            PackageUsage {
                repository: "b".into(),
                project: "numpy".into(),
                downloads: 1,
                bytes: 99,
            },
            PackageUsage {
                repository: "a".into(),
                project: "django".into(),
                downloads: 1,
                bytes: 5,
            },
        ]
    );
    assert_eq!(
        metrics.usage_top(Some("a"), &interval),
        [
            PackageUsage {
                repository: "a".into(),
                project: "flask".into(),
                downloads: 2,
                bytes: 40,
            },
            PackageUsage {
                repository: "a".into(),
                project: "django".into(),
                downloads: 1,
                bytes: 5,
            },
        ]
    );
}

#[test]
fn test_usage_top_is_empty_when_the_window_predates_every_bucket() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(download_of("a", "flask", "1.0", None, 10));
    settle_and_assert(&metrics, || metrics.daily_usage().len() == 1);
    let interval = metrics.resolve_usage_interval(Some(100 * SECONDS_PER_DAY), Some(200 * SECONDS_PER_DAY));

    assert!(metrics.usage_top(None, &interval).is_empty());
}

#[test]
fn test_usage_versions_splits_by_version_and_labels_absent_as_null() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(download_of("a", "flask", "3.0", None, 10));
    metrics.record(download_of("a", "flask", "3.0", None, 10));
    metrics.record(download("a", "flask", "flask.bin", 5));
    settle_and_assert(&metrics, || metrics.daily_usage().len() == 2);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics.usage_versions(None, &interval),
        [
            VersionUsage {
                repository: "a".into(),
                project: "flask".into(),
                version: Some("3.0".into()),
                downloads: 2,
                bytes: 20,
            },
            VersionUsage {
                repository: "a".into(),
                project: "flask".into(),
                version: None,
                downloads: 1,
                bytes: 5,
            },
        ]
    );
}

#[test]
fn test_usage_sources_splits_by_source_and_labels_local_as_null() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(download_of("a", "flask", "1.0", Some("alpha"), 10));
    metrics.record(download_of("a", "flask", "1.0", None, 5));
    settle_and_assert(&metrics, || metrics.daily_usage().len() == 2);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics.usage_sources(None, &interval),
        [
            SourceUsage {
                repository: "a".into(),
                project: "flask".into(),
                source: Some("alpha".into()),
                downloads: 1,
                bytes: 10,
            },
            SourceUsage {
                repository: "a".into(),
                project: "flask".into(),
                source: None,
                downloads: 1,
                bytes: 5,
            },
        ]
    );
}

#[test]
fn test_usage_timeline_buckets_downloads_by_ascending_day() {
    let (_dir, meta) = store();
    let earlier = Metrics::start_durable(meta.analytics(), None, clock_on_day(500));
    earlier.record(download_of("a", "flask", "1.0", None, 10));
    settle_and_assert(&earlier, || meta.analytics().load_daily().unwrap().is_some());
    drop(earlier);

    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(501));
    metrics.record(download_of("a", "flask", "1.0", None, 20));
    metrics.record(download_of("a", "django", "1.0", None, 3));
    settle_and_assert(&metrics, || metrics.daily_usage().len() == 3);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics.usage_timeline(None, &interval),
        [
            TimelineBucket {
                day: 500,
                start_unix: 500 * SECONDS_PER_DAY,
                end_unix: 501 * SECONDS_PER_DAY,
                downloads: 1,
                bytes: 10,
            },
            TimelineBucket {
                day: 501,
                start_unix: 501 * SECONDS_PER_DAY,
                end_unix: 502 * SECONDS_PER_DAY,
                downloads: 2,
                bytes: 23,
            },
        ]
    );
}

#[test]
fn test_usage_unused_distinguishes_idle_projects_from_active_and_page_only() {
    let (_dir, meta) = store();
    let past = Metrics::start_durable(meta.analytics(), None, clock_on_day(100));
    past.record(download("a", "old", "old.bin", 7));
    past.record(download("a", "old", "old.bin", 7));
    settle_and_assert(&past, || persisted_downloads(&meta.analytics()) == Some(2));
    drop(past);

    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(500));
    metrics.record(download("a", "flask", "flask.bin", 10));
    metrics.record(Event::Page {
        route: "a".into(),
        project: "page-only".into(),
    });
    let interval = metrics.resolve_usage_interval(None, None);
    settle_and_assert(&metrics, || metrics.usage_top(None, &interval).len() == 1);

    assert_eq!(
        metrics.usage_unused(None, &interval),
        [UnusedPackage {
            repository: "a".into(),
            project: "old".into(),
            lifetime_downloads: 2,
        }]
    );
    assert!(metrics.usage_unused(Some("other"), &interval).is_empty());
}

#[test]
fn test_usage_top_breaks_ties_by_repository_then_project() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(download_of("a", "alpha", "1.0", None, 10));
    metrics.record(download_of("a", "beta", "1.0", None, 10));
    metrics.record(download_of("b", "alpha", "1.0", None, 10));
    settle_and_assert(&metrics, || metrics.daily_usage().len() == 3);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics
            .usage_top(None, &interval)
            .into_iter()
            .map(|row| (row.repository, row.project))
            .collect::<Vec<_>>(),
        [
            ("a".to_owned(), "alpha".to_owned()),
            ("a".to_owned(), "beta".to_owned()),
            ("b".to_owned(), "alpha".to_owned()),
        ]
    );
}

#[test]
fn test_usage_versions_breaks_ties_by_version() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(download_of("a", "flask", "2.0", None, 10));
    metrics.record(download_of("a", "flask", "1.0", None, 10));
    settle_and_assert(&metrics, || metrics.daily_usage().len() == 2);
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics
            .usage_versions(None, &interval)
            .into_iter()
            .map(|row| row.version)
            .collect::<Vec<_>>(),
        [Some("1.0".to_owned()), Some("2.0".to_owned())]
    );
}

#[test]
fn test_usage_sources_breaks_ties_by_source() {
    let (_dir, _meta, metrics) = durable_on(500, None);
    metrics.record(download_of("a", "flask", "1.0", Some("beta"), 10));
    metrics.record(download_of("a", "flask", "1.0", Some("alpha"), 10));
    settle_and_assert(&metrics, || metrics.daily_usage().len() == 2);
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
fn test_usage_unused_breaks_ties_by_repository_then_project() {
    let (_dir, meta) = store();
    let past = Metrics::start_durable(meta.analytics(), None, clock_on_day(100));
    for (route, project) in [("a", "alpha"), ("a", "beta"), ("b", "alpha")] {
        past.record(download(route, project, "file.bin", 5));
    }
    settle_and_assert(&past, || persisted_downloads(&meta.analytics()) == Some(3));
    drop(past);

    let metrics = Metrics::start_durable(meta.analytics(), None, clock_on_day(500));
    let interval = metrics.resolve_usage_interval(None, None);

    assert_eq!(
        metrics
            .usage_unused(None, &interval)
            .into_iter()
            .map(|row| (row.repository, row.project, row.lifetime_downloads))
            .collect::<Vec<_>>(),
        [
            ("a".to_owned(), "alpha".to_owned(), 1),
            ("a".to_owned(), "beta".to_owned(), 1),
            ("b".to_owned(), "alpha".to_owned(), 1),
        ]
    );
}
