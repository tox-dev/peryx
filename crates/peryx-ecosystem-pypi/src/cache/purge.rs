use peryx_driver::serving::PurgeReport;
use peryx_driver::state::ServingState;

use super::{flight_gate, release_flight};

/// Remove `resource` from cached repository `index` while the server keeps serving it.
///
/// The deletion runs as the holder of the project's flight, the same gate every page writer joins
/// before it reaches upstream. A refresh already fetching this project therefore finishes and
/// publishes first, and the purge then removes what it published; one that arrives while the purge
/// holds the gate re-reads the row afterwards and finds nothing to revalidate, rather than
/// republishing the page it was about to store. Operator intent wins either way, and because the
/// count runs under the same guard the reported numbers are the rows that actually went.
///
/// A dry run reports without deleting and takes the gate all the same, so its counts describe a
/// settled cache rather than one mid-write.
///
/// # Errors
/// Returns a message when a cached page cannot be read or the store cannot be written.
pub async fn purge_served_project(
    state: &ServingState,
    index: &str,
    resource: &str,
    apply: bool,
) -> Result<PurgeReport, String> {
    let normalized = crate::normalize_name(resource);
    let key = format!("{index}/{normalized}");
    let gate = flight_gate(state, &key);
    let guard = gate.lock_owned().await;
    let report = crate::admin::purge_project(&state.meta, index, &normalized, apply);
    // Rendered representations outlive the rows they were built from. The offline purge gets that for
    // free from the restart that follows it; a live one has to retire them itself.
    if apply && report.is_ok() {
        super::invalidate_project(state, index, &normalized);
    }
    release_flight(state, &key, guard);
    report
}
