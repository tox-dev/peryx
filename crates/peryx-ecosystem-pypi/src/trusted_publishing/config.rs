use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use peryx_driver::serving::AuthInstallContext;
use peryx_driver::serving::PluginAuthConfig;
use peryx_driver::state::ServingState;
use peryx_identity::Glob;
use serde::Deserialize;

use super::http::TrustedPublishingRoutes;
use super::policy::TrustedPublisher;
use super::runtime::{OidcRuntime, PublisherBinding};
use crate::ECOSYSTEM;

pub const AUTH_FIELDS: &[&str] = &["oidc_audience", "oidc_trusted_endpoint_hosts", "trusted_publisher"];

pub fn auth_defaults() -> toml::Table {
    toml::Table::from_iter([("oidc_audience".to_owned(), toml::Value::String("peryx".to_owned()))])
}

pub fn validate(config: PluginAuthConfig<'_>) -> Result<(), String> {
    let trusted = parse(config.values)?;
    if trusted.publishers.is_empty() {
        return Ok(());
    }
    if !config.signing_key_configured {
        return Err("auth: `signing_key` is required when trusted publishers are configured".to_owned());
    }
    let mut ids = HashSet::new();
    for publisher in &trusted.publishers {
        if !ids.insert(&publisher.id) {
            return Err(format!(
                "trusted publisher {}: publisher IDs must be unique",
                publisher.id
            ));
        }
        if !config
            .indexes
            .iter()
            .any(|index| index.name == publisher.repository && index.ecosystem == ECOSYSTEM && index.writable)
        {
            return Err(invalid_repository(&publisher.id));
        }
    }
    Ok(())
}

pub fn install(context: &mut AuthInstallContext<'_>, values: &toml::Table) -> Result<(), String> {
    let Config {
        audience,
        trusted_endpoint_hosts,
        publishers,
    } = parse(values)?;
    if publishers.is_empty() {
        return Ok(());
    }
    let signer = context
        .signer()
        .cloned()
        .ok_or_else(|| "auth: `signing_key` is required when trusted publishers are configured".to_owned())?;
    let runtime = Arc::new(
        OidcRuntime::new(
            publishers
                .into_iter()
                .map(|publisher| {
                    let route = context
                        .writable_index_route(&ECOSYSTEM, &publisher.repository)
                        .ok_or_else(|| invalid_repository(&publisher.id))?
                        .to_owned();
                    Ok(PublisherBinding {
                        id: publisher.id,
                        repository: publisher.repository,
                        route,
                        publisher: TrustedPublisher {
                            issuer: publisher.issuer,
                            audience: audience.clone(),
                            subject: Glob::new(publisher.subject),
                            claims: publisher.claims,
                            projects: publisher.projects.into_iter().map(Glob::new).collect(),
                        },
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
            &trusted_endpoint_hosts,
            signer,
            context.token_ttl_secs(),
        )
        .map_err(|error| error.to_string())?,
    );
    context.register_service(runtime.clone());
    context.register_routes(Arc::new(TrustedPublishingRoutes::new(runtime)));
    Ok(())
}

#[must_use]
pub fn enabled(state: &ServingState) -> bool {
    state.plugin_service::<OidcRuntime>().is_some()
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    #[serde(rename = "oidc_audience")]
    audience: String,
    /// Hosts whose non-public addresses a discovered key endpoint may name. Each publisher's own
    /// issuer host is trusted without listing.
    #[serde(rename = "oidc_trusted_endpoint_hosts")]
    trusted_endpoint_hosts: Vec<String>,
    #[serde(rename = "trusted_publisher")]
    publishers: Vec<PublisherConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            audience: "peryx".to_owned(),
            trusted_endpoint_hosts: Vec::new(),
            publishers: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublisherConfig {
    id: String,
    issuer: String,
    repository: String,
    subject: String,
    #[serde(default)]
    projects: Vec<String>,
    #[serde(default)]
    claims: BTreeMap<String, String>,
}

fn parse(values: &toml::Table) -> Result<Config, String> {
    let config = toml::Value::Table(values.clone())
        .try_into::<Config>()
        .map_err(|error| format!("auth: {error}"))?;
    if config.audience.trim().is_empty() {
        return Err("auth: `oidc_audience` must not be empty".to_owned());
    }
    if config.trusted_endpoint_hosts.iter().any(|host| host.trim().is_empty()) {
        return Err("auth: `oidc_trusted_endpoint_hosts` entries must not be empty".to_owned());
    }
    if config.publishers.iter().any(|publisher| {
        publisher.id.trim().is_empty()
            || publisher.issuer.trim().is_empty()
            || publisher.repository.trim().is_empty()
            || publisher.subject.trim().is_empty()
            || publisher.projects.is_empty()
            || publisher.projects.iter().any(|project| project.trim().is_empty())
    }) {
        return Err("auth: trusted publisher fields and project lists must not be empty".to_owned());
    }
    Ok(config)
}

fn invalid_repository(id: &str) -> String {
    format!("trusted publisher {id}: repository must name a writable index with trusted publishing support")
}

#[cfg(test)]
#[path = "../../tests/unit/trusted_publishing/config_tests.rs"]
mod tests;
