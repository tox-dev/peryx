//! A standalone registry of ecosystem drivers, for the composition root's build and admin paths.

use std::sync::Arc;

use peryx_core::Ecosystem;

use crate::serving::EcosystemDriver;

/// The installed ecosystem drivers keyed by [`Ecosystem`], without any of the running server's state.
///
/// The router reaches drivers through [`AppState`](crate::AppState). The binary's config-build and
/// admin commands never construct an `AppState` — they open the stores directly — and reach the
/// drivers through this instead. The composition root builds one, naming its ecosystems in a single
/// place, and neutral build and admin code dispatches through it by an index's ecosystem without
/// naming any.
#[derive(Default)]
pub struct DriverSet {
    drivers: [Option<Arc<dyn EcosystemDriver>>; Ecosystem::COUNT],
}

impl DriverSet {
    /// Register `driver` under the ecosystem it serves, consuming and returning `self` so a set is
    /// built in one expression.
    #[must_use]
    pub fn with(mut self, driver: Arc<dyn EcosystemDriver>) -> Self {
        let slot = driver.ecosystem().slot();
        self.drivers[slot] = Some(driver);
        self
    }

    /// The driver for `ecosystem`, or `None` when none is registered.
    #[must_use]
    pub fn get(&self, ecosystem: Ecosystem) -> Option<&Arc<dyn EcosystemDriver>> {
        self.drivers[ecosystem.slot()].as_ref()
    }

    /// Every registered driver, in ecosystem declaration order.
    pub fn present(&self) -> impl Iterator<Item = &Arc<dyn EcosystemDriver>> {
        self.drivers.iter().flatten()
    }
}
