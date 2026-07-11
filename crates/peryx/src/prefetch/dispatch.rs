//! Subcommand entry point and the per-ecosystem mirror dispatch boundary.

use std::sync::Arc;

use peryx_driver::AppState;

use super::Output;
use super::oci::{oci_images, oci_lookup, oci_mirror, oci_plan, oci_settings};
use super::pypi::{pypi_plan, pypi_sync, pypi_verify};
use crate::cli::{PrefetchCommand, PrefetchOptions};
use crate::config::Config;
use crate::server;

/// Run a `peryx prefetch` subcommand.
///
/// # Errors
/// Returns an error when configuration is invalid, upstream access fails, selected cache entries
/// fail verification, or output cannot be written.
pub async fn run(config: &Config, command: &PrefetchCommand, out: &mut Output) -> anyhow::Result<()> {
    let state = server::build_state(config)?;
    let options = command.options();
    let driver = mirror_driver(&state, &options.index);
    match command {
        PrefetchCommand::Plan(_) => driver.plan(config, &state, options, out).await,
        PrefetchCommand::Sync(_) => driver.sync(config, &state, options, out).await,
        PrefetchCommand::Verify(_) => driver.verify(config, &state, options, out).await,
    }
}

/// A per-ecosystem mirror: `plan` lists, `sync` fetches, `verify` checks. Each ecosystem's custom
/// behavior lives in its own impl, so the orchestration above dispatches once and never branches.
#[async_trait::async_trait]
trait IndexMirror: Sync {
    async fn plan(
        &self,
        config: &Config,
        state: &Arc<AppState>,
        options: &PrefetchOptions,
        out: &mut Output,
    ) -> anyhow::Result<()>;
    async fn sync(
        &self,
        config: &Config,
        state: &Arc<AppState>,
        options: &PrefetchOptions,
        out: &mut Output,
    ) -> anyhow::Result<()>;
    async fn verify(
        &self,
        config: &Config,
        state: &Arc<AppState>,
        options: &PrefetchOptions,
        out: &mut Output,
    ) -> anyhow::Result<()>;
}

const PYPI_MIRROR: PypiMirror = PypiMirror;
const OCI_MIRROR: OciMirror = OciMirror;

/// Select the mirror by the target index's ecosystem, the one dispatch boundary. An unknown or a
/// `PyPI` index takes the `PyPI` mirror, which reports the unknown through its own resolution.
fn mirror_driver(state: &Arc<AppState>, name: &str) -> &'static dyn IndexMirror {
    let is_oci = state
        .indexes
        .iter()
        .any(|index| (index.name == name || index.route == name) && index.ecosystem == peryx_core::Ecosystem::Oci);
    if is_oci { &OCI_MIRROR } else { &PYPI_MIRROR }
}

struct PypiMirror;
struct OciMirror;

#[async_trait::async_trait]
impl IndexMirror for PypiMirror {
    async fn plan(
        &self,
        config: &Config,
        state: &Arc<AppState>,
        options: &PrefetchOptions,
        out: &mut Output,
    ) -> anyhow::Result<()> {
        pypi_plan(config, state, options, out).await
    }
    async fn sync(
        &self,
        config: &Config,
        state: &Arc<AppState>,
        options: &PrefetchOptions,
        out: &mut Output,
    ) -> anyhow::Result<()> {
        pypi_sync(config, state, options, out).await
    }
    async fn verify(
        &self,
        config: &Config,
        state: &Arc<AppState>,
        options: &PrefetchOptions,
        out: &mut Output,
    ) -> anyhow::Result<()> {
        pypi_verify(config, state, options, out).await
    }
}

#[async_trait::async_trait]
impl IndexMirror for OciMirror {
    async fn plan(
        &self,
        config: &Config,
        state: &Arc<AppState>,
        options: &PrefetchOptions,
        out: &mut Output,
    ) -> anyhow::Result<()> {
        oci_plan(oci_lookup(state, &options.index), &oci_images(config, options), out)
    }
    async fn sync(
        &self,
        config: &Config,
        state: &Arc<AppState>,
        options: &PrefetchOptions,
        out: &mut Output,
    ) -> anyhow::Result<()> {
        let images = oci_images(config, options);
        oci_mirror(
            state,
            oci_lookup(state, &options.index),
            oci_settings(config, &options.index),
            &images,
            peryx_ecosystem_oci::MirrorMode::Sync,
            out,
        )
        .await
    }
    async fn verify(
        &self,
        config: &Config,
        state: &Arc<AppState>,
        options: &PrefetchOptions,
        out: &mut Output,
    ) -> anyhow::Result<()> {
        let images = oci_images(config, options);
        oci_mirror(
            state,
            oci_lookup(state, &options.index),
            oci_settings(config, &options.index),
            &images,
            peryx_ecosystem_oci::MirrorMode::Verify,
            out,
        )
        .await
    }
}
