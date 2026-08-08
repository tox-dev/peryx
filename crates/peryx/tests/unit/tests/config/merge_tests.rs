use std::path::{Path, PathBuf};
use std::time::Duration;

use peryx_driver::rate_limit::{DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RouteLimit};
use peryx_upstream::CredentialFailure;
use rstest::rstest;

use super::toml_config;
use crate::config::{
    self, AcmeConfig, Config, ConfigError, CredentialFailureMode, IndexKind, LogConfig, LogFormat, LogSink,
    PartialConfig, PartialLogConfig, SecretSource, TlsConfig, WebhookSecret,
};

fn toml_error(text: &str) -> ConfigError {
    let partial = config::from_toml(PathBuf::from("x.toml"), text).unwrap();
    Config::default().apply(partial).unwrap_err()
}

#[cfg(windows)]
const fn exec_path() -> &'static str {
    r"C:\credential-helper.exe"
}

#[cfg(not(windows))]
const fn exec_path() -> &'static str {
    "/credential-helper"
}

fn exec_argv() -> String {
    format!("argv = [{:?}]", exec_path())
}

#[test]
fn test_tls_defaults_to_none() {
    assert_eq!(Config::default().tls, None);
    assert_eq!(toml_config("host = \"127.0.0.1\"").tls, None);
    // With neither table present the resolver yields no TLS; `apply` skips it, so exercise it directly.
    assert_eq!(super::super::merge::classify_tls(None, None).unwrap(), None);
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
            writer_identity: Some("writer-a".to_owned()),
            node_identity: Some("node-b".to_owned()),
            offline: Some(true),
            read_only: Some(true),
            cache_ttl_secs: Some(60),
            hot_cache_bytes: Some(1_048_576),
            max_stale_secs: Some(30),
            usage_retention_days: Some(90),
            ..PartialConfig::default()
        })
        .unwrap();
    assert_eq!(merged.host, "0.0.0.0");
    assert_eq!(merged.port, 9000);
    assert_eq!(merged.writer_identity.as_deref(), Some("writer-a"));
    assert_eq!(merged.node_identity.as_deref(), Some("node-b"));
    assert!(merged.offline);
    assert!(merged.read_only);
    assert_eq!(merged.cache_ttl_secs, 60);
    assert_eq!(merged.hot_cache_bytes, 1_048_576);
    assert_eq!(merged.max_stale_secs, 30);
    assert_eq!(merged.usage_retention_days, Some(90));
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
[[index]]\nname = \"pypi\"\nupstream_concurrency = 3\n\
[[index.upstream]]\nname = \"primary\"\nurl = \"https://pypi.org/simple/\"\ntoken = \"bear\"\n\
[[index]]\nname = \"corp\"\n\
[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\nusername = \"u\"\npassword = \"p\"\n\
[[index]]\nname = \"team-hosted\"\nhosted = true\nvolatile = false\n\
[[index.webhook]]\nname = \"ci\"\nurl = \"https://ci.example/hook\"\nsecret_env = \"PERYX_WEBHOOK_SECRET\"\nevents = [\"upload\", \"delete\"]\n\
[[index]]\nname = \"secret\"\nhosted = true\n\
[[index]]\nname = \"team\"\nroute = \"team/dev\"\nlayers = [\"team-hosted\", \"pypi\"]\nupload = \"team-hosted\"\n";
    let c = toml_config(text);
    assert_eq!(c.indexes.len(), 5);
    assert_eq!(c.indexes[0].route, "pypi"); // route defaults to name
    assert!(matches!(
        &c.indexes[0].kind,
        IndexKind::Cached { upstream_concurrency: 3, routing, .. }
            if matches!(&routing.upstreams[0].token, Some(SecretSource::Literal(token)) if token == "bear")
    ));
    assert!(matches!(
        &c.indexes[1].kind,
        IndexKind::Cached { routing, .. }
            if routing.upstreams[0].username.is_some()
                && routing.upstreams[0].password.is_some()
                && routing.upstreams[0].token.is_none()
    ));
    assert!(matches!(&c.indexes[2].kind, IndexKind::Hosted { volatile: false })); // explicit hosted, non-volatile
    assert_eq!(c.indexes[2].webhooks.len(), 1);
    assert_eq!(c.indexes[2].webhooks[0].name, "ci");
    assert_eq!(c.indexes[2].webhooks[0].url, "https://ci.example/hook");
    assert_eq!(
        c.indexes[2].webhooks[0].secret,
        WebhookSecret::Env("PERYX_WEBHOOK_SECRET".to_owned())
    );
    assert_eq!(c.indexes[2].webhooks[0].events, ["upload", "delete"]);
    assert!(matches!(&c.indexes[3].kind, IndexKind::Hosted { volatile: true })); // hosted defaults to volatile
    assert_eq!(c.indexes[4].route, "team/dev");
    assert!(
        matches!(&c.indexes[4].kind, IndexKind::Virtual { layers, upload: Some(upload) }
            if layers == &["team-hosted".to_owned(), "pypi".to_owned()] && upload == "team-hosted")
    );
}

#[test]
fn test_netrc_path_overlays_the_default() {
    let config = toml_config("netrc = \"/run/secrets/upstream.netrc\"\n");
    assert_eq!(config.netrc, Some(PathBuf::from("/run/secrets/upstream.netrc")));
}

#[test]
fn test_rate_limits_from_toml_overlay_defaults() {
    let c = toml_config(
        "\
[rate_limit]\nenabled = true\nmax_clients = 32\ntrusted_proxies = [\"127.0.0.1/32\", \"2001:db8::/32\"]\n\
[rate_limit.listing]\nrequests = 10\nwindow_secs = 5\n\
[rate_limit.upload]\nrequests = 2\n",
    );

    assert!(c.rate_limit.enabled);
    assert_eq!(c.rate_limit.max_clients, 32);
    assert_eq!(
        c.rate_limit.trusted_proxies,
        ["127.0.0.1/32".parse().unwrap(), "2001:db8::/32".parse().unwrap()]
    );
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
    let c = toml_config(
        "[[index]]\nname = \"pypi\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://pypi.org/simple/\"\n",
    );
    assert!(matches!(
        &c.indexes[0].kind,
        IndexKind::Cached {
            upstream_concurrency: DEFAULT_UPSTREAM_CONCURRENCY,
            ..
        }
    ));
}

#[test]
fn test_ordered_upstreams_resolve_routing_and_credentials() {
    let c = toml_config(
        r#"
[[index]]
name = "pypi"
fallback = false
protected = ["Internal.Pkg"]

[index.pins]
flask = "public"

[[index.upstream]]
name = "internal"
url = "https://packages.example/simple/"
artifact_url = "https://artifacts.example/packages/"
username = "reader"
password_file = "/run/secrets/internal-password"

[[index.upstream]]
name = "public"
url = "https://pypi.org/simple/"
token = "bearer"
"#,
    );
    let IndexKind::Cached { routing, .. } = &c.indexes[0].kind else {
        panic!("expected a routed cached index");
    };
    let primary = &routing.upstreams[0];
    assert_eq!(
        (primary.url.as_str(), primary.username.as_deref(), &primary.password),
        (
            "https://packages.example/simple/",
            Some("reader"),
            &Some(SecretSource::File(PathBuf::from("/run/secrets/internal-password")))
        )
    );
    assert!(!routing.fallback);
    assert_eq!(
        routing.upstreams[0].artifact_url.as_deref(),
        Some("https://artifacts.example/packages/")
    );
    assert_eq!(routing.protected, ["Internal.Pkg"]);
    assert_eq!(routing.pins.get("flask").map(String::as_str), Some("public"));
    assert_eq!(
        routing
            .upstreams
            .iter()
            .map(|upstream| upstream.name.as_str())
            .collect::<Vec<_>>(),
        ["internal", "public"]
    );
    assert!(matches!(
        &routing.upstreams[1].token,
        Some(SecretSource::Literal(token)) if token == "bearer"
    ));
}

#[test]
fn test_upstream_tls_paths_resolve_for_single_and_multi_source_routes() {
    let config = toml_config(
        r#"
[[index]]
name = "single"
[[index.upstream]]
name = "primary"
url = "https://single.example/simple/"
ca_file = "/run/tls/single-ca.pem"
client_cert_file = "/run/tls/single-cert.pem"
client_key_file = "/run/tls/single-key.pem"

[[index]]
name = "routed"
[[index.upstream]]
name = "primary"
url = "https://primary.example/simple/"
ca_file = "/run/tls/primary-ca.pem"
"#,
    );
    let IndexKind::Cached { routing, .. } = &config.indexes[0].kind else {
        panic!("expected cached index");
    };
    let tls = &routing.upstreams[0].tls;
    assert_eq!(tls.ca_file.as_deref(), Some(Path::new("/run/tls/single-ca.pem")));
    assert_eq!(
        tls.client_cert_file.as_deref(),
        Some(Path::new("/run/tls/single-cert.pem"))
    );
    assert_eq!(
        tls.client_key_file.as_deref(),
        Some(Path::new("/run/tls/single-key.pem"))
    );
    let IndexKind::Cached { routing, .. } = &config.indexes[1].kind else {
        panic!("expected routed cached index");
    };
    assert_eq!(
        routing.upstreams[0].tls.ca_file.as_deref(),
        Some(Path::new("/run/tls/primary-ca.pem"))
    );
    assert_eq!(
        format!("{:?}", routing.upstreams[0].tls),
        "UpstreamTlsConfig { custom_ca: true, client_identity: false }"
    );
}

#[rstest]
#[case::certificate_only(
    "[[index]]\nname = \"pypi\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://example/simple/\"\nclient_cert_file = \"cert.pem\"\n"
)]
#[case::key_only(
    "[[index]]\nname = \"pypi\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://example/simple/\"\nclient_key_file = \"key.pem\"\n"
)]
fn test_upstream_client_certificate_and_key_are_a_pair(#[case] text: &str) {
    assert_eq!(
        toml_error(text).to_string(),
        "index pypi: `client_cert_file` and `client_key_file` must be configured together"
    );
}

#[test]
fn test_routing_options_require_upstream_sources() {
    assert_eq!(
        toml_error("[[index]]\nname = \"pypi\"\nfallback = false\n").to_string(),
        "index pypi: `fallback`, `protected`, and `pins` require `[[index.upstream]]`"
    );
}

#[rstest]
#[case::cached("cached = \"https://pypi.org/simple/\"", "cached")]
#[case::upload_token("hosted = true\nupload_token = \"s\"", "upload_token")]
#[case::index_token(
    "token = \"x\"\n[[index.upstream]]\nname = \"public\"\nurl = \"https://pypi.org/simple/\"",
    "token"
)]
#[case::index_ca_file(
    "ca_file = \"ca.pem\"\n[[index.upstream]]\nname = \"public\"\nurl = \"https://pypi.org/simple/\"",
    "ca_file"
)]
fn test_removed_shorthand_keys_are_unknown_fields(#[case] body: &str, #[case] key: &str) {
    let error = config::from_toml(
        PathBuf::from("x.toml"),
        &format!("[[index]]\nname = \"pypi\"\n{body}\n"),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains(&format!("unknown field `{key}`")), "{error}");
}

#[test]
fn test_upstream_password_and_token_read_from_files() {
    let c = toml_config(
        "[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n\
         password_file = \"/run/secrets/pw\"\ntoken_file = \"/run/secrets/tok\"\n",
    );
    assert!(matches!(
        &c.indexes[0].kind,
        IndexKind::Cached { routing, .. }
            if matches!(&routing.upstreams[0].password, Some(SecretSource::File(pw)) if pw == Path::new("/run/secrets/pw"))
                && matches!(&routing.upstreams[0].token, Some(SecretSource::File(tok)) if tok == Path::new("/run/secrets/tok"))
    ));
}

#[test]
fn test_upstream_password_and_token_read_from_env() {
    let c = toml_config(
        "[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n\
         password_env = \"CORP_PASSWORD\"\ntoken_env = \"CORP_TOKEN\"\n",
    );
    assert!(matches!(
        &c.indexes[0].kind,
        IndexKind::Cached { routing, .. }
            if matches!(&routing.upstreams[0].password, Some(SecretSource::Env(pw)) if pw == "CORP_PASSWORD")
                && matches!(&routing.upstreams[0].token, Some(SecretSource::Env(tok)) if tok == "CORP_TOKEN")
    ));
}

#[test]
fn test_ordered_upstream_password_reads_from_env() {
    let c = toml_config(
        "[[index]]\nname = \"corp\"\n\
         [[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\npassword_env = \"CORP_PASSWORD\"\n",
    );
    let IndexKind::Cached { routing, .. } = &c.indexes[0].kind else {
        panic!("expected a routed cached index");
    };
    assert!(matches!(
        &routing.upstreams[0].password,
        Some(SecretSource::Env(var)) if var == "CORP_PASSWORD"
    ));
}

#[test]
fn test_upstream_credential_refresh_resolves_for_file_sources() {
    let config = toml_config(
        "[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n\
         token_file = \"/run/secrets/token\"\ncredential_refresh_secs = 30\n\
         credential_refresh_on_unauthorized = false\ncredential_failure = \"anonymous\"\n",
    );
    let IndexKind::Cached { routing, .. } = &config.indexes[0].kind else {
        panic!("expected cached index");
    };
    let Some(refresh) = routing.upstreams[0].credential_refresh else {
        panic!("expected credential refresh");
    };

    assert_eq!(
        (refresh.interval, refresh.on_unauthorized, refresh.failure),
        (Duration::from_secs(30), false, CredentialFailureMode::Anonymous)
    );
}

#[test]
fn test_routed_credential_refresh_applies_its_defaults() {
    let config = toml_config(
        "[[index]]\nname = \"corp\"\n\
         [[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n\
         token_env = \"CORP_TOKEN\"\ncredential_refresh_secs = 60\n",
    );
    let IndexKind::Cached { routing, .. } = &config.indexes[0].kind else {
        panic!("expected cached index");
    };
    let refresh = routing.upstreams[0].credential_refresh.expect("credential refresh");
    assert_eq!(
        (refresh.on_unauthorized, refresh.failure),
        (true, CredentialFailureMode::Fail)
    );
}

#[test]
fn test_exec_credential_resolves_for_a_cached_index() {
    let config = toml_config(&format!(
        "[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n\
         [index.upstream.credential_exec]\nargv = [{:?}, \"--profile\", \"production\"]\n\
         timeout_secs = 12\nenvironment = [\"HOME\", \"AWS_PROFILE\"]\nfailure = \"anonymous\"\n",
        exec_path()
    ));
    let IndexKind::Cached { routing, .. } = &config.indexes[0].kind else {
        panic!("expected cached index");
    };
    let Some(exec) = &routing.upstreams[0].credential_exec else {
        panic!("expected an exec credential");
    };

    assert_eq!(
        (exec.argv(), exec.timeout(), exec.environment(), exec.failure()),
        (
            &[exec_path().to_owned(), "--profile".to_owned(), "production".to_owned(),][..],
            Duration::from_secs(12),
            &["HOME".to_owned(), "AWS_PROFILE".to_owned()][..],
            CredentialFailure::Anonymous,
        )
    );
}

#[test]
fn test_routed_exec_credential_applies_its_defaults() {
    let config = toml_config(&format!(
        "[[index]]\nname = \"corp\"\n\
         [[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n\
         [index.upstream.credential_exec]\n{}\n",
        exec_argv()
    ));
    let IndexKind::Cached { routing, .. } = &config.indexes[0].kind else {
        panic!("expected cached index");
    };
    let primary = routing.upstreams[0].credential_exec.as_ref().expect("exec credential");
    assert_eq!(
        (primary.timeout(), primary.environment(), primary.failure()),
        (Duration::from_secs(30), &[][..], CredentialFailure::Fail)
    );
}

#[rstest]
#[case::empty_argv("argv = []", "`credential_exec.argv` must not be empty")]
#[case::relative_argv(
    "argv = [\"helper\"]",
    "`credential_exec.argv` must start with an absolute executable path"
)]
#[case::nul_argv("argv = [\"/helper\\u0000argument\"]", "`credential_exec.argv` contains a null byte")]
#[case::zero_timeout("timeout_secs = 0", "`credential_exec.timeout_secs` must be between 1 and 300")]
#[case::long_timeout("timeout_secs = 301", "`credential_exec.timeout_secs` must be between 1 and 300")]
#[case::invalid_environment("environment = [\"A=B\"]", "`credential_exec.environment` contains an invalid name")]
fn test_exec_credential_rejects_invalid_settings(#[case] settings: &str, #[case] reason: &str) {
    let settings = if settings.starts_with("argv") {
        settings.to_owned()
    } else {
        format!("{}\n{settings}", exec_argv())
    };
    let text = format!(
        "[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n[index.upstream.credential_exec]\n{settings}\n"
    );

    assert_eq!(toml_error(&text).to_string(), format!("index corp: {reason}"));
}

#[test]
fn test_exec_credential_bounds_argv_items() {
    let argv = std::iter::repeat_n(format!("{:?}", exec_path()), 65)
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!(
        "[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n[index.upstream.credential_exec]\nargv = [{argv}]\n"
    );

    assert_eq!(
        toml_error(&text).to_string(),
        "index corp: `credential_exec.argv` exceeds its item or byte limit"
    );
}

#[test]
fn test_exec_credential_bounds_environment_items() {
    let environment = std::iter::repeat_n("\"NAME\"", 65).collect::<Vec<_>>().join(", ");
    let text = format!(
        "[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n\
         [index.upstream.credential_exec]\n{}\nenvironment = [{environment}]\n",
        exec_argv()
    );

    assert_eq!(
        toml_error(&text).to_string(),
        "index corp: `credential_exec.environment` exceeds its item limit"
    );
}

#[rstest]
#[case::username("username = \"service\"")]
#[case::password("password_file = \"/run/secret\"")]
#[case::token("token_env = \"TOKEN\"")]
fn test_exec_credential_rejects_static_credentials(#[case] credential: &str) {
    let text = format!(
        "[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n{credential}\n\
         [index.upstream.credential_exec]\n{}\n",
        exec_argv()
    );

    assert_eq!(
        toml_error(&text).to_string(),
        "index corp: `credential_exec` is mutually exclusive with username, password, and token settings"
    );
}

#[test]
fn test_exec_credential_rejects_refresh_controls() {
    let error = toml_error(&format!(
        "[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\ncredential_refresh_secs = 30\n\
         [index.upstream.credential_exec]\n{}\n",
        exec_argv()
    ));

    assert_eq!(
        error.to_string(),
        "index corp: `credential_exec` controls its own expiry and failure behavior"
    );
}

#[rstest]
#[case::zero_interval(
    "token_file = \"token\"\ncredential_refresh_secs = 0\n",
    "`credential_refresh_secs` must be positive"
)]
#[case::literal_source(
    "token = \"secret\"\ncredential_refresh_secs = 30\n",
    "credential refresh requires `token_file`/`token_env` or `username` with `password_file`/`password_env`"
)]
#[case::password_without_username(
    "password_file = \"password\"\ncredential_refresh_secs = 30\n",
    "credential refresh requires `token_file`/`token_env` or `username` with `password_file`/`password_env`"
)]
#[case::missing_interval(
    "token_file = \"token\"\ncredential_failure = \"anonymous\"\n",
    "`credential_refresh_secs` is required for credential refresh controls"
)]
fn test_credential_refresh_rejects_invalid_controls(#[case] controls: &str, #[case] reason: &str) {
    let text = format!(
        "[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n{controls}"
    );
    assert_eq!(toml_error(&text).to_string(), format!("index corp: {reason}"));
}

#[test]
fn test_routed_credential_refresh_rejects_invalid_controls() {
    assert_eq!(
        toml_error(
            "[[index]]\nname = \"corp\"\n\
             [[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n\
             token_file = \"token\"\ncredential_failure = \"anonymous\"\n",
        )
        .to_string(),
        "index corp: `credential_refresh_secs` is required for credential refresh controls"
    );
}

#[rstest]
#[case::password_and_file("password = \"p\"\npassword_file = \"/run/secrets/pw\"\n")]
#[case::password_and_env("password = \"p\"\npassword_env = \"CORP_PASSWORD\"\n")]
#[case::file_and_env("password_file = \"/run/secrets/pw\"\npassword_env = \"CORP_PASSWORD\"\n")]
#[case::token_and_file("token = \"t\"\ntoken_file = \"/run/secrets/tok\"\n")]
#[case::token_and_env("token = \"t\"\ntoken_env = \"CORP_TOKEN\"\n")]
fn test_an_upstream_credential_may_not_have_two_sources(#[case] credential: &str) {
    let text = format!(
        "[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n{credential}"
    );
    let err = toml_error(&text).to_string();
    assert!(
        err.contains("index corp: set at most one of a secret, its `_file` sibling, and its `_env` sibling"),
        "{err}"
    );
}

#[rstest]
#[case::password_and_file("password = \"p\"\npassword_file = \"/run/secrets/pw\"\n")]
#[case::password_and_env("password = \"p\"\npassword_env = \"CORP_PASSWORD\"\n")]
#[case::token_and_file("token = \"t\"\ntoken_file = \"/run/secrets/tok\"\n")]
#[case::token_and_env("token = \"t\"\ntoken_env = \"CORP_TOKEN\"\n")]
fn test_an_ordered_upstream_credential_may_not_have_two_sources(#[case] credential: &str) {
    let text = format!(
        "[[index]]\nname = \"corp\"\n\
         [[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\n{credential}"
    );
    let err = toml_error(&text).to_string();
    assert!(
        err.contains("index corp: set at most one of a secret, its `_file` sibling, and its `_env` sibling"),
        "{err}"
    );
}

#[test]
fn test_an_upstream_env_credential_may_not_be_empty() {
    let err =
        toml_error("[[index]]\nname = \"corp\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://corp/simple/\"\npassword_env = \"\"\n").to_string();
    assert_eq!(
        err,
        "index corp: `_env` names an environment variable and must not be empty"
    );
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
fn test_upload_target_requires_layers() {
    assert!(
        toml_error("[[index]]\nname = \"hosted\"\nhosted = true\nupload = \"other\"\n")
            .to_string()
            .contains("`upload` names the layer that receives uploads and requires `layers`")
    );
}

#[test]
fn test_layers_excludes_upstreams() {
    let text = "[[index]]\nname = \"team\"\nlayers = [\"a\"]\n\
                [[index.upstream]]\nname = \"mirror\"\nurl = \"https://mirror.example/simple/\"\n";
    assert!(
        toml_error(text)
            .to_string()
            .contains("`layers` and `[[index.upstream]]` are mutually exclusive")
    );
}

#[test]
fn test_index_ecosystem_parses_and_defaults() {
    let c = toml_config(
        "[[index]]\nname = \"pypi\"\necosystem = \"pypi\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://pypi.org/simple/\"\n",
    );
    assert_eq!(c.indexes[0].ecosystem, peryx_ecosystem_pypi::ECOSYSTEM);
    let d = toml_config(
        "[[index]]\nname = \"pypi\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://pypi.org/simple/\"\n",
    );
    assert_eq!(d.indexes[0].ecosystem, peryx_ecosystem_pypi::ECOSYSTEM);
}

#[test]
fn test_unknown_ecosystem_is_rejected() {
    let partial = config::from_toml(
        PathBuf::from("x.toml"),
        "[[index]]\nname = \"pypi\"\necosystem = \"npm\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://pypi.org/simple/\"\n",
    )
    .unwrap();
    let err = Config::default().apply(partial).unwrap_err();
    assert!(err.to_string().contains("unknown ecosystem"), "{err}");
}

#[test]
fn test_malformed_ecosystem_is_rejected() {
    let partial = config::from_toml(
        PathBuf::from("x.toml"),
        "[[index]]\nname = \"pypi\"\necosystem = \"not valid\"\n[[index.upstream]]\nname = \"primary\"\nurl = \"https://pypi.org/simple/\"\n",
    )
    .unwrap();

    let error = Config::default().apply(partial).unwrap_err();

    assert!(error.to_string().contains("unknown ecosystem"), "{error}");
}
