use peryx_ha::OperationKind;

use crate::state::ServingState;

impl ServingState {
    pub fn record_operation_trace(&self, kind: OperationKind, fence: u64) {
        self.availability.record_operation_trace(&self.meta, kind, fence);
    }
}

#[cfg(test)]
#[path = "../../tests/unit/state/traces/tests.rs"]
mod tests;
