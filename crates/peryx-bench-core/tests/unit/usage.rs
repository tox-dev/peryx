use std::time::Duration;

use super::Usage;

#[test]
fn usage_skips_absent_process() {
    assert_eq!(Usage::watch(None).finish(), None);
}

#[test]
fn usage_samples_process_tree() {
    let usage = Usage::watch(Some(std::process::id()));
    std::thread::sleep(Duration::from_millis(250));
    let cost = usage.finish().expect("the current process is sampled");
    assert!(cost.peak_rss_bytes > 0);
}
