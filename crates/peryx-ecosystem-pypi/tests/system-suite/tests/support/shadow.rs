use std::collections::{BTreeMap, BTreeSet};

use peryx::config::{SecretSource, TokenConfig, UpstreamConfig, UpstreamRoutingConfig, UpstreamTlsConfig};
use peryx_identity::Action;

pub fn writer_token(secret: SecretSource) -> TokenConfig {
    TokenConfig {
        name: "uploader".to_owned(),
        secret,
        resources: vec!["*".to_owned()],
        actions: BTreeSet::from([Action::Write, Action::Delete]),
        expires_at: None,
    }
}

pub fn single_route(url: &str) -> UpstreamRoutingConfig {
    UpstreamRoutingConfig {
        upstreams: vec![UpstreamConfig {
            name: "primary".to_owned(),
            url: url.to_owned(),
            artifact_url: None,
            trusted_hosts: Vec::new(),
            username: None,
            password: None,
            token: None,
            credential_exec: None,
            credential_refresh: None,
            tls: UpstreamTlsConfig::default(),
        }],
        fallback: true,
        protected: Vec::new(),
        pins: BTreeMap::new(),
    }
}
