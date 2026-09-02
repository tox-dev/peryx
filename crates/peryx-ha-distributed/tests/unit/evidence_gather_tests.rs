use std::num::NonZeroU32;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::future::BoxFuture;

use crate::backoff::{RETRY_EXHAUSTED, ReconnectPolicy, jitter};
use crate::evidence_gather::{
    Attempt, GatherEnd, GatherSchedule, Observation, RetiredSources, SourceFailure, gather, outcome,
};
use crate::peer::TransportError;

const POLL: Duration = Duration::from_millis(50);
/// Long enough for a source to spend every attempt the policy allows.
const BUDGET: Duration = Duration::from_mins(2);
const SOURCE: &str = "replica-a";

/// Answers with whatever the test queued, then repeats its last answer, so a test says how a source
/// behaves rather than how many times a schedule will ask.
struct Scripted(Mutex<Vec<Reply>>);

#[derive(Clone)]
enum Reply {
    Absent,
    Failed(TransportError),
    Found,
}

impl Scripted {
    fn new(replies: Vec<Reply>) -> Self {
        Self(Mutex::new(replies))
    }

    fn next(&self) -> Reply {
        let mut replies = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if replies.len() > 1 {
            replies.remove(0)
        } else {
            replies[0].clone()
        }
    }
}

fn policy(max_attempts: u32) -> ReconnectPolicy {
    ReconnectPolicy::new(
        Duration::from_millis(100),
        NonZeroU32::new(2).unwrap(),
        Duration::from_secs(30),
        NonZeroU32::new(max_attempts).unwrap(),
    )
}

/// Runs one gather over a single scripted source and reports how it ended, how long it took on the
/// logical clock, and which sources it retired.
async fn run(
    replies: Vec<Reply>,
    budget: Duration,
    schedule_jitter: Duration,
    max_attempts: u32,
) -> (GatherEnd, Duration, Vec<SourceFailure>) {
    let source = Scripted::new(replies);
    let retired = RetiredSources::default();
    let schedule = GatherSchedule {
        poll: POLL,
        policy: policy(max_attempts),
        jitter: schedule_jitter,
        retired: &retired,
    };
    let started = tokio::time::Instant::now();
    let end = gather(
        vec![(SOURCE, &source)],
        &(),
        budget,
        &schedule,
        |source: &Scripted, ()| -> BoxFuture<'_, Attempt<()>> {
            Box::pin(async move {
                match source.next() {
                    Reply::Absent => Attempt::Absent,
                    Reply::Failed(error) => Attempt::Failed(error),
                    Reply::Found => Attempt::Found(()),
                }
            })
        },
        |()| Observation::Durable,
    )
    .await;
    (end, started.elapsed(), outcome(end, retired).retired)
}

fn failed() -> Reply {
    Reply::Failed(TransportError::Timeout)
}

/// The policy doubles from 100 ms, so nine retries before the tenth attempt gives up span
/// 100 + 200 + 400 + 800 + 1600 + 3200 + 6400 + 12800 + 25600 ms. Polling instead would have asked
/// roughly a thousand times in the same span.
#[tokio::test(start_paused = true)]
async fn test_a_failing_source_backs_off_and_is_retired_when_its_attempts_run_out() {
    let (end, elapsed, retired) = run(vec![failed()], BUDGET, Duration::ZERO, 10).await;

    assert_eq!(
        (end, elapsed, retired),
        (
            GatherEnd::Exhausted,
            Duration::from_millis(51_100),
            vec![SourceFailure {
                source: SOURCE.to_owned(),
                reason: RETRY_EXHAUSTED,
            }]
        )
    );
}

/// A source with nothing to report is healthy, so it keeps the cadence the caller chose rather than
/// being treated as a failure and slowed down.
#[tokio::test(start_paused = true)]
async fn test_an_absent_source_keeps_the_poll_cadence() {
    let replies = vec![Reply::Absent, Reply::Absent, Reply::Absent, Reply::Found];

    let (end, elapsed, retired) = run(replies, BUDGET, Duration::ZERO, 10).await;

    assert_eq!((end, elapsed, retired), (GatherEnd::Durable, POLL * 3, Vec::new()));
}

/// A server that names a delay knows its own load better than the schedule does, so the schedule waits
/// the longer of the two.
#[tokio::test(start_paused = true)]
async fn test_a_server_delay_raises_the_backoff_floor() {
    let asked = Duration::from_secs(5);
    let replies = vec![
        Reply::Failed(TransportError::ServerError {
            status: 503,
            retry_after: Some(asked),
        }),
        Reply::Found,
    ];

    let (end, elapsed, retired) = run(replies, BUDGET, Duration::ZERO, 10).await;

    assert_eq!((end, elapsed, retired), (GatherEnd::Durable, asked, Vec::new()));
}

/// A delay past the budget cannot produce evidence, so the gather spends what is left and reports the
/// timeout the caller asked for rather than sleeping through it.
#[tokio::test(start_paused = true)]
async fn test_no_retry_sleeps_past_the_budget() {
    let budget = Duration::from_secs(1);
    let replies = vec![Reply::Failed(TransportError::ServerError {
        status: 503,
        retry_after: Some(Duration::from_mins(1)),
    })];

    let (end, elapsed, retired) = run(replies, budget, Duration::ZERO, 10).await;

    assert_eq!((end, elapsed, retired), (GatherEnd::TimedOut, budget, Vec::new()));
}

/// Retry state belongs to one gather, so a source that spent its attempts proving a write is asked
/// again from the start on the next one rather than staying retired.
#[tokio::test(start_paused = true)]
async fn test_a_later_gather_starts_with_fresh_retry_state() {
    let first = run(vec![failed()], BUDGET, Duration::ZERO, 2).await;

    let second = run(vec![failed()], BUDGET, Duration::ZERO, 2).await;

    assert_eq!(first, second);
    assert_eq!(first.1, Duration::from_millis(100));
}

/// Tokio rounds a sleep deadline up to the next millisecond, so a jittered delay lands on the tick above
/// the value the schedule computed.
fn next_tick(delay: Duration) -> Duration {
    Duration::from_millis(u64::try_from(delay.as_nanos().div_ceil(1_000_000)).unwrap())
}

/// Jitter spreads sources that failed together, so a shared outage does not bring them all back at the
/// same instant.
#[tokio::test(start_paused = true)]
async fn test_backoff_is_spread_by_identity_derived_jitter() {
    let window = Duration::from_millis(100);
    let spread = jitter(SOURCE, 1, window);

    let (_, elapsed, _) = run(vec![failed(), Reply::Found], BUDGET, window, 10).await;

    assert_eq!(
        (elapsed, spread > Duration::ZERO),
        (next_tick(Duration::from_millis(100) + spread), true)
    );
}
