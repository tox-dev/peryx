use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use peryx_driver::IndexKind as RuntimeKind;
use peryx_storage::meta::MetaStore;
use peryx_upstream::Auth;
use rstest::rstest;
use tower::ServiceExt as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use peryx_ecosystem_oci::LibraryPrefix;

use crate::config::{
    AuthConfig, Config, IndexConfig, IndexKind, ReplicationConfig, SecretSource, WebhookConfig, WebhookSecret,
};
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
        anonymous_read: None,
        tokens: Vec::new(),
        kind: IndexKind::Cached {
            upstream: upstream.to_owned(),
            username: None,
            password: None,
            token: None,
            routing: None,
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
        anonymous_read: None,
        tokens: Vec::new(),
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
        anonymous_read: None,
        tokens: Vec::new(),
        kind: IndexKind::Virtual {
            layers: layers.iter().map(|&name| name.to_owned()).collect(),
            upload: upload.map(str::to_owned),
        },
    }
}

fn claim_writer(dir: &tempfile::TempDir, identity: &str) {
    MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity(identity)
        .unwrap();
}

fn replication_replica() -> ReplicationConfig {
    ReplicationConfig::Replica {
        upstream: "https://writer.example/".to_owned(),
        token: SecretSource::Literal("secret".to_owned()),
        poll_interval: Duration::from_secs(1),
        page_size: NonZeroUsize::MIN,
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
    let response = tokio::task::LocalSet::new()
        .run_until(router.oneshot(Request::builder().uri("/+status").body(Body::empty()).unwrap()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("root/pypi"));
}

#[tokio::test]
async fn test_build_router_fails_over_live_simple_requests() {
    let dir = tempfile::tempdir().unwrap();
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&first)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            br#"{"meta":{"api-version":"1.1"},"name":"flask","versions":[],"files":[]}"#.to_vec(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&second)
        .await;
    let partial = crate::config::from_toml(
        PathBuf::from("x.toml"),
        &format!(
            "[[index]]\nname = \"pypi\"\n\
             [[index.upstream]]\nname = \"first\"\nurl = \"{}/simple/\"\n\
             [[index.upstream]]\nname = \"second\"\nurl = \"{}/simple/\"\n",
            first.uri(),
            second.uri()
        ),
    )
    .unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default().apply(partial).unwrap()
    };
    let router = build_router(&config).unwrap();

    let response = tokio::task::LocalSet::new()
        .run_until(
            router.oneshot(
                Request::builder()
                    .uri("/pypi/simple/flask/")
                    .header("accept", "application/vnd.pypi.simple.v1+json")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes()).contains("flask"));
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
fn test_build_state_claims_configured_writer_identity() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        ..Config::default()
    };

    let state = build_state(&config).unwrap();

    assert_eq!(state.meta.writer_identity().unwrap().as_deref(), Some("writer-a"));
}

#[test]
fn test_build_state_rejects_a_competing_writer_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    store.claim_writer_identity("writer-a").unwrap();
    drop(store);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-b".to_owned()),
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected writer identity conflict");
    };

    let message = format!("{error:#}");
    assert!(message.contains("claim writer identity \"writer-b\""), "{message}");
    assert!(message.contains("claimed by writer \"writer-a\""), "{message}");
}

#[test]
fn test_build_state_makes_replica_upstreams_offline() {
    let dir = tempfile::tempdir().unwrap();
    claim_writer(&dir, "writer-a");
    let state = build_state(&Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        read_only: true,
        ..Config::default()
    })
    .unwrap();
    assert!(state.read_only);
    assert!(state.indexes.iter().all(|index| match &index.kind {
        peryx_driver::IndexKind::Cached { offline, .. } => *offline,
        peryx_driver::IndexKind::Hosted { .. } | peryx_driver::IndexKind::Virtual { .. } => true,
    }));
}

#[rstest]
#[case::read_only(false)]
#[case::replication(true)]
fn test_build_state_rejects_a_replica_without_writer_identity(#[case] configured_replication: bool) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        read_only: !configured_replication,
        replication: configured_replication.then(replication_replica),
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected invalid replica configuration");
    };

    assert_eq!(
        format!("{error:#}"),
        "validate configuration: writer identity: required in read replica mode"
    );
    assert!(!dir.path().join("peryx.redb").exists());
}

#[rstest]
#[case::read_only(false)]
#[case::replication(true)]
fn test_build_state_replica_does_not_claim_writer_identity(#[case] configured_replication: bool) {
    let dir = tempfile::tempdir().unwrap();
    claim_writer(&dir, "writer-a");

    let state = build_state(&Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        read_only: !configured_replication,
        replication: configured_replication.then(replication_replica),
        ..Config::default()
    })
    .unwrap();

    assert!(state.read_only);
    assert_eq!(state.meta.writer_identity().unwrap().as_deref(), Some("writer-a"));
}

#[rstest]
#[case::missing(None, "None")]
#[case::different(Some("writer-b"), "Some(\"writer-b\")")]
fn test_build_state_rejects_a_replica_with_an_unmatched_writer(
    #[case] active: Option<&str>,
    #[case] expected: &str,
    #[values(false, true)] configured_replication: bool,
) {
    let dir = tempfile::tempdir().unwrap();
    if let Some(active) = active {
        claim_writer(&dir, active);
    }
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        read_only: !configured_replication,
        replication: configured_replication.then(replication_replica),
        ..Config::default()
    };

    let Err(error) = build_state(&config) else {
        panic!("expected replica writer identity conflict");
    };

    assert_eq!(
        error.to_string(),
        format!("configured replica writer Some(\"writer-a\") does not match metadata store writer {expected}")
    );
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

#[test]
fn test_build_state_wires_the_token_realm_signing_key() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            signing_key: Some(SecretSource::Literal("super-secret".to_owned())),
            token_ttl_secs: 900,
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let state = build_state(&config).unwrap();

    assert!(state.signer.is_some());
    assert_eq!(state.token_ttl_secs, 900);
}

#[test]
fn test_build_state_reports_an_unreadable_signing_key_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        auth: AuthConfig {
            signing_key: Some(SecretSource::File(PathBuf::from("/nonexistent/peryx/signing-key"))),
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let Err(err) = build_state(&config) else {
        panic!("expected signing-key read error");
    };

    assert!(err.to_string().contains("read the token realm signing key"), "{err}");
}

#[rstest]
#[case::literal(empty_literal_signing_key, "token realm signing key must not be empty")]
#[case::file(empty_file_signing_key, "read the token realm signing key")]
fn test_build_state_rejects_an_empty_signing_key(#[case] source: fn(&Path) -> SecretSource, #[case] expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("data"),
        auth: AuthConfig {
            signing_key: Some(source(dir.path())),
            ..AuthConfig::default()
        },
        ..Config::default()
    };

    let Err(err) = build_state(&config) else {
        panic!("expected empty signing-key error");
    };

    assert_eq!(err.to_string(), expected);
}

fn empty_literal_signing_key(_: &Path) -> SecretSource {
    SecretSource::Literal(" \n".to_owned())
}

fn empty_file_signing_key(dir: &Path) -> SecretSource {
    let path = dir.join("signing-key");
    std::fs::write(&path, " \n").unwrap();
    SecretSource::File(path)
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
    let err = build_indexes(&indexes(), &AuthConfig::default(), false).unwrap_err();
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
fn test_build_indexes_reports_an_unreadable_secret_file() {
    let mut index = hosted("store");
    index.kind = IndexKind::Hosted {
        upload_token: Some(SecretSource::File(PathBuf::from("/nonexistent/peryx/token"))),
        volatile: true,
    };

    let err = build_indexes(&[index], &AuthConfig::default(), false).unwrap_err();

    assert!(
        err.to_string().contains("read the access rules of index store"),
        "{err}"
    );
}

#[test]
fn test_build_indexes_reads_upstream_credentials_from_files() {
    let dir = tempfile::tempdir().unwrap();
    let password = dir.path().join("password");
    let token = dir.path().join("token");
    std::fs::write(&password, "mirror-secret\n").unwrap();
    std::fs::write(&token, "upstream-token\n").unwrap();
    let mut index = cached("corp", "https://corp/simple/");
    let IndexKind::Cached {
        password: pw,
        token: tk,
        ..
    } = &mut index.kind
    else {
        panic!("expected a cached index");
    };
    *pw = Some(SecretSource::File(password));
    *tk = Some(SecretSource::File(token));

    let indexes = build_indexes(&[index], &AuthConfig::default(), false).unwrap();

    assert!(matches!(&indexes[0].kind, RuntimeKind::Cached { .. }));
}

#[test]
fn test_build_state_installs_normalized_upstream_routes() {
    let dir = tempfile::tempdir().unwrap();
    let partial = crate::config::from_toml(
        PathBuf::from("x.toml"),
        r#"
[[index]]
name = "pypi"
protected = ["Internal.Pkg"]

[index.pins]
flask = "public"

[[index.upstream]]
name = "internal"
url = "https://packages.example/simple/"

[[index.upstream]]
name = "public"
url = "https://pypi.org/simple/"
"#,
    )
    .unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default().apply(partial).unwrap()
    };

    let state = build_state(&config).unwrap();
    let router = &state.upstream_routes["pypi"];

    assert_eq!(
        router
            .candidates("internal-pkg")
            .map(peryx_upstream::NamedUpstream::name)
            .collect::<Vec<_>>(),
        ["internal"]
    );
    assert_eq!(
        router
            .candidates("flask")
            .map(peryx_upstream::NamedUpstream::name)
            .collect::<Vec<_>>(),
        ["public"]
    );
}

#[test]
fn test_build_indexes_reports_unreadable_upstream_credentials() {
    let mut index = cached("corp", "https://corp/simple/");
    let IndexKind::Cached { password, .. } = &mut index.kind else {
        panic!("expected a cached index");
    };
    *password = Some(SecretSource::File(PathBuf::from("/nonexistent/peryx/password")));

    let err = build_indexes(&[index], &AuthConfig::default(), false).unwrap_err();

    assert!(
        err.to_string().contains("read the upstream credentials of index corp"),
        "{err}"
    );
}

#[test]
fn test_build_indexes_defaults_upload_to_first_local_layer() {
    let configs = [
        cached("pypi", "https://pypi.org/simple/"),
        hosted("store"),
        virtual_index(&["pypi", "store"], None),
    ];
    let indexes = build_indexes(&configs, &AuthConfig::default(), false).unwrap();
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
    let indexes = build_indexes(&configs, &AuthConfig::default(), false).unwrap();
    let RuntimeKind::Virtual { upload, .. } = &indexes[1].kind else {
        panic!("expected virtual index");
    };
    assert_eq!(*upload, None);
}
