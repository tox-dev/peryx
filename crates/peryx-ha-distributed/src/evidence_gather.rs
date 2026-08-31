use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};

use crate::dc_ack::Deadline;

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

pub async fn gather<Source, Context, Evidence, Request, Observe>(
    sources: Vec<&Source>,
    context: &Context,
    budget: Duration,
    poll: Duration,
    request: Request,
    mut observe: Observe,
) -> Deadline
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
                Observation::Durable => return,
                Observation::Complete => {}
                Observation::Pending => requests.push(Box::pin(async move {
                    tokio::time::sleep(poll).await;
                    (index, request(sources[index], context).await)
                })),
            }
        }
        std::future::pending::<()>().await;
    };
    match tokio::time::timeout(budget, gather).await {
        Ok(()) => Deadline::Live,
        Err(_) => Deadline::Expired,
    }
}
