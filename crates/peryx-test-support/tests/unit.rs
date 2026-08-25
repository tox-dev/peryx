#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::time::Duration;

use crate::startup_log;
#[cfg(unix)]
use crate::{StartupSignal, wait_for_startup};

#[test]
fn startup_log_keeps_the_failure_and_backtrace_tail() {
    let lines: Vec<_> = (0..80).map(|index| format!("line {index}")).collect();
    let excerpt = startup_log(&lines.join("\n"));
    assert!(excerpt.starts_with("line 0\n"));
    assert!(excerpt.contains("... 20 lines omitted ..."));
    assert!(excerpt.ends_with("line 79"));
    assert_eq!(startup_log("short\nlog"), "short\nlog");
}

#[test]
#[cfg(unix)]
fn node_timeout_reports_a_reaped_child_while_the_event_channel_is_open() {
    let mut child = Command::new("true").spawn().expect("start child");
    child.wait().expect("reap child");
    let (event_sender, process_events) = mpsc::channel();
    let signal =
        wait_for_startup(&mut child, &process_events, Duration::ZERO, |_| false).expect("classify startup timeout");
    drop(event_sender);
    assert!(matches!(signal, StartupSignal::Exited(_)));
}
