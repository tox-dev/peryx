use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use peryx_driver::state::ServingState;
use tokio::task::JoinHandle;

fn pending() -> &'static Mutex<HashMap<usize, Vec<JoinHandle<()>>>> {
    static PENDING: OnceLock<Mutex<HashMap<usize, Vec<JoinHandle<()>>>>> = OnceLock::new();
    PENDING.get_or_init(Mutex::default)
}

/// The serving state's identity, stable across the `Arc` clones the router and driver hand around,
/// so a captured handle files under the bucket the owning test drains.
fn bucket(state: &Arc<ServingState>) -> usize {
    Arc::as_ptr(state) as usize
}

pub(super) fn capture(state: &Arc<ServingState>, refresh: JoinHandle<()>) {
    pending()
        .lock()
        .expect("revalidation probe")
        .entry(bucket(state))
        .or_default()
        .push(refresh);
}

pub(super) fn drain(state: &Arc<ServingState>) -> Vec<JoinHandle<()>> {
    pending()
        .lock()
        .expect("revalidation probe")
        .remove(&bucket(state))
        .unwrap_or_default()
}
