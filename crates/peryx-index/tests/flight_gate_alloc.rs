use std::alloc::System;

use peryx_index::serving::{Inflight, flight_gate};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn test_dropped_flights_release_their_registrations() {
    let inflight = Inflight::default();
    let region = Region::new(ALLOCATOR);
    let registrations = 1_024;

    for index in 0..registrations {
        drop(flight_gate(&inflight, &index.to_string()));
    }

    let deallocations_before_inflight_drop = region.change().deallocations;
    drop(inflight);
    assert!(region.change().deallocations - deallocations_before_inflight_drop < 2 * registrations);
}
