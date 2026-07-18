#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

//! How retention evaluation scales over a large index.
//!
//! The design claim is that a plan costs no more than sorting each project's files: the planner groups
//! one project at a time, sorts its candidates, and classifies each in one pass, so a million-record
//! index stays linearithmic rather than quadratic. This leg feeds the planner a generated repository of
//! one million artifact records — fifty thousand projects of twenty versions each — and measures one
//! full evaluation.
//!
//! The CI performance runner builds this leg; it establishes a baseline rather than gating a threshold.

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use peryx_policy::{
    RetentionCandidate, RetentionClass, RetentionConfig, RetentionOutcome, RetentionPolicy, RetentionSelector,
    RetentionVisibility,
};

const RECORDS: u64 = 1_000_000;
const VERSIONS_PER_PROJECT: u64 = 20;

fn candidate(project: u64, version: u64) -> RetentionCandidate {
    RetentionCandidate {
        project: format!("project-{project}"),
        version: Some(format!("{version}.0")),
        artifact: format!("project-{project}-{version}.0.whl"),
        digest: format!("sha256:{project}-{version}"),
        class: if version.is_multiple_of(7) {
            RetentionClass::Trash
        } else {
            RetentionClass::Hosted
        },
        visibility: RetentionVisibility::Active,
        source: None,
        bytes: 4096,
        upload_time_unix: Some(version.cast_signed()),
        // Ascending rank against descending version, so the planner always sorts.
        rank: VERSIONS_PER_PROJECT - version - 1,
        orphan: false,
    }
}

fn repository() -> Vec<Vec<RetentionCandidate>> {
    (0..RECORDS / VERSIONS_PER_PROJECT)
        .map(|project| {
            (0..VERSIONS_PER_PROJECT)
                .map(|version| candidate(project, version))
                .collect()
        })
        .collect()
}

fn policy() -> RetentionPolicy {
    RetentionPolicy::compile(&RetentionConfig {
        keep: vec![RetentionSelector::KeepLatest { count: 5 }],
        expire: vec![
            RetentionSelector::Trash,
            RetentionSelector::Age { older_than_seconds: 10 },
        ],
    })
}

fn bench_plan(criterion: &mut Criterion) {
    let repository = repository();
    let policy = policy();
    let mut group = criterion.benchmark_group("retention");
    group.throughput(Throughput::Elements(RECORDS));
    group.bench_function("plan_one_million_records", |bencher| {
        bencher.iter(|| {
            let mut removed = 0_u64;
            for project in &repository {
                for decision in policy.plan_project(Some(1_000), black_box(project.clone())) {
                    removed += u64::from(decision.outcome == RetentionOutcome::Remove);
                }
            }
            black_box(removed)
        });
    });
    group.finish();
}

criterion_group!(benches, bench_plan);
criterion_main!(benches);
