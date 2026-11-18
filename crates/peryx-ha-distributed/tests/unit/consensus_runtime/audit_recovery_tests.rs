use super::*;

use openraft::alias::WatchOf;
use openraft::async_runtime::Watch;
use tokio::sync::mpsc;

use crate::raft::RaftMetrics;
use tokio::time::Instant;

const LOCAL: VoterId = 7;

#[tokio::test(start_paused = true)]
async fn test_a_failed_audit_recovery_retries_after_the_retry_delay() {
    let mut leading = RaftMetrics::new_initial(LOCAL);
    leading.current_leader = Some(LOCAL);
    let (_leadership, metrics) = <WatchOf<TypeConfig> as Watch>::channel(leading);
    let (attempts, mut attempted_at) = mpsc::unbounded_channel();
    let mut calls = 0;
    let watcher = tokio::spawn(watch_transfer_audits_on_leadership(LOCAL, metrics, move || {
        calls += 1;
        attempts.send(Instant::now()).unwrap();
        std::future::ready(if calls == 1 { Err("store unavailable") } else { Ok(()) })
    }));

    let first = attempted_at.recv().await.unwrap();
    let second = attempted_at.recv().await.unwrap();

    watcher.abort();
    assert_eq!(second - first, AUDIT_RECOVERY_RETRY_DELAY);
}
