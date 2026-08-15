use std::path::PathBuf;

use rstest::rstest;

use crate::config::{self, Config};
use crate::operator;
use crate::tests::support::{plugins, plugins_without_retention};

use super::support::s3_blob_config;

fn config(source: &str) -> Config {
    let plugins = plugins();
    Config::with_plugins(&plugins)
        .apply_with_plugins(config::from_toml(PathBuf::from("test.toml"), source).unwrap(), &plugins)
        .unwrap()
}

#[rstest]
#[case::hosted_name("target")]
#[case::hosted_route("local")]
#[case::virtual_write_target("aggregate")]
fn test_import_accepts_writable_selectors(#[case] selector: &str) {
    let root = tempfile::tempdir().unwrap();
    let import = root.path().join("import");
    std::fs::create_dir(&import).unwrap();
    let data = root.path().join("data");
    let source = format!(
        r#"
data_dir = {data:?}

[[index]]
name = "target"
route = "local"
hosted = true

[[index]]
name = "aggregate"
route = "all"
layers = ["target"]
write_target = "target"
"#
    );
    let config = config(&source);

    let mut out = Vec::new();

    operator::import_dir_with_plugins(&config, &plugins(), selector, &import, &mut out).unwrap();

    assert_eq!(
        out,
        b"status\tartifact\tresource\tgroup\treason\nsummary\t\t\t\timported=0 skipped=0 rejected=0\n"
    );
}

#[test]
fn test_import_rejects_a_missing_directory() {
    let root = tempfile::tempdir().unwrap();
    let missing_dir = root.path().join("missing");

    let error =
        operator::import_dir(&Config::with_plugins(&plugins()), "main", &missing_dir, &mut Vec::new()).unwrap_err();

    assert_eq!(
        error.to_string(),
        format!("import directory {} does not exist", missing_dir.display())
    );
}

#[rstest]
#[case::cached("cache", "index \"cache\" is read-only")]
#[case::virtual_without_write_target("aggregate", "index \"aggregate\" has no write target")]
#[case::unknown("unknown", "unknown index \"unknown\"")]
fn test_import_rejects_unwritable_selectors(#[case] selector: &str, #[case] expected: &str) {
    let root = tempfile::tempdir().unwrap();
    let source = format!(
        r#"
data_dir = {:?}

[[index]]
name = "cache"
route = "cache"

[[index.upstream]]
name = "origin"
url = "https://packages.example/catalog/"

[[index]]
name = "aggregate"
route = "all"
layers = ["cache"]
"#,
        root.path().join("data")
    );
    let config = config(&source);

    let error =
        operator::import_dir_with_plugins(&config, &plugins(), selector, root.path(), &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), expected);
}

#[test]
fn test_import_rejects_an_ecosystem_without_directory_import() {
    let root = tempfile::tempdir().unwrap();
    let plugins = plugins_without_retention();
    let config = Config {
        data_dir: root.path().join("data"),
        ..Config::with_plugins(&plugins)
    };
    let error =
        operator::import_dir_with_plugins(&config, &plugins, "plain", root.path(), &mut Vec::new()).unwrap_err();

    assert_eq!(error.to_string(), "no import driver for the plain ecosystem");
}

#[test]
fn test_import_rejects_object_storage_before_opening_metadata() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: root.path().join("data"),
        blob: s3_blob_config(),
        ..Config::with_plugins(&plugins())
    };

    let error =
        operator::import_dir_with_plugins(&config, &plugins(), "main", root.path(), &mut Vec::new()).unwrap_err();

    assert_eq!(
        (
            error.to_string().contains("filesystem-backed repository"),
            config.data_dir.join("peryx.redb").exists(),
        ),
        (true, false)
    );
}
