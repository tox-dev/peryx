//! Config snapshot serialization for backup manifests.

use serde::Serialize;

use crate::config::{Config, IndexKind, LogFormat, LogSink, SecretSource};

#[derive(Serialize)]
struct SnapshotConfig {
    host: String,
    port: u16,
    data_dir: String,
    cache_ttl_secs: i64,
    index: Vec<SnapshotIndex>,
    log: SnapshotLog,
}

#[derive(Serialize)]
struct SnapshotIndex {
    name: String,
    route: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_concurrency: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hosted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_token_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volatile: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    layers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload: Option<String>,
}

#[derive(Serialize)]
struct SnapshotLog {
    level: String,
    format: &'static str,
    sink: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
}

pub(super) fn config_snapshot(config: &Config) -> anyhow::Result<String> {
    let snapshot = SnapshotConfig {
        host: config.host.clone(),
        port: config.port,
        data_dir: config.data_dir.display().to_string(),
        cache_ttl_secs: config.cache_ttl_secs,
        index: config.indexes.iter().map(snapshot_index).collect(),
        log: SnapshotLog {
            level: config.log.level.clone(),
            format: log_format(config.log.format),
            sink: log_sink(config.log.sink),
            file: config.log.file.as_ref().map(|path| path.display().to_string()),
        },
    };
    Ok(toml::to_string_pretty(&snapshot)?)
}

fn snapshot_index(index: &crate::config::IndexConfig) -> SnapshotIndex {
    match &index.kind {
        IndexKind::Cached {
            upstream,
            username,
            password,
            token,
            upstream_concurrency,
            ..
        } => SnapshotIndex {
            name: index.name.clone(),
            route: index.route.clone(),
            cached: Some(upstream.clone()),
            username: username.clone(),
            password: password.clone(),
            token: token.clone(),
            upstream_concurrency: Some(*upstream_concurrency),
            hosted: None,
            upload_token: None,
            upload_token_file: None,
            volatile: None,
            layers: None,
            upload: None,
        },
        IndexKind::Hosted { upload_token, volatile } => SnapshotIndex {
            name: index.name.clone(),
            route: index.route.clone(),
            cached: None,
            username: None,
            password: None,
            token: None,
            upstream_concurrency: None,
            hosted: Some(true),
            upload_token: literal_secret(upload_token.as_ref()),
            upload_token_file: secret_file(upload_token.as_ref()),
            volatile: Some(*volatile),
            layers: None,
            upload: None,
        },
        IndexKind::Virtual { layers, upload } => SnapshotIndex {
            name: index.name.clone(),
            route: index.route.clone(),
            cached: None,
            username: None,
            password: None,
            token: None,
            upstream_concurrency: None,
            hosted: None,
            upload_token: None,
            upload_token_file: None,
            volatile: None,
            layers: Some(layers.clone()),
            upload: upload.clone(),
        },
    }
}

/// A snapshot records where a secret lives, not what it is: a secret kept in a file stays in its file.
fn literal_secret(source: Option<&SecretSource>) -> Option<String> {
    match source? {
        SecretSource::Literal(secret) => Some(secret.clone()),
        SecretSource::File(_) => None,
    }
}

fn secret_file(source: Option<&SecretSource>) -> Option<String> {
    match source? {
        SecretSource::Literal(_) => None,
        SecretSource::File(path) => Some(path.display().to_string()),
    }
}

const fn log_format(format: LogFormat) -> &'static str {
    match format {
        LogFormat::Pretty => "pretty",
        LogFormat::Json => "json",
    }
}

const fn log_sink(sink: LogSink) -> &'static str {
    match sink {
        LogSink::Stdout => "stdout",
        LogSink::File => "file",
        LogSink::Journald => "journald",
        LogSink::Syslog => "syslog",
    }
}
