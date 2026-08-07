use std::collections::BTreeMap;

use peryx_driver::retention::encode_cursor;
use peryx_ecosystem_registry::pypi::store::PypiStore as _;
use peryx_ecosystem_registry::pypi::upload::Uploaded;
use peryx_ecosystem_registry::pypi::{CoreMetadata, File, Provenance, Yanked};
use peryx_policy::{RetentionFrontier, RetentionSummary};

use super::*;
use crate::app;
use crate::cli::{RetentionCommand, RetentionDryRunArgs, RetentionExportArgs};

fn seed_upload(config: &Config, project: &str, version: &str) {
    let meta = MetaStore::open(config.data_dir.join("peryx.redb")).unwrap();
    let filename = format!("{project}-{version}.whl");
    let record = serde_json::to_vec(&Uploaded {
        version: version.to_owned(),
        file: File {
            filename: filename.clone(),
            url: format!("http://localhost/files/{filename}"),
            hashes: BTreeMap::from([("sha256".to_owned(), format!("sha-{version}"))]),
            requires_python: None,
            size: Some(1024),
            upload_time: Some("2020-01-01T00:00:00Z".to_owned()),
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
        trashed: None,
    })
    .unwrap();
    meta.put_upload("hosted", project, &filename, &record).unwrap();
}

fn rules_file(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("rules.toml");
    std::fs::write(&path, body).unwrap();
    path
}

fn dry_run(config: &Config, args: RetentionDryRunArgs) -> String {
    let mut out = Vec::new();
    app::retention(config, &RetentionCommand::DryRun(args), &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn dry_run_args(index: &str) -> RetentionDryRunArgs {
    RetentionDryRunArgs {
        runtime: runtime_args(),
        index: index.to_owned(),
        rules: None,
        limit: None,
        cursor: None,
    }
}

#[test]
fn test_retention_dry_run_lists_every_candidate_with_the_plan_identity() {
    let (_dir, config, _digest) = cache_fixture();
    seed_upload(&config, "pkg", "1.0");
    seed_upload(&config, "pkg", "2.0");

    let text = dry_run(&config, dry_run_args("hosted"));

    assert!(text.starts_with("action\tproject\tversion\tartifact\tdigest\tclass\tvisibility\tbytes\trule\n"));
    assert!(
        text.contains("retain\tpkg\t2.0\tpkg-2.0.whl\tsha-2.0\thosted\tactive\t1024\t\n"),
        "{text}"
    );
    assert!(
        text.contains("retain\tpkg\t1.0\tpkg-1.0.whl\tsha-1.0\thosted\tactive\t1024\t\n"),
        "{text}"
    );
    assert!(text.contains("summary\tpolicy_version="), "{text}");
}

#[test]
fn test_retention_dry_run_applies_rules_from_a_file() {
    let (dir, config, _digest) = cache_fixture();
    seed_upload(&config, "pkg", "1.0");
    seed_upload(&config, "pkg", "2.0");
    let rules = rules_file(
        dir.path(),
        "[[keep]]\nselector = \"keep-latest\"\ncount = 1\n[[expire]]\nselector = \"project-prefix\"\nprefix = \"\"\n",
    );
    let mut args = dry_run_args("hosted");
    args.rules = Some(rules);

    let text = dry_run(&config, args);

    assert!(text.contains("retain\tpkg\t2.0\tpkg-2.0.whl"), "{text}");
    assert!(
        text.contains("remove\tpkg\t1.0\tpkg-1.0.whl\tsha-1.0\thosted\tactive\t1024\tproject-prefix\n"),
        "{text}"
    );
}

#[test]
fn test_retention_dry_run_pages_and_resumes() {
    let (_dir, config, _digest) = cache_fixture();
    seed_upload(&config, "pkg", "1.0");
    seed_upload(&config, "pkg", "2.0");
    let mut args = dry_run_args("hosted");
    args.limit = Some(1);

    let first = dry_run(&config, args.clone());
    let cursor = first
        .lines()
        .find_map(|line| line.strip_prefix("next-cursor\t"))
        .expect("a full page prints a cursor")
        .to_owned();
    args.cursor = Some(cursor);
    let second = dry_run(&config, args);

    assert!(first.contains("pkg-2.0.whl"), "{first}");
    assert!(second.contains("pkg-1.0.whl"), "{second}");
    assert!(!second.contains("pkg-2.0.whl"), "{second}");
}

#[test]
fn test_retention_export_streams_json_lines_identity_first() {
    let (_dir, config, _digest) = cache_fixture();
    seed_upload(&config, "pkg", "1.0");
    let mut out = Vec::new();

    app::retention(
        &config,
        &RetentionCommand::Export(RetentionExportArgs {
            runtime: runtime_args(),
            index: "hosted".to_owned(),
            rules: None,
            cursor: None,
        }),
        &mut out,
    )
    .unwrap();

    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].contains("\"summary\""), "{text}");
    assert!(lines[1].contains("\"artifact\":\"pkg-1.0.whl\""), "{text}");
}

#[test]
fn test_retention_export_rejects_a_stale_cursor() {
    let (_dir, config, _digest) = cache_fixture();
    seed_upload(&config, "pkg", "1.0");
    let stale = encode_cursor(
        0,
        RetentionSummary {
            policy_version: 999,
            frontier: RetentionFrontier::default(),
        },
    );

    let error = app::retention(
        &config,
        &RetentionCommand::Export(RetentionExportArgs {
            runtime: runtime_args(),
            index: "hosted".to_owned(),
            rules: None,
            cursor: Some(stale),
        }),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("stale"), "{error}");
}

#[test]
fn test_retention_dry_run_rejects_an_unknown_index() {
    let (_dir, config, _digest) = cache_fixture();

    let error = app::retention(
        &config,
        &RetentionCommand::DryRun(dry_run_args("absent")),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("unknown index"), "{error}");
}

#[test]
fn test_retention_dry_run_rejects_a_malformed_cursor() {
    let (_dir, config, _digest) = cache_fixture();
    let mut args = dry_run_args("hosted");
    args.cursor = Some("not a cursor".to_owned());

    let error = app::retention(&config, &RetentionCommand::DryRun(args), &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("invalid retention plan cursor"), "{error}");
}

#[test]
fn test_retention_dry_run_rejects_an_unreadable_rules_file() {
    let (_dir, config, _digest) = cache_fixture();
    let mut args = dry_run_args("hosted");
    args.rules = Some(config.data_dir.join("missing.toml"));

    let error = app::retention(&config, &RetentionCommand::DryRun(args), &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("read rules file"), "{error}");
}

#[test]
fn test_retention_dry_run_rejects_invalid_rules_toml() {
    let (dir, config, _digest) = cache_fixture();
    let rules = rules_file(dir.path(), "[[keep]]\nselector = \"nonsense\"\n");
    let mut args = dry_run_args("hosted");
    args.rules = Some(rules);

    let error = app::retention(&config, &RetentionCommand::DryRun(args), &mut Vec::new()).unwrap_err();

    assert!(error.to_string().contains("parse rules file"), "{error}");
}

/// A sink that accepts the first line and then fails, standing in for a reader that hung up after the
/// header so a write mid-plan errors.
#[derive(Default)]
struct FailAfterFirstLine {
    past_first: bool,
}

impl std::io::Write for FailAfterFirstLine {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.past_first {
            return Err(std::io::Error::other("reader hung up"));
        }
        self.past_first = buf.contains(&b'\n');
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn test_retention_dry_run_propagates_a_write_failure() {
    let (_dir, config, _digest) = cache_fixture();
    seed_upload(&config, "pkg", "1.0");
    let mut out = FailAfterFirstLine::default();

    let error = app::retention(&config, &RetentionCommand::DryRun(dry_run_args("hosted")), &mut out).unwrap_err();

    assert!(error.to_string().contains("interrupted"), "{error}");
}

#[test]
fn test_retention_export_propagates_a_write_failure() {
    let (_dir, config, _digest) = cache_fixture();
    seed_upload(&config, "pkg", "1.0");
    let mut out = FailAfterFirstLine::default();

    let error = app::retention(
        &config,
        &RetentionCommand::Export(RetentionExportArgs {
            runtime: runtime_args(),
            index: "hosted".to_owned(),
            rules: None,
            cursor: None,
        }),
        &mut out,
    )
    .unwrap_err();

    assert!(error.to_string().contains("interrupted"), "{error}");
}

/// A sink that fails on the first write, standing in for a reader gone before the header lands.
struct AlwaysFail;

impl std::io::Write for AlwaysFail {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("reader hung up"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn test_retention_dry_run_propagates_a_header_write_failure() {
    let (_dir, config, _digest) = cache_fixture();
    seed_upload(&config, "pkg", "1.0");

    let error = app::retention(
        &config,
        &RetentionCommand::DryRun(dry_run_args("hosted")),
        &mut AlwaysFail,
    )
    .unwrap_err();

    assert!(!error.to_string().is_empty());
}

#[test]
fn test_retention_export_propagates_a_header_write_failure() {
    let (_dir, config, _digest) = cache_fixture();
    seed_upload(&config, "pkg", "1.0");

    let error = app::retention(
        &config,
        &RetentionCommand::Export(RetentionExportArgs {
            runtime: runtime_args(),
            index: "hosted".to_owned(),
            rules: None,
            cursor: None,
        }),
        &mut AlwaysFail,
    )
    .unwrap_err();

    assert!(!error.to_string().is_empty());
}

#[test]
fn test_retention_export_reports_an_unreadable_metadata_frontier() {
    let (_dir, config, _digest) = cache_fixture();
    seed_upload(&config, "pkg", "1.0");
    raw_insert_bytes(
        &config.data_dir.join("peryx.redb"),
        "policy_input_generation",
        "hosted",
        b"{ not json",
    );

    let error = app::retention(
        &config,
        &RetentionCommand::Export(RetentionExportArgs {
            runtime: runtime_args(),
            index: "hosted".to_owned(),
            rules: None,
            cursor: None,
        }),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert!(!error.to_string().is_empty());
}
