use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use peryx_driver::IndexKind as RuntimeKind;
use peryx_upstream::Auth;
use rstest::rstest;
use tower::ServiceExt as _;

use peryx_ecosystem_oci::LibraryPrefix;

use crate::config::{Config, IndexConfig, IndexKind, WebhookConfig, WebhookSecret};
use crate::server::{build_index_settings, build_indexes, build_router, build_state, upstream_auth};

fn config_with(dir: &tempfile::TempDir, indexes: Vec<IndexConfig>) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        indexes,
        ..Config::default()
    }
}

fn cached(name: &str, upstream: &str) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Cached {
            upstream: upstream.to_owned(),
            username: None,
            password: None,
            token: None,
            upstream_concurrency: peryx_driver::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
            offline: false,
            prefetch: Box::default(),
        },
    }
}

fn hosted(name: &str) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Hosted {
            upload_token: None,
            volatile: true,
        },
    }
}

fn virtual_index(layers: &[&str], upload: Option<&str>) -> IndexConfig {
    IndexConfig {
        name: "team".to_owned(),
        route: "team/dev".to_owned(),
        policy: peryx_policy::PolicyConfig::default(),
        ecosystem_policy: toml::Table::new(),
        ecosystem_settings: toml::Table::new(),
        webhooks: Vec::new(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Virtual {
            layers: layers.iter().map(|&name| name.to_owned()).collect(),
            upload: upload.map(str::to_owned),
        },
    }
}

#[tokio::test]
async fn test_build_router_serves_status() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    let router = build_router(&config).unwrap();
    let response = router
        .oneshot(Request::builder().uri("/+status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("root/pypi"));
}

#[test]
fn test_build_state_opens_configured_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };

    let state = build_state(&config).unwrap();

    assert_eq!(state.indexes.len(), config.indexes.len());
    assert!(dir.path().join("peryx.redb").exists());
}

#[test]
fn test_build_state_applies_upstream_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let mut pypi = cached("pypi", "https://pypi.org/simple/");
    let IndexKind::Cached {
        upstream_concurrency, ..
    } = &mut pypi.kind
    else {
        panic!("expected cached index");
    };
    *upstream_concurrency = 2;
    let config = config_with(&dir, vec![pypi]);

    let state = build_state(&config).unwrap();

    let snapshots = state.upstream_limits.snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].max_concurrent, 2);
}

#[test]
fn test_build_state_reports_metadata_store_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("peryx.redb")).unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };

    let Err(err) = build_state(&config) else {
        panic!("expected metadata store error");
    };

    assert!(err.to_string().contains("open metadata store"));
}

#[test]
fn test_build_state_reports_index_errors() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_with(&dir, vec![cached("pypi", "not a url")]);

    let Err(err) = build_state(&config) else {
        panic!("expected index error");
    };

    assert!(err.to_string().contains("build cached index pypi"));
}

#[test]
fn test_build_state_reports_webhook_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = hosted("hosted");
    index.webhooks.push(WebhookConfig {
        name: "ci".to_owned(),
        url: "ftp://ci.example/hook".to_owned(),
        secret: WebhookSecret::Literal("secret".to_owned()),
        events: Vec::new(),
    });
    let config = config_with(&dir, vec![index]);

    let Err(err) = build_state(&config) else {
        panic!("expected webhook error");
    };

    assert!(err.to_string().contains("build webhook targets"));
}

#[test]
fn test_build_state_reports_missing_webhook_secret_env() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = hosted("hosted");
    index.webhooks.push(WebhookConfig {
        name: "ci".to_owned(),
        url: "https://ci.example/hook".to_owned(),
        secret: WebhookSecret::Env("PERYX_TEST_MISSING_WEBHOOK_SECRET".to_owned()),
        events: Vec::new(),
    });
    let config = config_with(&dir, vec![index]);

    let Err(err) = build_state(&config) else {
        panic!("expected webhook env error");
    };

    assert!(
        err.to_string()
            .contains("read webhook secret env var PERYX_TEST_MISSING_WEBHOOK_SECRET")
    );
}

#[tokio::test]
async fn test_build_state_starts_webhook_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = hosted("hosted");
    index.webhooks.push(WebhookConfig {
        name: "ci".to_owned(),
        url: "https://ci.example/hook".to_owned(),
        secret: WebhookSecret::Literal("secret".to_owned()),
        events: Vec::new(),
    });
    let config = config_with(&dir, vec![index]);

    let state = build_state(&config).unwrap();

    assert!(!state.webhooks.is_empty());
}

#[rstest]
#[case::bearer_takes_precedence(Some("tok"), Some("u"), Some("p"), Auth::Bearer("tok".to_owned()))]
#[case::basic(None, Some("u"), Some("p"), Auth::Basic { username: "u".to_owned(), password: "p".to_owned() })]
#[case::none(None, None, None, Auth::None)]
fn test_upstream_auth(
    #[case] token: Option<&str>,
    #[case] user: Option<&str>,
    #[case] pass: Option<&str>,
    #[case] expected: Auth,
) {
    assert_eq!(upstream_auth(token, user, pass), expected);
}

#[test]
fn test_build_router_data_dir_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("blocker");
    std::fs::write(&file, "x").unwrap();
    let config = Config {
        data_dir: file.join("sub"),
        ..Config::default()
    };
    let err = build_router(&config).unwrap_err();
    assert!(err.to_string().contains("create data directory"));
}

#[rstest]
#[case::bad_upstream(
    || vec![cached("pypi", "not a url")],
    &["build cached index pypi", "<invalid upstream URL>"][..]
)]
#[case::invalid_policy(
    || {
        let mut index = cached("pypi", "https://pypi.org/simple/");
        index
            .ecosystem_policy
            .insert("allow_versions".to_owned(), "not a specifier".into());
        vec![index]
    },
    &["compile policy for pypi"][..]
)]
#[case::unknown_policy_key(
    || {
        let mut index = cached("pypi", "https://pypi.org/simple/");
        index.ecosystem_policy.insert("bogus".to_owned(), 1.into());
        vec![index]
    },
    &["compile policy for pypi", "unknown field `bogus`"][..]
)]
#[case::duplicate_name(|| vec![hosted("a"), hosted("a")], &["duplicate index name"][..])]
#[case::duplicate_route(
    || {
        let mut second = hosted("b");
        second.route = "a".to_owned();
        vec![hosted("a"), second]
    },
    &["duplicate index route"][..]
)]
#[case::unsafe_route(
    || {
        let mut index = hosted("safe");
        index.route = "root/../pypi".to_owned();
        vec![index]
    },
    &["invalid index route root/../pypi"][..]
)]
#[case::reserved_route(
    || {
        let mut index = hosted("safe");
        index.route = "browse/private".to_owned();
        vec![index]
    },
    &["invalid index route browse/private"][..]
)]
#[case::unknown_layer(
    || vec![hosted("x"), virtual_index(&["ghost"], None)],
    &["unknown index ghost"][..]
)]
#[case::non_local_upload_target(
    || vec![cached("pypi", "https://pypi.org/simple/"), virtual_index(&["pypi"], Some("pypi"))],
    &["not a hosted index"][..]
)]
fn test_build_indexes_rejects(#[case] indexes: fn() -> Vec<IndexConfig>, #[case] expected: &[&str]) {
    let err = build_indexes(&indexes(), false).unwrap_err();
    let message = err.to_string();
    for substr in expected {
        assert!(message.contains(substr), "{message}");
    }
}

#[rstest]
#[case::absent(None, LibraryPrefix::Auto)]
#[case::auto(Some("auto".into()), LibraryPrefix::Auto)]
#[case::always(Some(true.into()), LibraryPrefix::Always)]
#[case::never(Some(false.into()), LibraryPrefix::Never)]
fn test_build_index_settings_compiles_an_oci_library_prefix(
    #[case] value: Option<toml::Value>,
    #[case] expected: LibraryPrefix,
) {
    let mut index = IndexConfig {
        ecosystem: peryx_core::Ecosystem::Oci,
        ..cached("hub", "https://registry-1.docker.io/")
    };
    if let Some(value) = value {
        index.ecosystem_settings.insert("library_prefix".to_owned(), value);
    }
    let settings = build_index_settings(&[index]).unwrap();
    assert_eq!(settings["hub"].library_prefix, expected);
}

#[rstest]
#[case::invalid_oci_value(
    peryx_core::Ecosystem::Oci,
    "library_prefix",
    "always".into(),
    &["compile settings for hub", "must be true, false, or \"auto\""][..]
)]
#[case::unknown_oci_key(
    peryx_core::Ecosystem::Oci,
    "libary_prefix",
    true.into(),
    &["compile settings for hub", "unknown field `libary_prefix`"][..]
)]
#[case::settings_on_an_ecosystem_without_any(
    peryx_core::Ecosystem::Pypi,
    "library_prefix",
    "auto".into(),
    &["compile settings for hub", "unknown field `library_prefix`"][..]
)]
fn test_build_index_settings_rejects(
    #[case] ecosystem: peryx_core::Ecosystem,
    #[case] key: &str,
    #[case] value: toml::Value,
    #[case] expected: &[&str],
) {
    let mut index = IndexConfig {
        ecosystem,
        ..cached("hub", "https://registry-1.docker.io/")
    };
    index.ecosystem_settings.insert(key.to_owned(), value);
    let message = build_index_settings(&[index]).unwrap_err().to_string();
    for substr in expected {
        assert!(message.contains(substr), "{message}");
    }
}

#[test]
fn test_build_indexes_defaults_upload_to_first_local_layer() {
    let configs = [
        cached("pypi", "https://pypi.org/simple/"),
        hosted("store"),
        virtual_index(&["pypi", "store"], None),
    ];
    let indexes = build_indexes(&configs, false).unwrap();
    let RuntimeKind::Virtual { upload, layers } = &indexes[2].kind else {
        panic!("expected virtual index");
    };
    assert_eq!(*upload, Some(1)); // "store" is the first hosted layer
    assert_eq!(layers, &[0, 1]);
}

#[test]
fn test_build_indexes_overlay_without_local_layer_has_no_upload() {
    let configs = [
        cached("pypi", "https://pypi.org/simple/"),
        virtual_index(&["pypi"], None),
    ];
    let indexes = build_indexes(&configs, false).unwrap();
    let RuntimeKind::Virtual { upload, .. } = &indexes[1].kind else {
        panic!("expected virtual index");
    };
    assert_eq!(*upload, None);
}
