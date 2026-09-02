use peryx_index::serving::{FlightGuard, Inflight, flight_gate};

/// Hold the sync gate for `key`, waiting for whoever holds it.
///
/// The guard reports nothing about the sync that ran ahead of it. Coalescing an answer out of it would
/// mean reporting a generation the caller never revalidated, so each caller makes its own conditional
/// request and reports what that request returned.
pub async fn acquire(inflight: &Inflight, key: &str) -> FlightGuard {
    flight_gate(inflight, key).lock_owned().await
}
