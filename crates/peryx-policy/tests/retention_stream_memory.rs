//! A retention plan must hold one expanded decision, not the whole resource's expansion.
//!
//! Every removal repeats the surviving groups, so what a resource serializes to grows with removals
//! times survivors. What the planner holds live must not. The allocator is global, so this binary runs
//! one test.

use std::alloc::System;

use peryx_policy::{
    RetentionCandidate, RetentionClass, RetentionConfig, RetentionPolicy, RetentionSelector, RetentionVisibility,
};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

/// Groups in the resource and how many the policy keeps. The rest are removals, each repeating the
/// whole surviving set: a million owned strings, tens of megabytes, expanded at once.
const GROUPS: usize = 2_000;
const RETAINED: usize = 1_000;
/// Sorting, classifying and indexing two thousand candidates. The expansion this replaces owns a
/// thousand thousand-entry vectors, some seventy megabytes; a tenth of that would fail here.
const MAX_PLANNING_BYTES: usize = 4 << 20;
/// Candidates, verdicts, the surviving-group index and one expanded decision, with room to spare.
const MAX_RESIDENT_BYTES: isize = 4 << 20;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn test_streaming_a_plan_expands_one_decision_at_a_time() {
    let policy = keep_latest();
    let baseline = resident();
    let mut peak = baseline;
    let mut streamed = 0_usize;

    let planning = Region::new(ALLOCATOR);
    let plan = policy.plan_resource(None, candidates());
    let planned = planning.change().bytes_allocated;

    for decision in plan.decisions() {
        peak = peak.max(resident());
        streamed += decision.retained_groups.len();
    }

    let held = peak - baseline;
    assert_eq!(streamed, RETAINED * (GROUPS - RETAINED));
    assert!(planned < MAX_PLANNING_BYTES, "planning allocated {planned} bytes");
    assert!(held < MAX_RESIDENT_BYTES, "streaming held {held} bytes");
}

fn resident() -> isize {
    let stats = ALLOCATOR.stats();
    isize::try_from(stats.bytes_allocated).unwrap() - isize::try_from(stats.bytes_deallocated).unwrap()
}

fn keep_latest() -> RetentionPolicy {
    RetentionPolicy::compile(
        &RetentionConfig {
            keep: vec![RetentionSelector::KeepLatestGroups { count: RETAINED as u64 }],
            expire: vec![RetentionSelector::ResourcePrefix { prefix: String::new() }],
        },
        str::to_owned,
    )
}

/// One candidate per group, ranked newest first, so the policy keeps the first [`RETAINED`] of them.
fn candidates() -> Vec<RetentionCandidate> {
    (0..GROUPS)
        .map(|rank| RetentionCandidate {
            resource: "resource".to_owned(),
            group: Some(format!("{rank:04}.0.0-rc.1+build.20260830.commit.0123456789ab")),
            artifact: format!("resource-{rank:04}.whl"),
            digest: format!("sha256:{rank:064}"),
            class: RetentionClass::Hosted,
            visibility: RetentionVisibility::Active,
            source: None,
            bytes: 1024,
            upload_time_unix: None,
            rank: rank as u64,
            orphan: false,
        })
        .collect()
}
