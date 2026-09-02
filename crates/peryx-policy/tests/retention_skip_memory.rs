//! Advancing a plan to a deep page must not expand the rows it passes over.
//!
//! Every removal repeats the surviving groups, so reaching a page by taking and discarding the rows
//! before it costs survivors times rows passed. Skipping drops the candidate and its verdict instead.
//! This measures both routes to the same row. The allocator is global, so this binary runs one test.

use std::alloc::System;

use peryx_policy::{
    RetentionCandidate, RetentionClass, RetentionConfig, RetentionOutcome, RetentionPolicy, RetentionSelector,
    RetentionVisibility,
};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

/// Groups in the resource and how many the policy keeps, so every row past [`RETAINED`] is a removal
/// carrying the whole surviving set.
const GROUPS: usize = 2_000;
const RETAINED: usize = 1_000;
/// A page deep enough that nine hundred removals stand before it.
const SKIP: usize = 1_900;
/// Nine hundred removals, each cloning a thousand owned groups, run to tens of megabytes. A tenth of
/// that still separates the two routes by two orders of magnitude.
const MIN_EXPANDED_BYTES: usize = 8 << 20;
/// The one row the page keeps clones the surviving index once; dropping the rows before it allocates
/// nothing at all.
const MAX_SKIPPED_BYTES: usize = 256 << 10;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn test_skipping_to_a_deep_page_costs_a_fraction_of_expanding_to_it() {
    let policy = keep_latest();

    let plan = policy.plan_resource(None, candidates());
    let expanding = Region::new(ALLOCATOR);
    let taken = plan.decisions().nth(SKIP).unwrap();
    let expanded = expanding.change().bytes_allocated;

    let mut plan = policy.plan_resource(None, candidates());
    let skipping = Region::new(ALLOCATOR);
    let dropped = plan.skip(SKIP as u64);
    let reached = plan.decisions().next().unwrap();
    let skipped = skipping.change().bytes_allocated;

    assert_eq!(dropped, SKIP as u64);
    assert_eq!(reached, taken);
    assert_eq!(reached.outcome, RetentionOutcome::Remove);
    assert_eq!(reached.retained_groups.len(), RETAINED);
    assert!(expanded > MIN_EXPANDED_BYTES, "expanding allocated {expanded} bytes");
    assert!(skipped < MAX_SKIPPED_BYTES, "skipping allocated {skipped} bytes");
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
