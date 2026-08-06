//! Ecosystem contracts for non-ecosystem crates.

use async_trait::async_trait;
use peryx_driver::AppState;

/// Minimal API an ecosystem exposes to the composition root.
#[async_trait]
pub trait EcosystemInstaller: Send + Sync {
    /// Register runtime services (ecosystem driver + indexer + optional capabilities).
    async fn install(&self, _state: &mut AppState) {}

    /// Register ecosystem drivers for policy resolution, index discovery, and route dispatch.
    fn register_driver(&self, _state: &mut AppState) {}
}

