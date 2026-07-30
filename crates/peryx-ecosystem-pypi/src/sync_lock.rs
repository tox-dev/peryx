use peryx_index::serving::{FlightGuard, Inflight, flight_gate};

pub async fn acquire(inflight: &Inflight, key: &str) -> (FlightGuard, bool) {
    match flight_gate(inflight, key).try_lock_owned() {
        Ok(guard) => (guard, false),
        Err(_) => (flight_gate(inflight, key).lock_owned().await, true),
    }
}
