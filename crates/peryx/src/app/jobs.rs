use std::io::Write;
use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::{Context as _, ensure};
use peryx_driver::AppState;
use peryx_driver::jobs::{
    JobLimits, JobScheduler, MAX_SEARCH_REBUILD_CHUNK, NodeJob, ScheduledJob, SearchRebuildJob, scheduled_job,
};
use peryx_ha_distributed::AuthorityDrainJob;
use peryx_storage::meta::{JobRunQuery, MetaStore};

use crate::cli::JobCommand;
use crate::config::Config;

/// # Errors
/// Returns an error when storage or output fails, a job run is unknown, or execution cannot start.
pub fn job(config: &Config, command: &JobCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    job_with_plugins(config, &crate::compiled_plugins(), command, out)
}

/// # Errors
/// Returns an error when store access, job construction or execution, runtime startup, or output fails.
pub fn job_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    command: &JobCommand,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let plugins = crate::server::activate_plugins(config, plugins)?;
    job_with_active_plugins(config, &plugins, command, out)
}

pub fn job_with_active_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    command: &JobCommand,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    match command {
        JobCommand::List(_) => job_list(&open_store(config, plugins)?, out),
        JobCommand::Show(args) => job_show(&open_store(config, plugins)?, &args.id, out),
        JobCommand::Run {
            command,
            target,
            source,
            item_limit,
            concurrency,
            timeout_secs,
            ..
        } => run_registered_job(
            config,
            plugins,
            command.as_deref(),
            peryx_plugin_registry::OperatorJobRequest {
                target,
                source: source.as_deref(),
                item_limit: *item_limit,
                concurrency: *concurrency,
                timeout_secs: *timeout_secs,
            },
            out,
        ),
        JobCommand::Reindex { chunk_size, .. } => run_search_rebuild(config, plugins, *chunk_size, out),
        JobCommand::Drain { authority, .. } => run_authority_drain(config, plugins, authority, out),
    }
}

fn open_store(config: &Config, plugins: &peryx_plugin_registry::PluginRegistry) -> anyhow::Result<MetaStore> {
    let path = config.data_dir.join("peryx.redb");
    crate::metadata::open_existing_read_only(&path, plugins)
}

fn run_registered_job(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    command: Option<&str>,
    request: peryx_plugin_registry::OperatorJobRequest<'_>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let jobs = plugins.operator_job_commands().collect::<Vec<_>>();
    let command = command
        .filter(|command| jobs.iter().any(|(registered, _)| registered == command))
        .ok_or_else(|| operator_job_selection_error(command, &jobs))?;
    let configured = plugins
        .compile_operator_job(command, request)
        .map_err(anyhow::Error::msg)?;
    run_node_job(
        config,
        plugins,
        move |state| scheduled_job(state, &ScheduledJob::Plugin(configured)),
        out,
    )
}

fn operator_job_selection_error(
    command: Option<&str>,
    jobs: &[(&str, peryx_plugin_registry::OperatorJobDefaults)],
) -> anyhow::Error {
    let reason = command.map_or_else(
        || "operator job command is required".to_owned(),
        |command| format!("unknown operator job command {command:?}"),
    );
    if jobs.is_empty() {
        return anyhow::anyhow!("{reason}\nno operator job commands are registered");
    }
    let jobs = jobs
        .iter()
        .map(|(command, defaults)| {
            format!(
                "  {command} (item-limit={}, concurrency={}, timeout-secs={})",
                defaults.item_limit, defaults.concurrency, defaults.timeout_secs
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::anyhow!("{reason}\nregistered operator job commands:\n{jobs}")
}

fn run_search_rebuild(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    chunk_size: usize,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    ensure!(
        chunk_size <= MAX_SEARCH_REBUILD_CHUNK,
        "chunk-size exceeds the per-run limit"
    );
    let chunk = NonZeroUsize::new(chunk_size).context("chunk-size must be positive")?;
    run_node_job(
        config,
        plugins,
        move |_| Ok(Arc::new(SearchRebuildJob::new(chunk))),
        out,
    )
}

fn run_authority_drain(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    authority: &str,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    ensure!(!authority.trim().is_empty(), "authority must not be empty");
    ensure!(
        config.availability.mode().is_distributed(),
        "authority drain requires distributed availability"
    );
    run_node_job(
        config,
        plugins,
        move |state| {
            let drainer = state
                .serving
                .authority_drainer()
                .cloned()
                .ok_or_else(|| "distributed availability did not install authority draining".to_owned())?;
            Ok(Arc::new(AuthorityDrainJob::new(authority, drainer)))
        },
        out,
    )
}

fn run_node_job(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    create: impl FnOnce(&AppState) -> Result<Arc<dyn NodeJob>, String>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let report = runtime.block_on(async {
        let state = crate::server::build_state_with_active_plugins(config, plugins)?;
        let scheduler = JobScheduler::new(state.serving.clone(), JobLimits::node_local());
        let result = scheduler
            .run(create(&state).map_err(anyhow::Error::msg)?)
            .await
            .map_err(anyhow::Error::msg);
        scheduler.shutdown().await;
        result
    })?;
    writeln!(out, "processed\t{}", report.processed)?;
    writeln!(out, "changed\t{}", report.changed)?;
    writeln!(out, "quota_released\t{}", report.quota_released)?;
    writeln!(out, "quota_remaining\t{}", report.quota_remaining)?;
    Ok(())
}

fn job_list(store: &MetaStore, out: &mut dyn Write) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *out, &store.query_job_runs(&JobRunQuery::default())?)?;
    writeln!(out)?;
    Ok(())
}

fn job_show(store: &MetaStore, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let run = store
        .get_job_run(id)?
        .with_context(|| format!("unknown job run {id:?}"))?;
    serde_json::to_writer(&mut *out, &run)?;
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/job_tests.rs"]
mod tests;
