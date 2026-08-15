use peryx_core::{AnalyticsSnapshotStore, AvailabilityReadError};

impl AnalyticsSnapshotStore for crate::meta::MetaStore {
    fn load_analytics_snapshot(&self) -> Result<Option<Vec<u8>>, AvailabilityReadError> {
        self.analytics()
            .load_apply()
            .map_err(|error| AvailabilityReadError::new(error.to_string()))
    }
}

#[cfg(test)]
#[path = "../tests/unit/availability/tests.rs"]
mod tests;
