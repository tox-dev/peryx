use std::path::PathBuf;

use velodex_http::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RouteLimit};
use velodex_policy::PackageType;

use crate::config::{
    self, Config, IndexKind, LogConfig, LogFormat, LogSink, PartialConfig, PartialLogConfig, PrefetchMode,
    WebhookSecret,
};

fn toml_config(text: &str) -> Config {
    let partial = config::from_toml(PathBuf::from("x.toml"), text).unwrap();
    Config::default().apply(partial).unwrap()
}

#[test]
fn test_default_config() {
    let c = Config::default();
    assert_eq!(c.host, "127.0.0.1");
    assert_eq!(c.port, 4433);
    assert_eq!(c.data_dir, PathBuf::from("velodex-data"));
    assert!(!c.offline);
    assert_eq!(c.cache_ttl_secs, 300);
    assert_eq!(c.log, LogConfig::default());
    assert_eq!(c.rate_limit, RateLimitConfig::default());
    // A pypi mirror with a local store overlaid in front, served at root/pypi.
    let routes: Vec<&str> = c.indexes.iter().map(|index| index.route.as_str()).collect();
    assert_eq!(routes, ["pypi", "local", "root/pypi"]);
    assert!(matches!(&c.indexes[0].kind, IndexKind::Cached { .. }));
    assert!(matches!(&c.indexes[1].kind, IndexKind::Hosted { .. }));
    assert!(matches!(&c.indexes[2].kind, IndexKind::Virtual { upload: Some(target), .. } if target == "local"));
}

#[test]
fn test_apply_overlays_only_present_fields() {
    let merged = Config::default()
        .apply(PartialConfig {
            host: Some("0.0.0.0".to_owned()),
            port: Some(9000),
            offline: Some(true),
            cache_ttl_secs: Some(60),
            ..PartialConfig::default()
        })
        .unwrap();
    assert_eq!(merged.host, "0.0.0.0");
    assert_eq!(merged.port, 9000);
    assert!(merged.offline);
    assert_eq!(merged.cache_ttl_secs, 60);
    assert_eq!(merged.data_dir, PathBuf::from("velodex-data"));
    assert_eq!(merged.indexes.len(), 3); // untouched, so the defaults remain
}

#[test]
fn test_apply_data_dir_and_log() {
    let merged = Config::default()
        .apply(PartialConfig {
            data_dir: Some(PathBuf::from("/tmp/velodex")),
            log: PartialLogConfig {
                level: Some("debug".to_owned()),
                format: Some(LogFormat::Json),
                sink: Some(LogSink::File),
                file: Some(PathBuf::from("velodex.log")),
            },
            ..PartialConfig::default()
        })
        .unwrap();
    assert_eq!(merged.data_dir, PathBuf::from("/tmp/velodex"));
    assert_eq!(merged.log.level, "debug");
    assert_eq!(merged.log.format, LogFormat::Json);
    assert_eq!(merged.log.sink, LogSink::File);
    assert_eq!(merged.log.file, Some(PathBuf::from("velodex.log")));
}

#[test]
fn test_log_config_apply_empty_keeps_defaults() {
    let base = LogConfig::default();
    assert_eq!(base.clone().apply(PartialLogConfig::default()), base);
}

#[test]
fn test_mirror_prefetch_from_toml() {
    let c = toml_config(
        "\
offline = true
[[index]]
name = \"pypi\"
cached = \"https://pypi.org/simple/\"
offline = true

[index.prefetch]
mode = \"metadata-only\"
packages = [\"requests>=2,<3\"]
requirements = [\"requirements.txt\"]
include_wheels = false
include_sdists = true
python_tags = [\"py3\"]
abi_tags = [\"none\"]
platform_tags = [\"any\"]
max_file_size_bytes = 1048576
",
    );
    assert!(c.offline);
    let IndexKind::Cached { offline, prefetch, .. } = &c.indexes[0].kind else {
        panic!("expected mirror");
    };
    assert!(*offline);
    assert_eq!(prefetch.mode, PrefetchMode::MetadataOnly);
    assert_eq!(prefetch.packages, vec!["requests>=2,<3".to_owned()]);
    assert_eq!(prefetch.requirements, vec![PathBuf::from("requirements.txt")]);
    assert!(!prefetch.include_wheels);
    assert!(prefetch.include_sdists);
    assert_eq!(prefetch.python_tags, vec!["py3".to_owned()]);
    assert_eq!(prefetch.abi_tags, vec!["none".to_owned()]);
    assert_eq!(prefetch.platform_tags, vec!["any".to_owned()]);
    assert_eq!(prefetch.max_file_size_bytes, Some(1_048_576));
    assert!(prefetch.metadata_only);
}

#[test]
fn test_indexes_from_toml_classify_all_kinds() {
    let text = "\
[[index]]\nname = \"pypi\"\ncached = \"https://pypi.org/simple/\"\ntoken = \"bear\"\nupstream_concurrency = 3\n\
[[index]]\nname = \"corp\"\ncached = \"https://corp/simple/\"\nusername = \"u\"\npassword = \"p\"\n\
[[index]]\nname = \"team-local\"\nhosted = true\nupload_token = \"s\"\nvolatile = false\n\
[[index.webhook]]\nname = \"ci\"\nurl = \"https://ci.example/hook\"\nsecret_env = \"VELODEX_WEBHOOK_SECRET\"\nevents = [\"upload\", \"delete\"]\n\
[[index]]\nname = \"secret\"\nupload_token = \"z\"\n\
[[index]]\nname = \"team\"\nroute = \"team/dev\"\nlayers = [\"team-local\", \"pypi\"]\nupload = \"team-local\"\n";
    let c = toml_config(text);
    assert_eq!(c.indexes.len(), 5);
    assert_eq!(c.indexes[0].route, "pypi"); // route defaults to name
    assert!(
        matches!(&c.indexes[0].kind, IndexKind::Cached { token: Some(token), upstream_concurrency: 3, .. } if token == "bear")
    );
    assert!(matches!(
        &c.indexes[1].kind,
        IndexKind::Cached {
            username: Some(_),
            password: Some(_),
            token: None,
            ..
        }
    ));
    assert!(matches!(&c.indexes[2].kind, IndexKind::Hosted { volatile: false, .. })); // explicit local, non-volatile
    assert_eq!(c.indexes[2].webhooks.len(), 1);
    assert_eq!(c.indexes[2].webhooks[0].name, "ci");
    assert_eq!(c.indexes[2].webhooks[0].url, "https://ci.example/hook");
    assert_eq!(
        c.indexes[2].webhooks[0].secret,
        WebhookSecret::Env("VELODEX_WEBHOOK_SECRET".to_owned())
    );
    assert_eq!(c.indexes[2].webhooks[0].events, ["upload", "delete"]);
    assert!(matches!(&c.indexes[3].kind, IndexKind::Hosted { volatile: true, .. })); // upload_token implies local, default volatile
    assert_eq!(c.indexes[4].route, "team/dev");
    assert!(
        matches!(&c.indexes[4].kind, IndexKind::Virtual { layers, upload: Some(upload) }
            if layers == &["team-local".to_owned(), "pypi".to_owned()] && upload == "team-local")
    );
}

#[test]
fn test_rate_limits_from_toml_overlay_defaults() {
    let c = toml_config(
        "\
[rate_limit]\nenabled = true\nmax_clients = 32\n\
[rate_limit.simple]\nrequests = 10\nwindow_secs = 5\n\
[rate_limit.upload]\nrequests = 2\n",
    );

    assert!(c.rate_limit.enabled);
    assert_eq!(c.rate_limit.max_clients, 32);
    assert_eq!(c.rate_limit.simple, RouteLimit::new(10, 5));
    assert_eq!(c.rate_limit.upload.requests, 2);
    assert_eq!(
        c.rate_limit.upload.window_secs,
        RateLimitConfig::default().upload.window_secs
    );
    assert_eq!(c.rate_limit.artifact, RateLimitConfig::default().artifact);
}

#[test]
fn test_mirror_upstream_concurrency_defaults() {
    let c = toml_config("[[index]]\nname = \"pypi\"\ncached = \"https://pypi.org/simple/\"\n");
    assert!(matches!(
        &c.indexes[0].kind,
        IndexKind::Cached {
            upstream_concurrency: DEFAULT_UPSTREAM_CONCURRENCY,
            ..
        }
    ));
}

#[test]
fn test_index_policy_from_toml() {
    let text = "\
[[index]]\nname = \"pypi\"\ncached = \"https://pypi.org/simple/\"\n\
[index.policy]\nallow_projects = [\"Flask\"]\nblock_projects = [\"bad-pkg\"]\nallow_versions = \">=1,<2\"\n\
allow_package_types = [\"wheel\"]\nblock_package_types = [\"sdist\"]\n\
allow_wheel_pythons = [\"py3\"]\nblock_wheel_pythons = [\"py2\"]\n\
allow_wheel_platforms = [\"any\"]\nblock_wheel_platforms = [\"win_amd64\"]\n\
max_file_size_bytes = 1048576\nmax_project_size_bytes = 10485760\n";
    let config = toml_config(text);
    let policy = &config.indexes[0].policy;
    assert_eq!(policy.allow_projects, ["Flask"]);
    assert_eq!(policy.block_projects, ["bad-pkg"]);
    assert_eq!(policy.allow_versions.as_deref(), Some(">=1,<2"));
    assert_eq!(policy.allow_package_types, [PackageType::Wheel]);
    assert_eq!(policy.block_package_types, [PackageType::Sdist]);
    assert_eq!(policy.allow_wheel_pythons, ["py3"]);
    assert_eq!(policy.block_wheel_pythons, ["py2"]);
    assert_eq!(policy.allow_wheel_platforms, ["any"]);
    assert_eq!(policy.block_wheel_platforms, ["win_amd64"]);
    assert_eq!(policy.max_file_size_bytes, Some(1_048_576));
    assert_eq!(policy.max_project_size_bytes, Some(10_485_760));
}

#[test]
fn test_index_without_kind_is_error() {
    let partial = config::from_toml(PathBuf::from("x.toml"), "[[index]]\nname = \"bad\"\n").unwrap();
    let err = Config::default().apply(partial).unwrap_err();
    assert!(err.to_string().contains("bad"));
}

#[test]
fn test_index_webhook_accepts_literal_secret() {
    let text = "\
[[index]]\nname = \"local\"\nhosted = true\n\
[[index.webhook]]\nname = \"ci\"\nurl = \"https://ci.example/hook\"\nsecret = \"signing-secret\"\n";
    let c = toml_config(text);
    assert_eq!(
        c.indexes[0].webhooks[0].secret,
        WebhookSecret::Literal("signing-secret".to_owned())
    );
}

#[test]
fn test_index_webhook_rejects_ambiguous_secret_source() {
    let text = "\
[[index]]\nname = \"local\"\nhosted = true\n\
[[index.webhook]]\nname = \"ci\"\nurl = \"https://ci.example/hook\"\nsecret = \"s\"\nsecret_env = \"S\"\n";
    let partial = config::from_toml(PathBuf::from("x.toml"), text).unwrap();
    let err = Config::default().apply(partial).unwrap_err();
    assert!(err.to_string().contains("exactly one of `secret` or `secret_env`"));
}

#[test]
fn test_index_webhook_rejects_empty_name() {
    let text = "\
[[index]]\nname = \"local\"\nhosted = true\n\
[[index.webhook]]\nname = \"\"\nurl = \"https://ci.example/hook\"\nsecret = \"s\"\n";
    let partial = config::from_toml(PathBuf::from("x.toml"), text).unwrap();
    let err = Config::default().apply(partial).unwrap_err();
    assert!(err.to_string().contains("webhook name is required"));
}

#[test]
fn test_index_webhook_rejects_empty_url() {
    let text = "\
[[index]]\nname = \"local\"\nhosted = true\n\
[[index.webhook]]\nname = \"ci\"\nurl = \"\"\nsecret = \"s\"\n";
    let partial = config::from_toml(PathBuf::from("x.toml"), text).unwrap();
    let err = Config::default().apply(partial).unwrap_err();
    assert!(err.to_string().contains("webhook url is required"));
}

#[test]
fn test_from_toml_rejects_unknown_key() {
    let err = config::from_toml(PathBuf::from("bad.toml"), "bogus = 1").unwrap_err();
    assert!(err.to_string().contains("bad.toml"));
}

#[test]
fn test_from_toml_rejects_unknown_index_key() {
    assert!(config::from_toml(PathBuf::from("x.toml"), "[[index]]\nname = \"a\"\nbogus = 1\n").is_err());
}

#[test]
fn test_from_toml_rejects_unknown_policy_key() {
    let err = config::from_toml(
        PathBuf::from("x.toml"),
        "[[index]]\nname = \"pypi\"\ncached = \"https://pypi.org/simple/\"\n[index.policy]\nbogus = 1\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("bogus"));
}

#[test]
fn test_from_toml_rejects_unknown_log_key() {
    assert!(config::from_toml(PathBuf::from("x.toml"), "[log]\nbogus = 1\n").is_err());
}

#[test]
fn test_from_toml_rejects_unknown_rate_limit_key() {
    assert!(config::from_toml(PathBuf::from("x.toml"), "[rate_limit]\nbogus = 1\n").is_err());
}

#[test]
fn test_from_toml_rejects_invalid_log_format() {
    assert!(config::from_toml(PathBuf::from("x.toml"), "[log]\nformat = \"xml\"\n").is_err());
}

#[test]
fn test_from_toml_rejects_invalid_log_sink() {
    assert!(config::from_toml(PathBuf::from("x.toml"), "[log]\nsink = \"kafka\"\n").is_err());
}

#[test]
fn test_from_file_ok() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("velodex.toml");
    std::fs::write(&path, "port = 1234\n").unwrap();
    assert_eq!(config::from_file(path).unwrap().port, Some(1234));
}

#[test]
fn test_from_file_missing_errors() {
    let dir = tempfile::tempdir().unwrap();
    let err = config::from_file(dir.path().join("nope.toml")).unwrap_err();
    assert!(err.to_string().contains("nope.toml"));
}

#[test]
fn test_index_ecosystem_parses_and_defaults() {
    let c = toml_config("[[index]]\nname = \"pypi\"\ncached = \"https://pypi.org/simple/\"\necosystem = \"pypi\"\n");
    assert_eq!(c.indexes[0].ecosystem, velodex_format::Ecosystem::Pypi);
    let d = toml_config("[[index]]\nname = \"pypi\"\ncached = \"https://pypi.org/simple/\"\n");
    assert_eq!(d.indexes[0].ecosystem, velodex_format::Ecosystem::Pypi);
}

#[test]
fn test_unknown_ecosystem_is_rejected() {
    let partial = config::from_toml(
        PathBuf::from("x.toml"),
        "[[index]]\nname = \"pypi\"\ncached = \"https://pypi.org/simple/\"\necosystem = \"npm\"\n",
    )
    .unwrap();
    let err = Config::default().apply(partial).unwrap_err();
    assert!(err.to_string().contains("unknown ecosystem"), "{err}");
}

fn env_partial(pairs: &[(&str, &str)]) -> Result<PartialConfig, config::ConfigError> {
    let map: std::collections::HashMap<&str, &str> = pairs.iter().copied().collect();
    config::from_env_source(|var| map.get(var).map(|value| (*value).to_owned()))
}

#[test]
fn test_env_overlays_scalar_and_log_fields() {
    let partial = env_partial(&[
        ("VELODEX_HOST", "0.0.0.0"),
        ("VELODEX_PORT", "8080"),
        ("VELODEX_DATA_DIR", "/srv/velodex"),
        ("VELODEX_OFFLINE", "true"),
        ("VELODEX_CACHE_TTL_SECS", "42"),
        ("VELODEX_LOG_LEVEL", "debug"),
        ("VELODEX_LOG_FORMAT", "json"),
        ("VELODEX_LOG_SINK", "file"),
        ("VELODEX_LOG_FILE", "/var/log/velodex.log"),
    ])
    .unwrap();
    assert_eq!(partial.host.as_deref(), Some("0.0.0.0"));
    assert_eq!(partial.port, Some(8080));
    assert_eq!(partial.data_dir, Some(PathBuf::from("/srv/velodex")));
    assert_eq!(partial.offline, Some(true));
    assert_eq!(partial.cache_ttl_secs, Some(42));
    assert_eq!(partial.log.level.as_deref(), Some("debug"));
    assert_eq!(partial.log.format, Some(LogFormat::Json));
    assert_eq!(partial.log.sink, Some(LogSink::File));
    assert_eq!(partial.log.file, Some(PathBuf::from("/var/log/velodex.log")));
    assert_eq!(partial.indexes, None);
}

#[test]
fn test_env_absent_yields_empty_overlay() {
    assert_eq!(env_partial(&[]).unwrap(), PartialConfig::default());
}

#[test]
fn test_env_empty_string_is_unset() {
    let partial = env_partial(&[("VELODEX_HOST", ""), ("VELODEX_PORT", "")]).unwrap();
    assert_eq!(partial.host, None);
    assert_eq!(partial.port, None);
}

#[test]
fn test_env_invalid_port_is_rejected() {
    let err = env_partial(&[("VELODEX_PORT", "seventy")]).unwrap_err();
    assert!(err.to_string().contains("VELODEX_PORT"), "{err}");
}

#[test]
fn test_env_invalid_ttl_is_rejected() {
    let err = env_partial(&[("VELODEX_CACHE_TTL_SECS", "soon")]).unwrap_err();
    assert!(err.to_string().contains("VELODEX_CACHE_TTL_SECS"), "{err}");
}

#[test]
fn test_env_invalid_offline_is_rejected() {
    let err = env_partial(&[("VELODEX_OFFLINE", "maybe")]).unwrap_err();
    assert!(err.to_string().contains("VELODEX_OFFLINE"), "{err}");
}

#[test]
fn test_env_invalid_log_format_is_rejected() {
    let err = env_partial(&[("VELODEX_LOG_FORMAT", "xml")]).unwrap_err();
    assert!(err.to_string().contains("VELODEX_LOG_FORMAT"), "{err}");
}

#[test]
fn test_env_invalid_log_sink_is_rejected() {
    let err = env_partial(&[("VELODEX_LOG_SINK", "pigeon")]).unwrap_err();
    assert!(err.to_string().contains("VELODEX_LOG_SINK"), "{err}");
}

#[test]
fn test_env_sits_between_file_and_cli() {
    let resolved = Config::default()
        .apply(config::from_toml(PathBuf::from("x.toml"), "port = 1000\nhost = \"filehost\"\n").unwrap())
        .unwrap()
        .apply(env_partial(&[("VELODEX_PORT", "2000")]).unwrap())
        .unwrap()
        .apply(PartialConfig {
            port: Some(3000),
            ..PartialConfig::default()
        })
        .unwrap();
    assert_eq!(resolved.port, 3000);
    assert_eq!(resolved.host, "filehost");
}

#[test]
fn test_env_overrides_file_when_cli_is_silent() {
    let resolved = Config::default()
        .apply(config::from_toml(PathBuf::from("x.toml"), "port = 1000\n").unwrap())
        .unwrap()
        .apply(env_partial(&[("VELODEX_PORT", "2000")]).unwrap())
        .unwrap();
    assert_eq!(resolved.port, 2000);
}

#[test]
fn test_from_env_reads_process_environment() {
    // The process-reading wrapper delegates to the injectable source; with the current environment it
    // must parse without error (no test sets a malformed VELODEX_* variable).
    assert!(config::from_env().is_ok());
}
