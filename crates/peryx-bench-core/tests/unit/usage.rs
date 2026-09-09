use std::sync::mpsc::sync_channel;
use std::time::Duration;

use anyhow::bail;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use super::{Cost, Usage, cpu_millis, tree_of};

#[test]
fn usage_skips_absent_process() {
    assert_eq!(Usage::watch(None).unwrap().finish().unwrap(), None);
}

#[test]
fn usage_reports_initial_failure() {
    assert!(
        Usage::watch(Some(u32::MAX))
            .err()
            .unwrap()
            .to_string()
            .starts_with("initial resource sample failed: process ")
    );
}

#[test]
fn usage_reports_terminal_sampling_failure() {
    let (sample_started, wait_for_sample) = sync_channel(0);
    let mut initial = true;
    let usage = Usage::watch_with(
        Duration::ZERO,
        Box::new(move || {
            if std::mem::take(&mut initial) {
                return Ok((1, 1));
            }
            sample_started.send(()).unwrap();
            bail!("later sample failed");
        }),
    )
    .unwrap();
    wait_for_sample.recv().unwrap();
    assert_eq!(
        usage.finish().unwrap_err().to_string(),
        "resource sampling failed: later sample failed"
    );
}

#[test]
fn usage_reports_sampler_thread_panic() {
    assert_eq!(
        Usage::watch_with(
            Duration::ZERO,
            Box::new(|| -> anyhow::Result<(u64, u64)> {
                panic!("sampler panic");
            })
        )
        .err()
        .unwrap()
        .to_string(),
        "initial resource sample failed: resource sampler thread panicked"
    );
}

#[test]
fn usage_samples_process_tree() {
    let cost = Usage::watch(Some(std::process::id()))
        .unwrap()
        .finish()
        .unwrap()
        .expect("the current process is sampled");
    assert!(cost.peak_rss_bytes > 0);
}

#[test]
fn finish_reports_the_sampled_cost_in_seconds() {
    let usage =
        Usage::watch_with(Duration::from_hours(1), Box::new(|| Ok((4096, 2500)))).expect("the initial sample succeeds");
    assert_eq!(
        usage.finish().expect("the sampler stops"),
        Some(Cost {
            cpu_seconds: 2.5,
            peak_rss_bytes: 4096,
        })
    );
}

#[test]
fn cpu_percent_converts_to_milliseconds_of_a_tick() {
    assert_eq!((cpu_millis(50.0), cpu_millis(0.0)), (100, 0));
}

#[test]
fn the_tree_holds_the_root_and_its_descendants_only() {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    let root = Pid::from_u32(std::process::id());
    let tree = tree_of(&system, root);
    assert_eq!((tree.contains(&root), tree.contains(&Pid::from_u32(1))), (true, false));
}
