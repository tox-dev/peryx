//! Mirror command dispatch.

use std::io::Write;

use peryx_driver::serving::{MirrorAction, MirrorRequest};

use crate::cli::{PrefetchCommand, PrefetchOptions};
use crate::config::{Config, IndexKind};
use crate::server;

/// Run a `peryx mirror` command through its ecosystem capability.
///
/// # Errors
/// Returns configuration, index resolution, or implementation errors.
/// # Panics
///
/// Panics if a configured ecosystem does not register mirroring.
pub async fn run(config: &Config, command: &PrefetchCommand, output: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    let state = server::build_state(config)?;
    let options = command.options();
    let index = state
        .indexes
        .iter()
        .find(|index| index.name == options.index || index.route == options.index)
        .ok_or_else(|| anyhow::anyhow!("unknown cached index {:?}", options.index))?;
    let driver = state
        .mirror_driver_for(index.ecosystem)
        .expect("configured ecosystem must register mirroring");
    let settings = config
        .indexes
        .iter()
        .find(|configured| configured.name == index.name)
        .map_or_else(toml::Table::new, |configured| configured.ecosystem_settings.clone());
    let configured = mirror_configuration(config, options);
    let overrides = mirror_overrides(&options.overrides)?;
    driver
        .mirror(
            state.clone(),
            MirrorRequest {
                action: action(command),
                index: &options.index,
                settings: &settings,
                configured: &configured,
                overrides: &overrides,
            },
            output,
        )
        .await
        .map_err(anyhow::Error::msg)
}

fn mirror_overrides(options: &[String]) -> anyhow::Result<toml::Table> {
    let mut overrides = toml::Table::new();
    for option in options {
        let Some((key, value)) = option
            .split_once('=')
            .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        else {
            anyhow::bail!("mirror option {option:?} must be KEY=VALUE");
        };
        let mut table = toml::from_str::<toml::Table>(&format!("value = {value}"))
            .map_err(|error| anyhow::anyhow!("invalid value for mirror option {key:?}: {error}"))?;
        overrides.insert(
            key.to_owned(),
            table.remove("value").expect("the parser preserves the key"),
        );
    }
    Ok(overrides)
}

const fn action(command: &PrefetchCommand) -> MirrorAction {
    match command {
        PrefetchCommand::Plan(_) => MirrorAction::Plan,
        PrefetchCommand::Sync(_) => MirrorAction::Sync,
        PrefetchCommand::Verify(_) => MirrorAction::Verify,
    }
}

fn mirror_configuration(config: &Config, options: &PrefetchOptions) -> toml::Table {
    let index = config
        .indexes
        .iter()
        .find(|index| index.name == options.index || index.route == options.index)
        .expect("runtime indexes originate from configuration");
    let prefetch = match &index.kind {
        IndexKind::Cached { prefetch, .. } => Some(prefetch.as_ref()),
        IndexKind::Virtual { layers, .. } => config.indexes.iter().find_map(|index| {
            layers
                .contains(&index.name)
                .then_some(&index.kind)
                .and_then(|kind| match kind {
                    IndexKind::Cached { prefetch, .. } => Some(prefetch.as_ref()),
                    _ => None,
                })
        }),
        IndexKind::Hosted { .. } => None,
    };
    prefetch.map_or_else(toml::Table::new, |prefetch| prefetch.options.clone())
}
