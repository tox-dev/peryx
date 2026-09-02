use std::sync::Mutex;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
pub use peryx_ha::SourceFailure;

use crate::backoff::{ReconnectPolicy, Retry, jitter};
use crate::dc_ack::Deadline;
use crate::peer::TransportError;

pub enum Observation {
    Pending,
    Complete,
    Durable,
}

/// What one query to a source produced. [`Attempt::Retire`] drops the source for the rest of this
/// gather, so a peer that broke the protocol is not asked again on the same write.
///
/// [`Attempt::Absent`] and [`Attempt::Failed`] are both "nothing yet" and are paced differently. A
/// source with no report to give is healthy and cheap to ask again, so it keeps the poll cadence. A
/// source that failed is asked on backoff and eventually retired, since asking a peer answering 503
/// every 50 ms for a whole write budget neither helps the write nor the peer.
pub enum Attempt<Evidence> {
    Found(Evidence),
    /// The source answered and holds nothing yet.
    Absent,
    /// The source failed in a way worth retrying.
    Failed(TransportError),
    Retire,
}

/// Widest spread added to a durability retry, matching the replication loops, so peers that failed
/// together do not come back together.
pub const DEFAULT_GATHER_JITTER: Duration = Duration::from_millis(100);

/// How a gather paces asking a source that has produced no evidence yet.
pub struct GatherSchedule<'a> {
    /// Cadence for a source that answered and holds nothing yet.
    pub poll: Duration,
    /// Backoff and attempt limit for a source that keeps failing.
    pub policy: ReconnectPolicy,
    /// Widest spread added to a backoff delay, derived from source identity.
    pub jitter: Duration,
    /// Where a source that reaches the attempt limit is named.
    pub retired: &'a RetiredSources,
}

/// How a gather stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatherEnd {
    /// Evidence proved the operation durable.
    Durable,
    /// Every source gave all it can, whether that was a final report, a terminal retirement, or an
    /// attempt limit spent on failures. Waiting longer cannot produce evidence that no longer has a
    /// source to come from.
    Exhausted,
    /// The budget ran out with sources still to hear from.
    TimedOut,
}

/// What a collector concluded, and every source it retired on the way.
///
/// A budget that ran out and a source set with nothing left to give both report
/// [`Deadline::Expired`](crate::Deadline::Expired), because a peer may commit after the client stops
/// waiting either way. `timed_out` and `retired` keep them apart, so an unproven write reports what
/// stopped it rather than only that it stopped.
#[derive(Debug, PartialEq, Eq)]
pub struct GatherOutcome {
    pub deadline: Deadline,
    pub timed_out: bool,
    pub retired: Vec<SourceFailure>,
}

/// Collects [`SourceFailure`]s while a collector's request closure runs on several sources at once.
#[derive(Debug, Default)]
pub struct RetiredSources(Mutex<Vec<SourceFailure>>);

impl RetiredSources {
    /// Retires `source` when `error` is terminal, and reports whether it did.
    pub fn record(&self, source: &str, error: &TransportError) -> bool {
        let Some(failure) = SourceFailure::terminal(source, error) else {
            return false;
        };
        self.push(failure);
        true
    }

    /// Retires `source` for a reason the transport error alone does not carry, such as spending its
    /// attempt limit on failures that were each worth one more try.
    pub fn give_up(&self, source: &str, reason: &'static str) {
        self.push(SourceFailure {
            source: source.to_owned(),
            reason,
        });
    }

    fn push(&self, failure: SourceFailure) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(failure);
    }

    fn take(self) -> Vec<SourceFailure> {
        let mut retired = self.0.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
        retired.sort_by(|left, right| left.source.cmp(&right.source));
        retired
    }
}

/// Pairs `end` with the sources `retired` collected, ordered by source name.
#[must_use]
pub fn outcome(end: GatherEnd, retired: RetiredSources) -> GatherOutcome {
    GatherOutcome {
        deadline: if end == GatherEnd::Durable {
            Deadline::Live
        } else {
            Deadline::Expired
        },
        timed_out: end == GatherEnd::TimedOut,
        retired: retired.take(),
    }
}

/// Asks every source until one proves the operation durable, every source has given all it holds, or
/// `budget` runs out. `sources` pairs each source with the name a retirement reports it under.
///
/// Retry state is per source and lives for this call alone, so a later gather asks a source that spent
/// its attempts here as if it were healthy.
pub async fn gather<Source, Context, Evidence, Request, Observe>(
    sources: Vec<(&str, &Source)>,
    context: &Context,
    budget: Duration,
    schedule: &GatherSchedule<'_>,
    request: Request,
    mut observe: Observe,
) -> GatherEnd
where
    Source: ?Sized + Send + Sync,
    Context: ?Sized + Sync,
    Evidence: Send,
    Request: for<'a> Fn(&'a Source, &'a Context) -> BoxFuture<'a, Attempt<Evidence>> + Send + Sync,
    Observe: FnMut(Evidence) -> Observation + Send,
{
    let started = tokio::time::Instant::now();
    let gather = async {
        let request = &request;
        let sources = &sources;
        let mut attempts = vec![0_u32; sources.len()];
        let mut requests: FuturesUnordered<BoxFuture<'_, (usize, Attempt<Evidence>)>> = sources
            .iter()
            .enumerate()
            .map(|(index, (_, source))| Box::pin(async move { (index, request(source, context).await) }) as _)
            .collect();
        while let Some((index, attempt)) = requests.next().await {
            let delay = match attempt {
                Attempt::Found(evidence) => match observe(evidence) {
                    Observation::Durable => return GatherEnd::Durable,
                    Observation::Complete => continue,
                    Observation::Pending => schedule.poll,
                },
                Attempt::Absent => schedule.poll,
                Attempt::Failed(error) => {
                    attempts[index] = attempts[index].saturating_add(1);
                    let attempt = attempts[index];
                    let source = sources[index].0;
                    match schedule.policy.on_error(&error, attempt) {
                        Retry::After(base) => backoff_delay(base, error.retry_after(), source, attempt, schedule),
                        Retry::GiveUp { reason } => {
                            schedule.retired.give_up(source, reason);
                            continue;
                        }
                    }
                }
                Attempt::Retire => continue,
            };
            // A source that cannot answer inside what is left of the budget is parked rather than asked
            // again: clamping the delay instead would spend its remaining attempts in one instant, and
            // the caller asked to hear about the budget running out, not about a source giving up.
            if delay >= budget.saturating_sub(started.elapsed()) {
                requests.push(Box::pin(std::future::pending()));
                continue;
            }
            requests.push(Box::pin(async move {
                tokio::time::sleep(delay).await;
                (index, request(sources[index].1, context).await)
            }));
        }
        // Every source has been retired or has given all it holds, so waiting out the budget only
        // delays the answer the caller already has.
        GatherEnd::Exhausted
    };
    tokio::time::timeout(budget, gather)
        .await
        .unwrap_or(GatherEnd::TimedOut)
}

/// Spreads `base` by identity-derived jitter, then honours a `Retry-After` the source asked for as a
/// floor. A server that names a delay knows more about its own load than the policy does, so the
/// policy only ever waits longer than it asked, never less.
fn backoff_delay(
    base: Duration,
    retry_after: Option<Duration>,
    source: &str,
    attempt: u32,
    schedule: &GatherSchedule<'_>,
) -> Duration {
    let spread = base.saturating_add(jitter(source, attempt, schedule.jitter));
    retry_after.map_or(spread, |asked| spread.max(asked))
}
