//! What a home failover and its retained-write drain cost on the compute path.
//!
//! Failover's design claim is that moving a dead home's authority is a decision, not a scan: selecting
//! the target weighs a bounded candidate set, and draining the old home's retained intents plans a
//! bounded batch, so neither stalls on a long roster or an unbounded backlog. These legs measure that
//! claim on the two pure hot paths #539 adds — the failover selection and the drain plan — reporting
//! latency percentiles alongside the allocation count and retained bytes each leg costs, and the drain's
//! throughput in intents per second.
//!
//! The system-level acceptance numbers a live cluster reports — end-to-end failover RTO wall-clock, the
//! drained ledger's disk footprint, and an unaffected datacenter's serving p99 during a failover — belong
//! to the comparative `peryx-bench` harness against running nodes, not this microbenchmark.
//!
//! The CI performance runner does not build this package's benches, so this is a local
//! `cargo bench -p peryx-ha-distributed` tool; it never gates CI.

use std::alloc::System;
use std::hint::black_box;
use std::num::NonZeroUsize;
use std::time::Instant;

use hdrhistogram::Histogram;
use peryx_ha_distributed::{Candidate, DatacenterId, DrainIntent, FailoverPolicy, OldEpochOp, Suspicion, plan_drain};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, Stats, StatsAlloc};

const SAMPLES: usize = 20_000;
const ROSTER: usize = 32;
const DRAIN_BATCH: usize = 128;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

fn main() {
    report_failover();
    report_drain();
}

/// The failover decision over a dead home and a full roster whose first live candidate sits last, the
/// worst case the bounded scan walks.
fn report_failover() {
    let policy = FailoverPolicy::new(NonZeroUsize::new(ROSTER).unwrap());
    let candidates = worst_case_roster();
    let region = Region::new(ALLOCATOR);
    for _ in 0..SAMPLES {
        black_box(policy.select(Suspicion::Dead, black_box(&candidates)));
    }
    let alloc = region.change();
    let latency = latency(|| {
        black_box(policy.select(Suspicion::Dead, black_box(&candidates)));
    });
    println!(
        "leg=failover_select roster={ROSTER} allocations={} retained_bytes={} p50_ns={} p99_ns={}",
        per_sample(alloc.allocations),
        retained_bytes(alloc),
        latency.value_at_quantile(0.5),
        latency.value_at_quantile(0.99),
    );
}

/// The drain plan over a full batch of retained intents, mostly replayable with a scatter of settled
/// ones, so the plan exercises both the finalize and the retire paths.
fn report_drain() {
    let intents = drain_batch();
    let region = Region::new(ALLOCATOR);
    for _ in 0..SAMPLES {
        black_box(plan_drain(black_box(intents.clone())));
    }
    let alloc = region.change();
    let latency = latency(|| {
        black_box(plan_drain(black_box(intents.clone())));
    });
    let p50_ns = latency.value_at_quantile(0.5);
    let throughput = (DRAIN_BATCH as u64 * 1_000_000_000).checked_div(p50_ns).unwrap_or(0);
    println!(
        "leg=drain_plan batch={DRAIN_BATCH} allocations={} retained_bytes={} p50_ns={p50_ns} p99_ns={} intents_per_sec={throughput}",
        per_sample(alloc.allocations),
        retained_bytes(alloc),
        latency.value_at_quantile(0.99),
    );
}

fn worst_case_roster() -> Vec<Candidate> {
    (0..ROSTER)
        .map(|slot| Candidate {
            datacenter: DatacenterId(format!("dc-{slot:02}")),
            // Only the last candidate is alive, so the bounded scan walks the whole roster before it takes one.
            suspicion: if slot + 1 == ROSTER {
                Suspicion::Alive
            } else {
                Suspicion::Dead
            },
        })
        .collect()
}

fn drain_batch() -> Vec<DrainIntent> {
    (0..DRAIN_BATCH)
        .map(|slot| DrainIntent {
            key: format!("corp\u{0}flask\u{0}k{slot:04}"),
            op: OldEpochOp {
                durably_committed: true,
                // Every eighth intent is already applied, so the plan exercises the retire path too.
                already_applied: slot.is_multiple_of(8),
                superseded: false,
            },
        })
        .collect()
}

const fn per_sample(total: usize) -> usize {
    total / SAMPLES
}

fn latency(mut operation: impl FnMut()) -> Histogram<u64> {
    let mut histogram = Histogram::new(3).unwrap();
    for _ in 0..SAMPLES {
        let start = Instant::now();
        operation();
        histogram
            .record(u64::try_from(start.elapsed().as_nanos()).unwrap())
            .unwrap();
    }
    histogram
}

fn retained_bytes(stats: Stats) -> isize {
    isize::try_from(stats.bytes_allocated).unwrap() - isize::try_from(stats.bytes_deallocated).unwrap()
        + stats.bytes_reallocated
}
