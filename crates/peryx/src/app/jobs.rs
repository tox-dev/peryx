//! Durable job-run history commands.

use std::io::Write;

use anyhow::Context as _;
use peryx_storage::meta::{JobKind, JobRunRecord, JobState, MetaStore};

use crate::cli::JobCommand;
use crate::config::Config;

/// List or show durable job-run history.
///
/// # Errors
/// Returns an error if the metadata store cannot be opened or read, the job run is unknown, or
/// output fails.
pub fn job(config: &Config, command: &JobCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    let path = config.data_dir.join("peryx.redb");
    let store = MetaStore::open_existing(&path).with_context(|| format!("open metadata store {}", path.display()))?;
    match command {
        JobCommand::List(_) => job_list(&store, out),
        JobCommand::Show(args) => job_show(&store, &args.id, out),
    }
}

fn job_list(store: &MetaStore, out: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(
        out,
        "id\tkind\tscope\tstate\tstarted_at_unix\tfinished_at_unix\tprocessed\tchanged\terror"
    )?;
    for run in store.list_job_runs()? {
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            run.id,
            job_kind(run.kind),
            optional_text(&run.scope),
            job_state(run.state),
            run.started_at_unix,
            optional_number(run.finished_at_unix),
            run.items_processed,
            run.items_changed,
            run.error.as_deref().map_or("-", optional_text),
        )?;
    }
    Ok(())
}

fn job_show(store: &MetaStore, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let run = store
        .get_job_run(id)?
        .with_context(|| format!("unknown job run {id:?}"))?;
    write_job(&run, out)
}

fn write_job(run: &JobRunRecord, out: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(out, "id\t{}", run.id)?;
    writeln!(out, "kind\t{}", job_kind(run.kind))?;
    writeln!(out, "scope\t{}", optional_text(&run.scope))?;
    writeln!(out, "state\t{}", job_state(run.state))?;
    writeln!(out, "started_at_unix\t{}", run.started_at_unix)?;
    writeln!(out, "finished_at_unix\t{}", optional_number(run.finished_at_unix))?;
    writeln!(out, "processed\t{}", run.items_processed)?;
    writeln!(out, "changed\t{}", run.items_changed)?;
    writeln!(out, "error\t{}", run.error.as_deref().map_or("-", optional_text))?;
    Ok(())
}

const fn job_kind(kind: JobKind) -> &'static str {
    match kind {
        JobKind::CacheRefresh => "cache_refresh",
    }
}

const fn job_state(state: JobState) -> &'static str {
    match state {
        JobState::Running => "running",
        JobState::Succeeded => "succeeded",
        JobState::Failed => "failed",
    }
}

const fn optional_text(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

fn optional_number(value: Option<i64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}
