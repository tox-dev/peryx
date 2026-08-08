/// Runtime contract for wiring an ecosystem into shared application state.
pub trait EcosystemInstaller<State>: Send + Sync {
    /// Use the default install path unless an ecosystem requires custom setup.
    fn install(&self, state: &mut State) {
        self.register_driver(state);
    }

    /// Register the ecosystem driver, indexers, and lexicon for one plugin.
    fn register_driver(&self, state: &mut State);
}

#[cfg(test)]
#[path = "../tests/unit/contracts/tests.rs"]
mod tests;
