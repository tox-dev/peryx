use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use peryx_driver::serving::AuthInstallContext;
use peryx_driver::serving::PluginAuthConfig;
use peryx_driver::state::ServingState;
use peryx_identity::Glob;
use serde::Deserialize;
use sigstore_verify::Verifier as SigstoreVerifier;
use sigstore_verify::trust_root::TrustedRoot;

use super::http::TrustedPublishingRoutes;
use super::policy::{AttestationPolicy, TrustedPublisher};
use super::runtime::{OidcRuntime, PublisherBinding};
use crate::ECOSYSTEM;

pub const AUTH_FIELDS: &[&str] = &[
    "oidc_audience",
    "oidc_trusted_endpoint_hosts",
    "sigstore_trusted_root",
    "trusted_publisher",
];

const MAX_TRUSTED_ROOT_BYTES: usize = 1024 * 1024;

pub fn auth_defaults() -> toml::Table {
    toml::Table::from_iter([("oidc_audience".to_owned(), toml::Value::String("peryx".to_owned()))])
}

pub fn validate(config: PluginAuthConfig<'_>) -> Result<(), String> {
    let trusted = parse(config.values)?;
    attestation_verifier(trusted.sigstore_trusted_root.as_deref())?;
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
        sigstore_trusted_root,
        publishers,
    } = parse(values)?;
    if publishers.is_empty() {
        return Ok(());
    }
    let signer = context
        .signer()
        .cloned()
        .ok_or_else(|| "auth: `signing_key` is required when trusted publishers are configured".to_owned())?;
    let attestation_verifier = attestation_verifier(sigstore_trusted_root.as_deref())?;
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
                            attestation: publisher.attestation_identity.map(|identity| AttestationPolicy {
                                identity,
                                claims: publisher.attestation_claims,
                            }),
                        },
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
            &trusted_endpoint_hosts,
            signer,
            context.token_ttl_secs(),
            attestation_verifier,
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
    sigstore_trusted_root: Option<String>,
    #[serde(rename = "trusted_publisher")]
    publishers: Vec<PublisherConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            audience: "peryx".to_owned(),
            trusted_endpoint_hosts: Vec::new(),
            sigstore_trusted_root: None,
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
    #[serde(default)]
    attestation_identity: Option<String>,
    #[serde(default)]
    attestation_claims: BTreeMap<String, String>,
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
    if config
        .sigstore_trusted_root
        .as_ref()
        .is_some_and(|root| root.is_empty() || root.len() > MAX_TRUSTED_ROOT_BYTES)
    {
        return Err("auth: `sigstore_trusted_root` must be between 1 byte and 1 MiB".to_owned());
    }
    if config.publishers.iter().any(|publisher| {
        publisher.id.trim().is_empty()
            || publisher.issuer.trim().is_empty()
            || publisher.repository.trim().is_empty()
            || publisher.subject.trim().is_empty()
            || publisher.projects.is_empty()
            || publisher.projects.iter().any(|project| project.trim().is_empty())
            || publisher
                .attestation_identity
                .as_ref()
                .is_some_and(|identity| identity.trim().is_empty())
            || publisher
                .attestation_claims
                .iter()
                .any(|(claim, value)| claim.trim().is_empty() || value.trim().is_empty())
            || (publisher.attestation_identity.is_none() && !publisher.attestation_claims.is_empty())
    }) {
        return Err("auth: trusted publisher fields and project lists must not be empty".to_owned());
    }
    Ok(config)
}

fn invalid_repository(id: &str) -> String {
    format!("trusted publisher {id}: repository must name a writable index with trusted publishing support")
}

fn attestation_verifier(root: Option<&str>) -> Result<Option<Arc<SigstoreVerifier>>, String> {
    root.map(|root| {
        TrustedRoot::from_json(root)
            .ok()
            .and_then(|root| SigstoreVerifier::new(&root).ok())
            .map(Arc::new)
            .ok_or_else(|| "auth: `sigstore_trusted_root` is invalid".to_owned())
    })
    .transpose()
}

#[cfg(test)]
#[path = "../../tests/unit/trusted_publishing/config_tests.rs"]
mod tests;
