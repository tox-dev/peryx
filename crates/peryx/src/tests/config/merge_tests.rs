use std::path::PathBuf;

use peryx_http::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RouteLimit};
use rstest::rstest;

use super::toml_config;
use crate::config::{
    self, AcmeConfig, Config, ConfigError, IndexKind, LogConfig, LogFormat, LogSink, PartialConfig, PartialLogConfig,
    TlsConfig, WebhookSecret,
};

fn toml_error(text: &str) -> ConfigError {
    let partial = config::from_toml(PathBuf::from("x.toml"), text).unwrap();
    Config::default().apply(partial).unwrap_err()
}

#[test]
fn test_tls_defaults_to_none() {
    assert_eq!(Config::default().tls, None);
    assert_eq!(toml_config("host = \"127.0.0.1\"").tls, None);
    // With neither table present the resolver yields no TLS; `apply` skips it, so exercise it directly.
    assert_eq!(config::classify_tls(None, None).unwrap(), None);
}

#[test]
fn test_tls_manual_cert_and_key() {
    let config = toml_config("[tls]\ncert = \"cert.pem\"\nkey = \"key.pem\"\n");
    assert_eq!(
        config.tls,
        Some(TlsConfig::Manual {
            cert: PathBuf::from("cert.pem"),
            key: PathBuf::from("key.pem"),
        })
    );
}

#[test]
fn test_tls_manual_requires_both_cert_and_key() {
    assert!(matches!(
        toml_error("[tls]\ncert = \"cert.pem\"\n"),
        ConfigError::Tls { reason } if reason.contains("cert` and `key")
    ));
}

#[test]
fn test_acme_defaults_cache_dir_and_production() {
    let config = toml_config("[acme]\ndomains = [\"registry.example.com\"]\ncontact = \"admin@example.com\"\n");
    assert_eq!(
        config.tls,
        Some(TlsConfig::Acme(AcmeConfig {
            domains: vec!["registry.example.com".to_owned()],
            contact: "admin@example.com".to_owned(),
            cache_dir: PathBuf::from("acme-cache"),
            staging: false,
        }))
    );
}

#[test]
fn test_acme_staging_and_cache_dir() {
    let config = toml_config(
        "[acme]\ndomains = [\"a.example\", \"b.example\"]\ncontact = \"ops@example.com\"\ncache-dir = \"/var/acme\"\nstaging = true\n",
    );
    let Some(TlsConfig::Acme(acme)) = config.tls else {
        panic!("expected acme config");
    };
    assert_eq!(acme.domains, ["a.example", "b.example"]);
    assert_eq!(acme.cache_dir, PathBuf::from("/var/acme"));
    assert!(acme.staging);
}

#[test]
fn test_acme_requires_a_domain() {
    assert!(matches!(
        toml_error("[acme]\ncontact = \"admin@example.com\"\n"),
        ConfigError::Tls { reason } if reason.contains("domain")
    ));
}

#[test]
fn test_acme_requires_a_contact() {
    assert!(matches!(
        toml_error("[acme]\ndomains = [\"registry.example.com\"]\n"),
        ConfigError::Tls { reason } if reason.contains("contact")
    ));
}

#[test]
fn test_tls_and_acme_are_mutually_exclusive() {
    assert!(matches!(
        toml_error("[tls]\ncert = \"c\"\nkey = \"k\"\n\n[acme]\ndomains = [\"x\"]\ncontact = \"a@b\"\n"),
        ConfigError::Tls { reason } if reason.contains("at most one")
    ));
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
    assert_eq!(merged.data_dir, PathBuf::from("peryx-data"));
    assert_eq!(merged.indexes.len(), 6); // untouched, so the defaults remain (PyPI trio + OCI trio)
}

#[test]
fn test_apply_data_dir_and_log() {
    let merged = Config::default()
        .apply(PartialConfig {
            data_dir: Some(PathBuf::from("/tmp/peryx")),
            log: PartialLogConfig {
                level: Some("debug".to_owned()),
                format: Some(LogFormat::Json),
                sink: Some(LogSink::File),
                file: Some(PathBuf::from("peryx.log")),
            },
            ..PartialConfig::default()
        })
        .unwrap();
    assert_eq!(merged.data_dir, PathBuf::from("/tmp/peryx"));
    assert_eq!(merged.log.level, "debug");
    assert_eq!(merged.log.format, LogFormat::Json);
    assert_eq!(merged.log.sink, LogSink::File);
    assert_eq!(merged.log.file, Some(PathBuf::from("peryx.log")));
}

#[test]
fn test_log_config_apply_empty_keeps_defaults() {
    let base = LogConfig::default();
    assert_eq!(base.clone().apply(PartialLogConfig::default()), base);
}

#[test]
fn test_indexes_from_toml_classify_all_kinds() {
    let text = "\
[[index]]\nname = \"pypi\"\ncached = \"https://pypi.org/simple/\"\ntoken = \"bear\"\nupstream_concurrency = 3\n\
[[index]]\nname = \"corp\"\ncached = \"https://corp/simple/\"\nusername = \"u\"\npassword = \"p\"\n\
[[index]]\nname = \"team-hosted\"\nhosted = true\nupload_token = \"s\"\nvolatile = false\n\
[[index.webhook]]\nname = \"ci\"\nurl = \"https://ci.example/hook\"\nsecret_env = \"PERYX_WEBHOOK_SECRET\"\nevents = [\"upload\", \"delete\"]\n\
[[index]]\nname = \"secret\"\nupload_token = \"z\"\n\
[[index]]\nname = \"team\"\nroute = \"team/dev\"\nlayers = [\"team-hosted\", \"pypi\"]\nupload = \"team-hosted\"\n";
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
    assert!(matches!(&c.indexes[2].kind, IndexKind::Hosted { volatile: false, .. })); // explicit hosted, non-volatile
    assert_eq!(c.indexes[2].webhooks.len(), 1);
    assert_eq!(c.indexes[2].webhooks[0].name, "ci");
    assert_eq!(c.indexes[2].webhooks[0].url, "https://ci.example/hook");
    assert_eq!(
        c.indexes[2].webhooks[0].secret,
        WebhookSecret::Env("PERYX_WEBHOOK_SECRET".to_owned())
    );
    assert_eq!(c.indexes[2].webhooks[0].events, ["upload", "delete"]);
    assert!(matches!(&c.indexes[3].kind, IndexKind::Hosted { volatile: true, .. })); // upload_token implies hosted, default volatile
    assert_eq!(c.indexes[4].route, "team/dev");
    assert!(
        matches!(&c.indexes[4].kind, IndexKind::Virtual { layers, upload: Some(upload) }
            if layers == &["team-hosted".to_owned(), "pypi".to_owned()] && upload == "team-hosted")
    );
}

#[test]
fn test_rate_limits_from_toml_overlay_defaults() {
    let c = toml_config(
        "\
[rate_limit]\nenabled = true\nmax_clients = 32\n\
[rate_limit.listing]\nrequests = 10\nwindow_secs = 5\n\
[rate_limit.upload]\nrequests = 2\n",
    );

    assert!(c.rate_limit.enabled);
    assert_eq!(c.rate_limit.max_clients, 32);
    assert_eq!(c.rate_limit.listing, RouteLimit::new(10, 5));
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
fn test_index_without_kind_is_error() {
    let partial = config::from_toml(PathBuf::from("x.toml"), "[[index]]\nname = \"bad\"\n").unwrap();
    let err = Config::default().apply(partial).unwrap_err();
    assert!(err.to_string().contains("bad"));
}

#[test]
fn test_index_webhook_accepts_literal_secret() {
    let text = "\
[[index]]\nname = \"hosted\"\nhosted = true\n\
[[index.webhook]]\nname = \"ci\"\nurl = \"https://ci.example/hook\"\nsecret = \"signing-secret\"\n";
    let c = toml_config(text);
    assert_eq!(
        c.indexes[0].webhooks[0].secret,
        WebhookSecret::Literal("signing-secret".to_owned())
    );
}

#[rstest]
#[case::ambiguous_secret_source(
    "[[index]]\nname = \"hosted\"\nhosted = true\n\
     [[index.webhook]]\nname = \"ci\"\nurl = \"https://ci.example/hook\"\nsecret = \"s\"\nsecret_env = \"S\"\n",
    "exactly one of `secret` or `secret_env`"
)]
#[case::empty_name(
    "[[index]]\nname = \"hosted\"\nhosted = true\n\
     [[index.webhook]]\nname = \"\"\nurl = \"https://ci.example/hook\"\nsecret = \"s\"\n",
    "webhook name is required"
)]
#[case::empty_url(
    "[[index]]\nname = \"hosted\"\nhosted = true\n\
     [[index.webhook]]\nname = \"ci\"\nurl = \"\"\nsecret = \"s\"\n",
    "webhook url is required"
)]
fn test_index_webhook_rejects(#[case] text: &str, #[case] expected: &str) {
    assert!(toml_error(text).to_string().contains(expected));
}

#[test]
fn test_index_ecosystem_parses_and_defaults() {
    let c = toml_config("[[index]]\nname = \"pypi\"\ncached = \"https://pypi.org/simple/\"\necosystem = \"pypi\"\n");
    assert_eq!(c.indexes[0].ecosystem, peryx_format::Ecosystem::Pypi);
    let d = toml_config("[[index]]\nname = \"pypi\"\ncached = \"https://pypi.org/simple/\"\n");
    assert_eq!(d.indexes[0].ecosystem, peryx_format::Ecosystem::Pypi);
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
