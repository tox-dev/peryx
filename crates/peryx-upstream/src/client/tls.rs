//! Per-upstream TLS trust and client identity.

use std::path::Path;

/// Parsed TLS material for one upstream and its artifact hosts.
#[derive(Clone, Default)]
pub struct UpstreamTls {
    roots: Vec<reqwest::Certificate>,
    identity: Option<reqwest::Identity>,
}

impl std::fmt::Debug for UpstreamTls {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamTls")
            .field("custom_ca", &!self.roots.is_empty())
            .field("client_identity", &self.identity.is_some())
            .finish()
    }
}

impl UpstreamTls {
    /// Read and parse TLS material for one upstream.
    ///
    /// `identity` names a PEM certificate chain and its unencrypted PEM private key. The files stay
    /// separate to match common secret mounts.
    ///
    /// # Errors
    /// Returns [`UpstreamTlsError`] when a file cannot be read or its PEM material is invalid.
    pub fn from_paths(ca_bundle: Option<&Path>, identity: Option<(&Path, &Path)>) -> Result<Self, UpstreamTlsError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let roots = match ca_bundle {
            Some(path) => {
                let roots =
                    reqwest::Certificate::from_pem_bundle(&std::fs::read(path).map_err(UpstreamTlsError::ReadCa)?)
                        .map_err(UpstreamTlsError::InvalidCa)?;
                if roots.is_empty() {
                    return Err(UpstreamTlsError::EmptyCa);
                }
                roots
            }
            None => Vec::new(),
        };
        let identity = identity
            .map(|(certificate, key)| {
                let mut pem = std::fs::read(certificate).map_err(UpstreamTlsError::ReadCertificate)?;
                pem.push(b'\n');
                pem.extend(std::fs::read(key).map_err(UpstreamTlsError::ReadKey)?);
                reqwest::Identity::from_pem(&pem).map_err(UpstreamTlsError::InvalidIdentity)
            })
            .transpose()?;
        Ok(Self { roots, identity })
    }

    pub(super) fn apply(&self, mut builder: reqwest::ClientBuilder, include_identity: bool) -> reqwest::ClientBuilder {
        if !self.roots.is_empty() {
            builder = builder.tls_certs_merge(self.roots.clone());
        }
        if include_identity && let Some(identity) = &self.identity {
            builder = builder.identity(identity.clone());
        }
        builder
    }

    pub(super) const fn has_identity(&self) -> bool {
        self.identity.is_some()
    }
}

/// A redacted upstream TLS configuration error.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamTlsError {
    #[error("cannot read upstream CA bundle")]
    ReadCa(#[source] std::io::Error),
    #[error("cannot read upstream client certificate")]
    ReadCertificate(#[source] std::io::Error),
    #[error("cannot read upstream client private key")]
    ReadKey(#[source] std::io::Error),
    #[error("upstream CA bundle has invalid PEM certificates")]
    InvalidCa(#[source] reqwest::Error),
    #[error("upstream CA bundle contains no certificates")]
    EmptyCa,
    #[error("upstream client certificate or private key has invalid PEM")]
    InvalidIdentity(#[source] reqwest::Error),
}
