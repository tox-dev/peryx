pub trait EcosystemInstaller<State>: Send + Sync {
    fn install(&self, state: &mut State) {
        self.register_driver(state);
    }

    fn register_driver(&self, state: &mut State);
}

#[cfg(test)]
#[path = "../tests/unit/contracts/tests.rs"]
mod tests;
