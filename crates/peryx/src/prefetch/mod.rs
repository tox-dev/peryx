mod dispatch;

pub(crate) use dispatch::run_with_active_plugins;
pub use dispatch::{run, run_with_plugins};

#[cfg(test)]
#[path = "../../tests/unit/prefetch/tests.rs"]
mod tests;
