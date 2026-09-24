use std::future::Future;
use std::sync::Arc;

use peryx_index::serving::{Inflight, Turn, flight_gate};

/// A sync result, marked by whether this caller's own request produced it.
///
/// Callers count and record only what they led, so a publication counts at most once: for its leader, when
/// the leader counts.
#[derive(Debug)]
pub enum Synced<T> {
    Led(T),
    Joined(T),
}

impl<T> Synced<T> {
    pub fn into_inner(self) -> T {
        match self {
            Self::Led(result) | Self::Joined(result) => result,
        }
    }
}

/// Run `sync` as the flight for `key`, or answer with the flight that completed while this caller waited.
///
/// Concurrent callers for one key make one upstream request, and every waiter receives the leader's
/// result, failure included. A leader dropped before it finishes publishes nothing, so the next waiter
/// leads its own request.
pub async fn coalesce<T, E>(
    inflight: &Inflight,
    key: &str,
    sync: impl Future<Output = Result<T, E>>,
) -> Synced<Result<T, Arc<E>>>
where
    T: Clone + Send + Sync + 'static,
    E: Send + Sync + 'static,
{
    match flight_gate(inflight, key).lock_or_join().await {
        Turn::Joined(result) => Synced::Joined(result),
        Turn::Lead(guard) => {
            let result = sync.await.map_err(Arc::new);
            guard.complete(result.clone());
            Synced::Led(result)
        }
    }
}
