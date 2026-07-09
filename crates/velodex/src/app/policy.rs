//! Policy dry-run: preview allow/deny decisions over the cached and uploaded records.

use std::io::Write;

use anyhow::Context as _;
use velodex_ecosystem_pypi::policy::PypiPolicy as _;
use velodex_ecosystem_pypi::upload::Uploaded;
use velodex_ecosystem_pypi::{ProjectDetail, normalize_name, parse_detail};
use velodex_policy::{PolicyAction, PolicyDenial};
use velodex_storage::meta::CachedIndex;

use super::{CacheStores, index_names, split_page_key};
use crate::cli::{PolicyCommand, PolicyDryRunArgs};
use crate::config::Config;
use crate::server;

/// Run a policy inspection command.
///
/// # Errors
/// Returns an error if configured indexes cannot be built, the metadata store cannot be read, or
/// output fails.
pub fn policy(config: &Config, command: &PolicyCommand, out: &mut dyn Write) -> anyhow::Result<()> {
    let stores = CacheStores::open(config)?;
    let indexes = server::build_indexes(&config.indexes, config.offline)?;
    match command {
        PolicyCommand::DryRun(args) => policy_dry_run(config, &stores, &indexes, args, out),
    }
}

fn policy_dry_run(
    config: &Config,
    stores: &CacheStores,
    indexes: &[velodex_http::Index],
    args: &PolicyDryRunArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    writeln!(out, "action\tindex\tproject\tfilename\tversion\trule\tfield\treason")?;
    let names = index_names(config);
    let project_filter = args.project.as_deref().map(normalize_name);
    stores
        .meta
        .scan_index_records(|key, bytes| {
            let (index_name, project) = split_page_key(key, &names);
            let Some(index) = matching_policy_index(indexes, &index_name, args.index.as_deref()) else {
                return Ok::<(), std::io::Error>(());
            };
            if project_filter
                .as_deref()
                .is_some_and(|filter| filter != project.as_str())
            {
                return Ok::<(), std::io::Error>(());
            }
            let record = CachedIndex::decode(bytes).map_err(std::io::Error::other)?;
            let parsed = parse_detail(&record.body).map_err(std::io::Error::other)?;
            let detail = ProjectDetail {
                meta: parsed.meta,
                name: project,
                versions: parsed.versions,
                files: parsed.files,
            };
            for denial in index.policy.preview_detail(PolicyAction::Serve, &detail) {
                write_policy_denial(out, &index.name, &denial)?;
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan cached index records")?;
    stores
        .meta
        .scan_upload_records(|key, bytes| {
            let Some((index_name, project, _filename)) = upload_key_parts(key, &names) else {
                return Ok::<(), std::io::Error>(());
            };
            let Some(index) = matching_policy_index(indexes, &index_name, args.index.as_deref()) else {
                return Ok::<(), std::io::Error>(());
            };
            if project_filter.as_deref().is_some_and(|filter| filter != project) {
                return Ok::<(), std::io::Error>(());
            }
            let uploaded: Uploaded = serde_json::from_slice(bytes).map_err(std::io::Error::other)?;
            if let Err(denial) = index.policy.check_file(PolicyAction::Upload, project, &uploaded.file) {
                write_policy_denial(out, &index.name, &denial)?;
            }
            Ok::<(), std::io::Error>(())
        })
        .context("scan upload records")?;
    Ok(())
}

fn matching_policy_index<'a>(
    indexes: &'a [velodex_http::Index],
    index_name: &str,
    filter: Option<&str>,
) -> Option<&'a velodex_http::Index> {
    let index = indexes.iter().find(|index| index.name == index_name)?;
    filter
        .is_none_or(|filter| filter == index.name || filter == index.route)
        .then_some(index)
}

fn write_policy_denial(out: &mut dyn Write, index: &str, denial: &PolicyDenial) -> std::io::Result<()> {
    writeln!(
        out,
        "{}\t{index}\t{}\t{}\t{}\t{}\t{}\t{}",
        denial.action,
        denial.project,
        denial.filename.as_deref().unwrap_or(""),
        denial.version.as_deref().unwrap_or(""),
        denial.rule,
        denial.field,
        denial.reason
    )
}

fn upload_key_parts<'a>(key: &'a str, index_names: &[&str]) -> Option<(String, &'a str, &'a str)> {
    for name in index_names {
        let Some(rest) = key.strip_prefix(name).and_then(|rest| rest.strip_prefix('/')) else {
            continue;
        };
        let (project, filename) = rest.split_once('/')?;
        return Some(((*name).to_owned(), project, filename));
    }
    let (index, rest) = key.split_once('/')?;
    let (project, filename) = rest.split_once('/')?;
    Some((index.to_owned(), project, filename))
}
