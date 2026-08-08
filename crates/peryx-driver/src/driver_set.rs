//! A standalone registry of ecosystem drivers, for the composition root's build and admin paths.

use std::collections::HashMap;
use std::sync::Arc;

use peryx_core::Ecosystem;

use crate::serving::EcosystemDriver;

/// The installed ecosystem drivers keyed by [`Ecosystem`], without any of the running server's state.
///
/// The router reaches drivers through [`AppState`](crate::AppState). The binary's config-build and
/// admin commands never construct an `AppState` - they open the stores directly - and reach the
/// drivers through this instead. The composition root builds one, naming its ecosystems in a single
/// place, and neutral build and admin code dispatches through it by an index's ecosystem without
/// naming any.
#[derive(Default)]
pub struct DriverSet {
    drivers: HashMap<Ecosystem, Arc<dyn EcosystemDriver>>,
}

impl DriverSet {
    /// Register `driver` under the ecosystem it serves, consuming and returning `self` so a set is
    /// built in one expression.
    #[must_use]
    pub fn with(mut self, driver: Arc<dyn EcosystemDriver>) -> Self {
        self.drivers.insert(driver.ecosystem(), driver);
        self
    }

    /// The driver for `ecosystem`, or `None` when none is registered.
    #[must_use]
    pub fn get(&self, ecosystem: Ecosystem) -> Option<&Arc<dyn EcosystemDriver>> {
        self.drivers.get(&ecosystem)
    }

    /// Every registered driver.
    pub fn present(&self) -> impl Iterator<Item = &Arc<dyn EcosystemDriver>> {
        self.drivers.values()
    }
}

#[cfg(test)]
#[path = "../tests/unit/driver_set/tests.rs"]
mod tests;
