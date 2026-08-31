use std::collections::BTreeMap;

use peryx_ecosystem_pypi::upload::Uploaded;
use peryx_ecosystem_pypi::{CoreMetadata, File, Provenance, Yanked};

use super::*;
use crate::app;
use crate::cli::{RetentionCommand, RetentionDryRunArgs, RetentionExportArgs};

const RETENTION_HEADER: &str = "action\tresource\tgroup\tartifact\tdigest\tclass\tvisibility\tbytes\trule\n";

fn seed_upload(config: &Config, project: &str, version: &str) -> Digest {
    let meta = MetaStore::open(config.data_dir.join("peryx.redb")).unwrap();
    let filename = format!("{project}-{version}.whl");
    let digest = Digest::of(filename.as_bytes());
    let record = serde_json::to_vec(&Uploaded {
        version: version.to_owned(),
        file: File {
            filename: filename.clone(),
            url: format!("http://localhost/files/{filename}"),
            hashes: BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
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
    digest
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
    let one = seed_upload(&config, "pkg", "1.0");
    let two = seed_upload(&config, "pkg", "2.0");

    let text = dry_run(&config, dry_run_args("hosted"));

    assert!(text.starts_with(RETENTION_HEADER));
    assert!(
        text.contains(&format!(
            "retain\tpkg\t2.0\tpkg-2.0.whl\t{}\thosted\tactive\t1024\t\n",
            two.as_str()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "retain\tpkg\t1.0\tpkg-1.0.whl\t{}\thosted\tactive\t1024\t\n",
            one.as_str()
        )),
        "{text}"
    );
    assert!(text.contains("summary\tpolicy_version="), "{text}");
}

#[test]
fn test_retention_dry_run_applies_rules_from_a_file() {
    let (dir, config, _digest) = cache_fixture();
    let one = seed_upload(&config, "pkg", "1.0");
    seed_upload(&config, "pkg", "2.0");
    let rules = rules_file(
        dir.path(),
        "[[keep]]\nselector = \"keep-latest-groups\"\ncount = 1\n[[expire]]\nselector = \"resource-prefix\"\nprefix = \"\"\n",
    );
    let mut args = dry_run_args("hosted");
    args.rules = Some(rules);

    let text = dry_run(&config, args);

    assert!(text.contains("retain\tpkg\t2.0\tpkg-2.0.whl"), "{text}");
    assert!(
        text.contains(&format!(
            "remove\tpkg\t1.0\tpkg-1.0.whl\t{}\thosted\tactive\t1024\tresource-prefix\n",
            one.as_str()
        )),
        "{text}"
    );
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
