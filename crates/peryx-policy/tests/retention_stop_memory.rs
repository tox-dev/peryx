//! A consumer that stops early must not pay for the rows it never took.
//!
//! A paged preview asks for one decision out of a resource with thousands; expanding the rest costs
//! removals times survivors in owned strings. The allocator is global, so this binary runs one test.

use std::alloc::System;

use peryx_policy::{
    RetentionCandidate, RetentionClass, RetentionConfig, RetentionOutcome, RetentionPolicy, RetentionSelector,
    RetentionVisibility,
};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

/// Groups in the resource and how many the policy keeps, so the row after the kept ones is the first
/// removal and carries every survivor.
const GROUPS: usize = 2_000;
const RETAINED: usize = 1_000;
/// One survivor index, cloned once. All thousand removals would need a thousand times this.
const MAX_ROW_BYTES: usize = 256 << 10;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn test_taking_one_decision_expands_only_that_row() {
    let policy = keep_latest();
    let mut decisions = policy.plan_resource(None, candidates()).decisions();

    let row = Region::new(ALLOCATOR);
    let removal = decisions.nth(RETAINED).unwrap();
    let allocated = row.change().bytes_allocated;

    assert_eq!(removal.group, Some(group(RETAINED)));
    assert_eq!(removal.outcome, RetentionOutcome::Remove);
    assert_eq!(removal.retained_groups, (0..RETAINED).map(group).collect::<Vec<_>>());
    assert!(allocated < MAX_ROW_BYTES, "one row allocated {allocated} bytes");
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
            group: Some(group(rank)),
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

/// Fixed width, so the index the planner sorts reads in the rank order these candidates carry.
fn group(rank: usize) -> String {
    format!("{rank:04}.0.0-rc.1+build.20260830.commit.0123456789ab")
}
