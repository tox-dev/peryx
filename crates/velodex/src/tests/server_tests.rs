use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use velodex_http::IndexKind as RuntimeKind;
use velodex_upstream::Auth;

use crate::config::{Config, IndexConfig, IndexKind};
use crate::server::{build_indexes, build_router, mirror_auth};

fn mirror(name: &str, upstream: &str) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        kind: IndexKind::Mirror {
            upstream: upstream.to_owned(),
            username: None,
            password: None,
            token: None,
        },
    }
}

fn local(name: &str) -> IndexConfig {
    IndexConfig {
        name: name.to_owned(),
        route: name.to_owned(),
        kind: IndexKind::Local {
            upload_token: None,
            volatile: true,
        },
    }
}

fn overlay(layers: &[&str], upload: Option<&str>) -> IndexConfig {
    IndexConfig {
        name: "team".to_owned(),
        route: "team/dev".to_owned(),
        kind: IndexKind::Overlay {
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
    assert!(build_router(&config).is_err());
}

#[test]
fn test_build_indexes_rejects_bad_upstream() {
    assert!(build_indexes(&[mirror("pypi", "not a url")]).is_err());
}

#[test]
fn test_build_indexes_rejects_duplicate_name() {
    let err = build_indexes(&[local("a"), local("a")]).unwrap_err();
    assert!(err.to_string().contains("duplicate index name"));
}

#[test]
fn test_build_indexes_rejects_duplicate_route() {
    let mut second = local("b");
    second.route = "a".to_owned();
    let err = build_indexes(&[local("a"), second]).unwrap_err();
    assert!(err.to_string().contains("duplicate index route"));
}

#[test]
fn test_build_indexes_rejects_unknown_layer() {
    let err = build_indexes(&[local("x"), overlay(&["ghost"], None)]).unwrap_err();
    assert!(err.to_string().contains("unknown index ghost"));
}

#[test]
fn test_build_indexes_rejects_non_local_upload_target() {
    let configs = [
        mirror("pypi", "https://pypi.org/simple/"),
        overlay(&["pypi"], Some("pypi")),
    ];
    let err = build_indexes(&configs).unwrap_err();
    assert!(err.to_string().contains("not a local index"));
}

#[test]
fn test_build_indexes_defaults_upload_to_first_local_layer() {
    let configs = [
        mirror("pypi", "https://pypi.org/simple/"),
        local("store"),
        overlay(&["pypi", "store"], None),
    ];
    let indexes = build_indexes(&configs).unwrap();
    let RuntimeKind::Overlay { upload, layers } = &indexes[2].kind else {
        panic!("expected overlay");
    };
    assert_eq!(*upload, Some(1)); // "store" is the first local layer
    assert_eq!(layers, &[0, 1]);
}

#[test]
fn test_build_indexes_overlay_without_local_layer_has_no_upload() {
    let configs = [mirror("pypi", "https://pypi.org/simple/"), overlay(&["pypi"], None)];
    let indexes = build_indexes(&configs).unwrap();
    let RuntimeKind::Overlay { upload, .. } = &indexes[1].kind else {
        panic!("expected overlay");
    };
    assert_eq!(*upload, None);
}
