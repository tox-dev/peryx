use std::sync::Mutex;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
pub use peryx_ha::SourceFailure;

use crate::dc_ack::Deadline;
use crate::peer::TransportError;

pub enum Observation {
    Pending,
    Complete,
    Durable,
}

/// What one query to a source produced. [`Attempt::Retire`] drops the source for the rest of this
/// gather, so a peer that broke the protocol is not asked again on the same write.
pub enum Attempt<Evidence> {
    Found(Evidence),
    Retry,
    Retire,
}

/// How a gather stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatherEnd {
    /// Evidence proved the operation durable.
    Durable,
    /// Every source gave all it can, whether that was a final report or a retirement. Waiting longer
    /// cannot produce evidence that no longer has a source to come from.
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
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(failure);
        true
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

pub async fn gather<Source, Context, Evidence, Request, Observe>(
    sources: Vec<&Source>,
    context: &Context,
    budget: Duration,
    poll: Duration,
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
    let gather = async {
        let request = &request;
        let sources = &sources;
        let mut requests: FuturesUnordered<BoxFuture<'_, (usize, Attempt<Evidence>)>> = sources
            .iter()
            .enumerate()
            .map(|(index, source)| Box::pin(async move { (index, request(source, context).await) }) as _)
            .collect();
        while let Some((index, attempt)) = requests.next().await {
            let observation = match attempt {
                Attempt::Found(evidence) => observe(evidence),
                Attempt::Retry => Observation::Pending,
                Attempt::Retire => continue,
            };
            match observation {
                Observation::Durable => return GatherEnd::Durable,
                Observation::Complete => {}
                Observation::Pending => requests.push(Box::pin(async move {
                    tokio::time::sleep(poll).await;
                    (index, request(sources[index], context).await)
                })),
            }
        }
        // Every source has been retired or has given all it holds, so waiting out the budget only
        // delays the answer the caller already has.
        GatherEnd::Exhausted
    };
    tokio::time::timeout(budget, gather)
        .await
        .unwrap_or(GatherEnd::TimedOut)
}
