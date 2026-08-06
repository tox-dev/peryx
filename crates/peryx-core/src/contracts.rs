/// Runtime contract for wiring an ecosystem into shared server state.
pub trait EcosystemInstaller<State>: Send + Sync {
    fn install(&self, state: &mut State) {
        self.register_driver(state);
    }

    fn register_driver(&self, state: &mut State);
}
