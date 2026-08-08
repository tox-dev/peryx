use std::path::Path;

use peryx_driver::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY;
use peryx_ecosystem_pypi::to_json;
use peryx_policy::PolicyConfig;
use wiremock::MockServer;

use crate::cli::{PrefetchCommand, PrefetchOptions, RuntimeArgs};
use crate::config::{Config, IndexConfig, IndexKind};

mod oci_tests;
mod pypi_tests;
mod selection_tests;

pub(super) fn mirror_config(data_dir: &Path, upstream: &str) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        indexes: vec![IndexConfig {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            policy: PolicyConfig::default(),
            ecosystem_policy: toml::Table::new(),
            ecosystem_settings: toml::Table::new(),
            webhooks: Vec::new(),
            ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
            anonymous_read: None,
            tokens: Vec::new(),
            kind: IndexKind::Cached {
                routing: crate::tests::single_route(upstream),
                upstream_concurrency: DEFAULT_UPSTREAM_CONCURRENCY,
                offline: false,
                prefetch: Box::default(),
            },
        }],
        ..Config::default()
    }
}

pub(super) fn overlay_config(data_dir: &Path, upstream: &str) -> Config {
    Config {
        data_dir: data_dir.to_path_buf(),
        indexes: vec![
            IndexConfig {
                name: "hosted".to_owned(),
                route: "hosted".to_owned(),
                policy: PolicyConfig::default(),
                ecosystem_policy: toml::Table::new(),
                ecosystem_settings: toml::Table::new(),
                webhooks: Vec::new(),
                ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
                anonymous_read: None,
                tokens: Vec::new(),
                kind: IndexKind::Hosted { volatile: true },
            },
            IndexConfig {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                policy: PolicyConfig::default(),
                ecosystem_policy: toml::Table::new(),
                ecosystem_settings: toml::Table::new(),
                webhooks: Vec::new(),
                ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
                anonymous_read: None,
                tokens: Vec::new(),
                kind: IndexKind::Cached {
                    routing: crate::tests::single_route(upstream),
                    upstream_concurrency: DEFAULT_UPSTREAM_CONCURRENCY,
                    offline: false,
                    prefetch: Box::default(),
                },
            },
            IndexConfig {
                name: "root/pypi".to_owned(),
                route: "root/pypi".to_owned(),
                policy: PolicyConfig::default(),
                ecosystem_policy: toml::Table::new(),
                ecosystem_settings: toml::Table::new(),
                webhooks: Vec::new(),
                ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
                anonymous_read: None,
                tokens: Vec::new(),
                kind: IndexKind::Virtual {
                    layers: vec!["hosted".to_owned(), "pypi".to_owned()],
                    upload: Some("hosted".to_owned()),
                },
            },
        ],
        ..Config::default()
    }
}

pub(super) fn command_options(data_dir: &Path, packages: Vec<String>) -> PrefetchOptions {
    PrefetchOptions {
        runtime: RuntimeArgs {
            config: None,
            host: None,
            port: None,
            data_dir: Some(data_dir.to_path_buf()),
            writer_identity: None,
            node_identity: None,
            offline: false,
            read_only: false,
            log_level: None,
            verbose: 0,
            log_format: None,
            log_sink: None,
            log_file: None,
        },
        index: "pypi".to_owned(),
        overrides: packages.is_empty().then(Vec::new).unwrap_or_else(|| {
            vec![format!(
                "packages={}",
                toml::Value::Array(packages.into_iter().map(toml::Value::String).collect())
            )]
        }),
    }
}

pub(super) fn set_option(options: &mut PrefetchOptions, key: &str, value: impl std::fmt::Display) {
    options.overrides.push(format!("{key}={value}"));
}

pub(super) fn file_entry(filename: &str, digest: impl Into<String>, size: usize) -> serde_json::Value {
    let digest = digest.into();
    serde_json::json!({
        "filename": filename,
        "url": format!("https://files.example/{filename}"),
        "hashes": {"sha256": digest},
        "size": size,
    })
}

pub(super) fn detail_page(name: &str, files: Vec<serde_json::Value>) -> Vec<u8> {
    let files = serde_json::Value::Array(files);
    to_json(&serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": name,
        "versions": ["1.0", "2.0"],
        "files": files,
    }))
    .into_bytes()
}

pub(super) fn mirror(dir: &tempfile::TempDir, server: &MockServer) -> Config {
    mirror_config(dir.path(), &format!("{}/simple/", server.uri()))
}

// `catalog::sync_catalog` coalesces concurrent syncs through a process-global lock keyed on the
// index name, so tests that sync the catalog need a distinct name to stay isolated under parallel
// runs; sharing one name lets an in-flight sync make another test's sync return `NotModified`.
pub(super) fn mirror_named(dir: &tempfile::TempDir, server: &MockServer, index: &str) -> Config {
    let mut config = mirror_config(dir.path(), &format!("{}/simple/", server.uri()));
    config.indexes[0].name = index.to_owned();
    config.indexes[0].route = index.to_owned();
    config
}

pub(super) async fn run_ok(config: &Config, command: &PrefetchCommand) -> String {
    let mut out = Vec::new();
    crate::prefetch::run(config, command, &mut out).await.unwrap();
    String::from_utf8(out).unwrap()
}

pub(super) async fn run_err(config: &Config, command: &PrefetchCommand) -> (String, anyhow::Error) {
    let mut out = Vec::new();
    let err = crate::prefetch::run(config, command, &mut out).await.unwrap_err();
    (String::from_utf8(out).unwrap(), err)
}
