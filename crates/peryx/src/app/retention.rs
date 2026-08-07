//! Retention-plan preview and export over the local store.
//!
//! Both commands resolve the target index's ecosystem driver and drive the shared
//! [query](peryx_driver::retention), so the CLI and the HTTP API produce the same ordered candidates.
//! A dry-run prints a page of tab-separated candidates; an export streams the whole plan as
//! [JSON Lines](https://jsonlines.org/), the identity first. Neither writes metadata.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use peryx_driver::retention::{RetentionQuery, decode_cursor, plan, summary};
use peryx_driver::serving::EcosystemDriver;
use peryx_policy::{RetentionConfig, RetentionDecision, RetentionPolicy, RetentionSummary};

use super::CacheStores;
use crate::cli::{RetentionCommand, RetentionDryRunArgs, RetentionExportArgs};
use crate::config::Config;
use crate::server;

/// Run a retention command against the configured store.
///
/// # Errors
/// Returns an error if the store cannot be opened, the index is unknown or has no retention-planning
/// driver, the rules file cannot be read, the cursor is stale or malformed, or output fails.
pub fn retention(config: &Config, command: &RetentionCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    let stores = CacheStores::open(config)?;
    match command {
        RetentionCommand::DryRun(args) => dry_run(config, &stores, args, out),
        RetentionCommand::Export(args) => export(config, &stores, args, out),
    }
}

fn dry_run(
    config: &Config,
    stores: &CacheStores,
    args: &RetentionDryRunArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let driver = resolve_driver(config, &args.index)?;
    let driver = driver
        .capabilities()
        .retention
        .context("the ecosystem does not support retention planning")?;
    let policy = load_rules(args.rules.as_deref())?;
    let (after, expect) = resume(args.cursor.as_deref())?;
    writeln!(
        out,
        "action\tproject\tversion\tartifact\tdigest\tclass\tvisibility\tbytes\trule"
    )?;
    let query = RetentionQuery {
        index: &args.index,
        policy: &policy,
        now: Some(now()),
        after,
        limit: args.limit,
        expect,
    };
    let page = plan(driver, &stores.meta, &query, &mut |decision| {
        write_row(out, decision).map_err(|err| err.to_string())
    })
    .map_err(|err| anyhow::anyhow!("{err}"))?;
    write_summary(out, "summary", page.summary)?;
    if let Some(cursor) = page.next_cursor {
        writeln!(out, "next-cursor\t{cursor}")?;
    }
    Ok(())
}

fn export(
    config: &Config,
    stores: &CacheStores,
    args: &RetentionExportArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let driver = resolve_driver(config, &args.index)?;
    let driver = driver
        .capabilities()
        .retention
        .context("the ecosystem does not support retention planning")?;
    let policy = load_rules(args.rules.as_deref())?;
    let (after, expect) = resume(args.cursor.as_deref())?;
    let summary = summary(&stores.meta, &args.index, &policy).map_err(|err| anyhow::anyhow!("{err}"))?;
    // Refuse a resume the repository has outgrown before writing a header the reader would trust.
    if expect.is_some_and(|expected| expected != summary) {
        anyhow::bail!("the plan cursor is stale: the repository changed since it was issued");
    }
    writeln!(
        out,
        "{}",
        serde_json::to_string(&serde_json::json!({ "summary": summary }))?
    )?;
    let query = RetentionQuery {
        index: &args.index,
        policy: &policy,
        now: Some(now()),
        after,
        limit: None,
        expect: None,
    };
    plan(driver, &stores.meta, &query, &mut |decision| {
        write_json_line(out, decision).map_err(|err| err.to_string())
    })
    .map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(())
}

fn resolve_driver(config: &Config, index: &str) -> anyhow::Result<Arc<dyn EcosystemDriver>> {
    let ecosystem = config
        .indexes
        .iter()
        .find(|configured| configured.name == index)
        .context(format!("unknown index {index:?}"))?
        .ecosystem;
    server::drivers()
        .get(ecosystem)
        .context(format!("no driver for the {ecosystem} ecosystem"))
        .cloned()
}

fn load_rules(path: Option<&Path>) -> anyhow::Result<RetentionPolicy> {
    let config = match path {
        Some(path) => {
            let text = std::fs::read_to_string(path).with_context(|| format!("read rules file {}", path.display()))?;
            toml::from_str::<RetentionConfig>(&text).with_context(|| format!("parse rules file {}", path.display()))?
        }
        None => RetentionConfig::default(),
    };
    Ok(RetentionPolicy::compile(&config))
}

fn resume(cursor: Option<&str>) -> anyhow::Result<(u64, Option<RetentionSummary>)> {
    match cursor {
        Some(cursor) => {
            let resume = decode_cursor(cursor).map_err(|err| anyhow::anyhow!("{err}"))?;
            Ok((resume.after, Some(resume.expect)))
        }
        None => Ok((0, None)),
    }
}

fn write_row(out: &mut dyn Write, decision: &RetentionDecision) -> std::io::Result<()> {
    writeln!(
        out,
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        name(&decision.outcome),
        decision.project,
        decision.version.as_deref().unwrap_or(""),
        decision.artifact,
        decision.digest,
        name(&decision.class),
        name(&decision.visibility),
        decision.bytes,
        decision.rule.unwrap_or(""),
    )
}

/// The `snake_case` name of a unit enum, for a tab-separated cell. The value serializes to a JSON
/// string whose quotes this strips.
fn name(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value)
        .expect("a retention enum always serializes")
        .trim_matches('"')
        .to_owned()
}

fn write_json_line(out: &mut dyn Write, decision: &RetentionDecision) -> std::io::Result<()> {
    writeln!(
        out,
        "{}",
        serde_json::to_string(decision).expect("a decision always serializes")
    )
}

fn write_summary(out: &mut dyn Write, label: &str, summary: RetentionSummary) -> std::io::Result<()> {
    let frontier = summary.frontier;
    writeln!(
        out,
        "{label}\tpolicy_version={}\trepository={}\tcatalog={}\tpolicy={}",
        summary.policy_version, frontier.repository, frontier.catalog, frontier.policy
    )
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}
