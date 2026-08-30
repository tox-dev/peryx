//! Both commands resolve the target index's ecosystem driver and drive the shared
//! [query](peryx_driver::retention), so the CLI and the HTTP API produce the same ordered candidates.
//! A dry-run prints a page of tab-separated candidates; an export streams the whole plan as
//! [JSON Lines](https://jsonlines.org/), the identity first. Neither writes metadata.

use std::cell::RefCell;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use peryx_driver::retention::{RetentionQuery, decode_cursor, plan};
use peryx_driver::serving::RetentionDriver;
use peryx_driver::{DriverSet, ScanCancellation};
use peryx_plugin_registry::PluginRegistry;
use peryx_policy::{RetentionConfig, RetentionDecision, RetentionPolicy, RetentionSummary};

use super::CacheStores;
use crate::cli::{RetentionCommand, RetentionDryRunArgs, RetentionExportArgs};
use crate::config::{Config, IndexConfig};

/// # Errors
/// Returns an error if the store cannot be opened, the index is unknown or has no retention-planning
/// driver, the rules file cannot be read, the cursor is stale or malformed, or output fails.
pub fn retention(config: &Config, command: &RetentionCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    retention_with_plugins(config, &crate::compiled_plugins(), command, out)
}

/// # Errors
/// Returns an error when configuration, storage, retention planning, or output fails.
pub fn retention_with_plugins(
    config: &Config,
    plugins: &PluginRegistry,
    command: &RetentionCommand,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let plugins = crate::server::activate_plugins(config, plugins)?;
    let stores = CacheStores::open(config, &plugins, false)?;
    match command {
        RetentionCommand::DryRun(args) => dry_run(config, plugins.drivers(), &stores, args, out),
        RetentionCommand::Export(args) => export(config, plugins.drivers(), &stores, args, out),
    }
}

fn dry_run(
    config: &Config,
    drivers: &DriverSet,
    stores: &CacheStores,
    args: &RetentionDryRunArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let index = resolve_index(config, &args.index)?;
    let driver = resolve_driver(drivers, index)?;
    let policy = load_rules(args.rules.as_deref(), |name| normalize_name(drivers, index, name))?;
    driver.validate_retention(&policy).map_err(anyhow::Error::msg)?;
    let ecosystem = index.ecosystem.as_str();
    let (after, expect, evaluated_at) = resume(args.cursor.as_deref(), &args.index, ecosystem)?;
    writeln!(
        out,
        "action\tresource\tgroup\tartifact\tdigest\tclass\tvisibility\tbytes\trule"
    )?;
    let query = RetentionQuery {
        index: &args.index,
        ecosystem,
        policy: &policy,
        now: evaluated_at,
        after,
        limit: args.limit,
        expect,
    };
    let page = plan(
        driver.as_ref(),
        &stores.meta,
        &query,
        &ScanCancellation::new(),
        &mut |_| Ok(()),
        &mut |decision| write_row(out, decision).map_err(|err| err.to_string()),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))?;
    write_summary(out, "summary", page.summary)?;
    if let Some(cursor) = page.next_cursor {
        writeln!(out, "next-cursor\t{cursor}")?;
    }
    Ok(())
}

fn export(
    config: &Config,
    drivers: &DriverSet,
    stores: &CacheStores,
    args: &RetentionExportArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let index = resolve_index(config, &args.index)?;
    let driver = resolve_driver(drivers, index)?;
    let policy = load_rules(args.rules.as_deref(), |name| normalize_name(drivers, index, name))?;
    driver.validate_retention(&policy).map_err(anyhow::Error::msg)?;
    let ecosystem = index.ecosystem.as_str();
    let (after, expect, evaluated_at) = resume(args.cursor.as_deref(), &args.index, ecosystem)?;
    let query = RetentionQuery {
        index: &args.index,
        ecosystem,
        policy: &policy,
        now: evaluated_at,
        after,
        limit: None,
        expect,
    };
    let out = RefCell::new(out);
    plan(
        driver.as_ref(),
        &stores.meta,
        &query,
        &ScanCancellation::new(),
        &mut |summary| {
            let header =
                serde_json::to_string(&serde_json::json!({ "summary": summary })).map_err(|error| error.to_string())?;
            writeln!(&mut **out.borrow_mut(), "{header}").map_err(|error| error.to_string())
        },
        &mut |decision| write_json_line(&mut **out.borrow_mut(), decision).map_err(|error| error.to_string()),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(())
}

fn resolve_index<'a>(config: &'a Config, index: &str) -> anyhow::Result<&'a IndexConfig> {
    config
        .indexes
        .iter()
        .find(|configured| configured.name == index)
        .context(format!("unknown index {index:?}"))
}

fn resolve_driver(drivers: &DriverSet, index: &IndexConfig) -> anyhow::Result<Arc<dyn RetentionDriver>> {
    drivers
        .get_retention(&index.ecosystem)
        .cloned()
        .context("the ecosystem does not support retention planning")
}

fn normalize_name(drivers: &DriverSet, index: &IndexConfig, name: &str) -> String {
    drivers
        .get_name(&index.ecosystem)
        .map_or_else(|| name.to_owned(), |driver| driver.normalize_name(name))
}

fn load_rules(path: Option<&Path>, normalize: impl Fn(&str) -> String) -> anyhow::Result<RetentionPolicy> {
    let config = match path {
        Some(path) => {
            let text = std::fs::read_to_string(path).with_context(|| format!("read rules file {}", path.display()))?;
            toml::from_str::<RetentionConfig>(&text).with_context(|| format!("parse rules file {}", path.display()))?
        }
        None => RetentionConfig::default(),
    };
    Ok(RetentionPolicy::compile(&config, normalize))
}

fn resume(
    cursor: Option<&str>,
    repository: &str,
    ecosystem: &str,
) -> anyhow::Result<(u64, Option<RetentionSummary>, Option<i64>)> {
    match cursor {
        Some(cursor) => {
            let resume = decode_cursor(cursor).map_err(|err| anyhow::anyhow!("{err}"))?;
            if resume.repository != repository || resume.ecosystem != ecosystem {
                anyhow::bail!("the plan cursor is stale: the repository changed since it was issued");
            }
            Ok((resume.after, Some(resume.expect), resume.evaluated_at))
        }
        None => Ok((0, None, Some(now()))),
    }
}

fn write_row(out: &mut dyn Write, decision: &RetentionDecision) -> std::io::Result<()> {
    writeln!(
        out,
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        name(&decision.outcome),
        decision.resource,
        decision.group.as_deref().unwrap_or(""),
        decision.artifact,
        decision.digest,
        name(&decision.class),
        name(&decision.visibility),
        decision.bytes,
        decision.rule.unwrap_or(""),
    )
}

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

#[cfg(test)]
#[path = "../../tests/unit/tests/app/retention_tests.rs"]
mod tests;
