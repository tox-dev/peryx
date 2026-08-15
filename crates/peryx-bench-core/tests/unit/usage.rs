use std::sync::mpsc::channel;
use std::time::Duration;

use anyhow::bail;

use super::Usage;

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
    let (release, released) = channel();
    let mut initial = true;
    let usage = Usage::watch_with(Duration::ZERO, move || {
        if std::mem::take(&mut initial) {
            return Ok((1, 1));
        }
        released.recv().unwrap();
        bail!("later sample failed");
    })
    .unwrap();
    release.send(()).unwrap();
    assert_eq!(
        usage.finish().unwrap_err().to_string(),
        "resource sampling failed: later sample failed"
    );
}

#[test]
fn usage_reports_sampler_thread_panic() {
    assert_eq!(
        Usage::watch_with(Duration::ZERO, || -> anyhow::Result<(u64, u64)> {
            panic!("sampler panic");
        })
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
