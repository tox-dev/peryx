use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::time::Duration;

use peryx::config::{
    AvailabilityConfig, Config, IndexKind, ReplicationConfig, SecretSource, TokenConfig, UpstreamConfig,
    UpstreamRoutingConfig, WebhookConfig, WebhookSecret,
};
use peryx::server::build_state;
use peryx_driver::IndexKind as RuntimeIndexKind;
use peryx_identity::Action;

#[test]
fn replica_disables_pypi_writers() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: dir.path().to_path_buf(),
        writer_identity: Some("writer-a".to_owned()),
        availability: AvailabilityConfig::Dc(ReplicationConfig::Replica {
            upstream: "https://primary.example/".to_owned(),
            token: SecretSource::Literal("replica-secret".to_owned()),
            poll_interval: Duration::from_millis(1),
            page_size: NonZeroUsize::new(10).unwrap(),
        }),
        ..Config::default()
    };
    peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb"))
        .unwrap()
        .claim_writer_identity("writer-a")
        .unwrap();
    config.indexes[0].kind = IndexKind::Cached {
        routing: UpstreamRoutingConfig {
            upstreams: vec![UpstreamConfig {
                name: "primary".to_owned(),
                url: "https://packages.example/simple/".to_owned(),
                artifact_url: None,
                trusted_hosts: Vec::new(),
                username: Some("replica".to_owned()),
                password: Some(SecretSource::File("missing-routed-upstream-password".into())),
                token: None,
                credential_exec: None,
                credential_refresh: None,
                tls: peryx::config::UpstreamTlsConfig::default(),
            }],
            fallback: true,
            protected: Vec::new(),
            pins: BTreeMap::default(),
        },
        upstream_concurrency: peryx_driver::rate_limit::DEFAULT_UPSTREAM_CONCURRENCY,
        offline: false,
        prefetch: Box::default(),
    };
    config.indexes[1].tokens.extend([
        TokenConfig {
            name: "reader".to_owned(),
            secret: SecretSource::Literal("reader-secret".to_owned()),
            resources: vec!["*".to_owned()],
            actions: BTreeSet::from([Action::Read, Action::Write]),
            expires_at: None,
        },
        TokenConfig {
            name: "writer".to_owned(),
            secret: SecretSource::File("missing-writer-token".into()),
            resources: vec!["*".to_owned()],
            actions: BTreeSet::from([Action::Write]),
            expires_at: None,
        },
    ]);
    config.indexes[1].webhooks.push(WebhookConfig {
        name: "audit".to_owned(),
        url: "https://hooks.example/audit".to_owned(),
        secret: WebhookSecret::Env("PERYX_TEST_MISSING_REPLICA_WEBHOOK_SECRET".to_owned()),
        events: Vec::new(),
    });

    let state = build_state(&config).unwrap();

    assert!(state.serving.read_only);
    assert!(matches!(
        state.serving.indexes[0].kind,
        RuntimeIndexKind::Cached { offline: true, .. }
    ));
    assert!(state.serving.upstream_routes.is_empty());
    assert!(state.serving.indexes[1].acl.grants_to_anyone(Action::Read));
    assert!(!state.serving.indexes[1].acl.grants_to_anyone(Action::Write));
    assert!(!state.serving.indexes[1].acl.grants_to_anyone(Action::Delete));
    assert!(matches!(
        state.serving.indexes[2].kind,
        RuntimeIndexKind::Virtual { write_target: None, .. }
    ));
    assert!(state.serving.webhooks.is_empty());
}
