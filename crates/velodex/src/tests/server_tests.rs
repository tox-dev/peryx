use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use velodex_http::IndexKind as RuntimeKind;
use velodex_upstream::Auth;

use crate::config::{Config, IndexConfig, IndexKind, WebhookConfig, WebhookSecret};
use crate::server::{build_indexes, build_router, build_state, mirror_auth};

fn mirror(name: &str, upstream: &str) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        policy: velodex_policy::PolicyConfig::default(),
        webhooks: Vec::new(),
        ecosystem: velodex_format::Ecosystem::Pypi,
        kind: IndexKind::Cached {
            upstream: upstream.to_owned(),
            username: None,
            password: None,
            token: None,
            upstream_concurrency: velodex_http::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
            offline: false,
            prefetch: Box::default(),
        },
    }
}

fn local(name: &str) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        policy: velodex_policy::PolicyConfig::default(),
        webhooks: Vec::new(),
        ecosystem: velodex_format::Ecosystem::Pypi,
        kind: IndexKind::Hosted {
            upload_token: None,
            volatile: true,
        },
    }
}

fn overlay(layers: &[&str], upload: Option<&str>) -> IndexConfig {
    IndexConfig {
        name: "team".to_owned(),
        route: "team/dev".to_owned(),
        policy: velodex_policy::PolicyConfig::default(),
        webhooks: Vec::new(),
        ecosystem: velodex_format::Ecosystem::Pypi,
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
    assert!(dir.path().join("velodex.redb").exists());
}

#[test]
fn test_build_state_applies_upstream_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let mut pypi = mirror("pypi", "https://pypi.org/simple/");
    let IndexKind::Cached {
        upstream_concurrency, ..
    } = &mut pypi.kind
    else {
        panic!("expected mirror");
    };
    *upstream_concurrency = 2;
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![pypi],
        ..Config::default()
    };

    let state = build_state(&config).unwrap();

    let snapshots = state.upstream_limits.snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].max_concurrent, 2);
}

#[test]
fn test_build_state_reports_metadata_store_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("velodex.redb")).unwrap();
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
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![mirror("pypi", "not a url")],
        ..Config::default()
    };

    let Err(err) = build_state(&config) else {
        panic!("expected index error");
    };

    assert!(err.to_string().contains("build mirror index pypi"));
}

#[test]
fn test_build_state_reports_webhook_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = local("local");
    index.webhooks.push(WebhookConfig {
        name: "ci".to_owned(),
        url: "ftp://ci.example/hook".to_owned(),
        secret: WebhookSecret::Literal("secret".to_owned()),
        events: Vec::new(),
    });
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![index],
        ..Config::default()
    };

    let Err(err) = build_state(&config) else {
        panic!("expected webhook error");
    };

    assert!(err.to_string().contains("build webhook targets"));
}

#[test]
fn test_build_state_reports_missing_webhook_secret_env() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = local("local");
    index.webhooks.push(WebhookConfig {
        name: "ci".to_owned(),
        url: "https://ci.example/hook".to_owned(),
        secret: WebhookSecret::Env("VELODEX_TEST_MISSING_WEBHOOK_SECRET".to_owned()),
        events: Vec::new(),
    });
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![index],
        ..Config::default()
    };

    let Err(err) = build_state(&config) else {
        panic!("expected webhook env error");
    };

    assert!(
        err.to_string()
            .contains("read webhook secret env var VELODEX_TEST_MISSING_WEBHOOK_SECRET")
    );
}

#[tokio::test]
async fn test_build_state_starts_webhook_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = local("local");
    index.webhooks.push(WebhookConfig {
        name: "ci".to_owned(),
        url: "https://ci.example/hook".to_owned(),
        secret: WebhookSecret::Literal("secret".to_owned()),
        events: Vec::new(),
    });
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        indexes: vec![index],
        ..Config::default()
    };

    let state = build_state(&config).unwrap();

    assert!(!state.webhooks.is_empty());
}

#[test]
fn test_mirror_auth_bearer_takes_precedence() {
    assert_eq!(
        mirror_auth(Some("tok"), Some("u"), Some("p")),
        Auth::Bearer("tok".to_owned())
    );
}

#[test]
fn test_mirror_auth_basic() {
    assert_eq!(
        mirror_auth(None, Some("u"), Some("p")),
        Auth::Basic {
            username: "u".to_owned(),
            password: "p".to_owned()
        }
    );
}

#[test]
fn test_mirror_auth_none() {
    assert_eq!(mirror_auth(None, None, None), Auth::None);
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

#[test]
fn test_build_indexes_rejects_bad_upstream() {
    let err = build_indexes(&[mirror("pypi", "not a url")], false).unwrap_err();
    assert!(err.to_string().contains("build mirror index pypi"));
    assert!(err.to_string().contains("<invalid upstream URL>"));
}

#[test]
fn test_build_indexes_rejects_invalid_policy() {
    let mut index = mirror("pypi", "https://pypi.org/simple/");
    index.policy.allow_versions = Some("not a specifier".to_owned());
    let err = build_indexes(&[index], false).unwrap_err();
    assert!(err.to_string().contains("compile policy for pypi"));
}

#[test]
fn test_build_indexes_rejects_duplicate_name() {
    let err = build_indexes(&[local("a"), local("a")], false).unwrap_err();
    assert!(err.to_string().contains("duplicate index name"));
}

#[test]
fn test_build_indexes_rejects_duplicate_route() {
    let mut second = local("b");
    second.route = "a".to_owned();
    let err = build_indexes(&[local("a"), second], false).unwrap_err();
    assert!(err.to_string().contains("duplicate index route"));
}

#[test]
fn test_build_indexes_rejects_unsafe_route() {
    let mut index = local("safe");
    index.route = "root/../pypi".to_owned();
    let err = build_indexes(&[index], false).unwrap_err();
    assert!(err.to_string().contains("invalid index route root/../pypi"));
}

#[test]
fn test_build_indexes_rejects_reserved_route() {
    let mut index = local("safe");
    index.route = "browse/private".to_owned();
    let err = build_indexes(&[index], false).unwrap_err();
    assert!(err.to_string().contains("invalid index route browse/private"));
}

#[test]
fn test_build_indexes_rejects_unknown_layer() {
    let err = build_indexes(&[local("x"), overlay(&["ghost"], None)], false).unwrap_err();
    assert!(err.to_string().contains("unknown index ghost"));
}

#[test]
fn test_build_indexes_rejects_non_local_upload_target() {
    let configs = [
        mirror("pypi", "https://pypi.org/simple/"),
        overlay(&["pypi"], Some("pypi")),
    ];
    let err = build_indexes(&configs, false).unwrap_err();
    assert!(err.to_string().contains("not a local index"));
}

#[test]
fn test_build_indexes_defaults_upload_to_first_local_layer() {
    let configs = [
        mirror("pypi", "https://pypi.org/simple/"),
        local("store"),
        overlay(&["pypi", "store"], None),
    ];
    let indexes = build_indexes(&configs, false).unwrap();
    let RuntimeKind::Virtual { upload, layers } = &indexes[2].kind else {
        panic!("expected overlay");
    };
    assert_eq!(*upload, Some(1)); // "store" is the first local layer
    assert_eq!(layers, &[0, 1]);
}

#[test]
fn test_build_indexes_overlay_without_local_layer_has_no_upload() {
    let configs = [mirror("pypi", "https://pypi.org/simple/"), overlay(&["pypi"], None)];
    let indexes = build_indexes(&configs, false).unwrap();
    let RuntimeKind::Virtual { upload, .. } = &indexes[1].kind else {
        panic!("expected overlay");
    };
    assert_eq!(*upload, None);
}
