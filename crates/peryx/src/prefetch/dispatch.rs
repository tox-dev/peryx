use std::io::Write;

use anyhow::Context as _;
use peryx_driver::serving::{MirrorAction, MirrorRequest};

use crate::cli::PrefetchCommand;
use crate::config::{Config, IndexKind};
use crate::server;

/// # Errors
/// Returns configuration, index resolution, or implementation errors.
pub async fn run(config: &Config, command: &PrefetchCommand, output: &mut (dyn Write + Send)) -> anyhow::Result<()> {
    run_with_plugins(config, &crate::compiled_plugins(), command, output).await
}

/// # Errors
/// Returns an error if state or mirror options are invalid, the index cannot be resolved, or mirroring fails.
pub async fn run_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    command: &PrefetchCommand,
    output: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    let plugins = server::activate_plugins(config, plugins)?;
    run_with_active_plugins(config, &plugins, command, output).await
}

pub async fn run_with_active_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    command: &PrefetchCommand,
    output: &mut (dyn Write + Send),
) -> anyhow::Result<()> {
    let state = server::build_state_with_active_plugins(config, plugins)?;
    let options = command.options();
    let index = config
        .indexes
        .iter()
        .find(|index| index.name == options.index || index.route == options.index)
        .ok_or_else(|| anyhow::anyhow!("unknown cached index {:?}", options.index))?;
    let driver = state
        .mirror_driver_for(&index.ecosystem)
        .context("configured ecosystem does not support mirroring")?;
    let configured = mirror_configuration(config, index)?;
    let overrides = mirror_overrides(&options.overrides)?;
    driver
        .mirror(
            state.clone(),
            MirrorRequest {
                action: action(command),
                index: &options.index,
                settings: &index.ecosystem_settings,
                configured: &configured,
                overrides: &overrides,
            },
            output,
        )
        .await
        .map_err(anyhow::Error::msg)
}

fn mirror_configuration(config: &Config, index: &crate::config::IndexConfig) -> anyhow::Result<toml::Table> {
    let prefetch = match &index.kind {
        IndexKind::Cached { prefetch, .. } => prefetch,
        IndexKind::Hosted { .. } => anyhow::bail!("index {:?} is hosted and has no upstream", index.name),
        IndexKind::Virtual { layers, .. } => {
            let mut cached = layers.iter().filter_map(|layer| {
                config
                    .indexes
                    .iter()
                    .find(|index| index.name == *layer)
                    .and_then(|index| match &index.kind {
                        IndexKind::Cached { prefetch, .. } => Some(prefetch),
                        _ => None,
                    })
            });
            let Some(prefetch) = cached.next() else {
                anyhow::bail!("index {:?} has no cached member", index.name);
            };
            anyhow::ensure!(
                cached.next().is_none(),
                "index {:?} has more than one cached member",
                index.name
            );
            prefetch
        }
    };
    Ok(prefetch.options.clone())
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
