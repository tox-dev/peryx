use std::fmt;
use std::num::NonZeroU32;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bb8_ldap::LdapConnectionManager;
use bb8_ldap::bb8::{ManageConnection, Pool};
use bb8_ldap::ldap3::{
    Ldap, LdapConnSettings, Scope, SearchEntry, SearchOptions, SearchResult, dn_escape, ldap_escape,
};
use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::{CertificateDer, pem::PemObject as _};
use url::Url;

use crate::{
    ExternalGroup, ExternalGroupGrant, ExternalIdentity, ExternalIdentityLinker, ExternalIdentityResolution,
    ExternalIdentityStore, ExternalLogin, ExternalSubject, ProviderId, UserName,
};

const LDAP_INVALID_CREDENTIALS: u32 = 49;
const LDAP_SIZE_LIMIT_EXCEEDED: u32 = 4;
const MAX_ATTRIBUTE_BYTES: usize = 128;
const MAX_DISPLAY_NAME_BYTES: usize = 1_024;
const MAX_DN_BYTES: usize = 4_096;

#[derive(Clone, PartialEq, Eq)]
pub enum LdapBindMode {
    Direct {
        dn_attribute: String,
    },
    Search {
        username_attribute: String,
        bind_dn: String,
        bind_password: String,
    },
}

impl fmt::Debug for LdapBindMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct { dn_attribute } => formatter
                .debug_struct("Direct")
                .field("dn_attribute", dn_attribute)
                .finish(),
            Self::Search {
                username_attribute,
                bind_dn,
                ..
            } => formatter
                .debug_struct("Search")
                .field("username_attribute", username_attribute)
                .field("bind_dn", bind_dn)
                .field("bind_password", &"[redacted]")
                .finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct LdapProviderSettings {
    pub id: ProviderId,
    pub url: Url,
    pub base_dn: String,
    pub bind: LdapBindMode,
    pub subject_attribute: String,
    pub display_name_attribute: String,
    pub group_attribute: Option<String>,
    pub custom_ca_pem: Option<Vec<u8>>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub max_connections: NonZeroU32,
}

impl fmt::Debug for LdapProviderSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LdapProviderSettings")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("base_dn", &self.base_dn)
            .field("bind", &self.bind)
            .field("subject_attribute", &self.subject_attribute)
            .field("display_name_attribute", &self.display_name_attribute)
            .field("group_attribute", &self.group_attribute)
            .field("custom_ca", &self.custom_ca_pem.is_some())
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("max_connections", &self.max_connections)
            .finish()
    }
}

/// Uses `StartTLS` and returns assertions without changing local identity state.
#[derive(Clone)]
pub struct LdapProvider {
    id: ProviderId,
    url: Url,
    base_dn: String,
    bind: RuntimeBindMode,
    subject_attribute: String,
    display_name_attribute: String,
    group_attribute: Option<String>,
    connect_timeout: Duration,
    request_timeout: Duration,
    manager: SafeLdapManager,
    pool: Arc<OnceLock<Pool<SafeLdapManager>>>,
    max_connections: u32,
    custom_ca: bool,
}

impl fmt::Debug for LdapProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LdapProvider")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("base_dn", &self.base_dn)
            .field("bind", &self.bind)
            .field("subject_attribute", &self.subject_attribute)
            .field("display_name_attribute", &self.display_name_attribute)
            .field("group_attribute", &self.group_attribute)
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .field("max_connections", &self.max_connections)
            .field("custom_ca", &self.custom_ca)
            .finish_non_exhaustive()
    }
}

impl LdapProvider {
    /// Parses trust roots without opening a directory connection.
    ///
    /// # Errors
    /// Returns [`LdapProviderBuildError`] for invalid URLs, DNs, attributes, trust roots, or bounds.
    pub fn new(settings: LdapProviderSettings) -> Result<Self, LdapProviderBuildError> {
        validate_settings(&settings)?;
        let custom_ca = settings.custom_ca_pem.is_some();
        let connection_settings = LdapConnSettings::new()
            .set_starttls(true)
            .set_conn_timeout(settings.connect_timeout)
            .set_config(Arc::new(tls_config(settings.custom_ca_pem.as_deref())?));
        let manager = LdapConnectionManager::new(settings.url.as_str())
            .map_err(|_| LdapProviderBuildError::InvalidUrl)?
            .with_connection_settings(connection_settings)
            .with_connect_timeout(settings.connect_timeout)
            .with_validation_timeout(settings.request_timeout.min(Duration::from_secs(1)));
        let bind = match settings.bind {
            LdapBindMode::Direct { dn_attribute } => RuntimeBindMode::Direct { dn_attribute },
            LdapBindMode::Search {
                username_attribute,
                bind_dn,
                bind_password,
            } => RuntimeBindMode::Search {
                username_attribute,
                bind_dn,
                bind_password,
            },
        };
        let max_connections = settings.max_connections.get();
        Ok(Self {
            id: settings.id,
            url: settings.url,
            base_dn: settings.base_dn,
            bind,
            subject_attribute: settings.subject_attribute,
            display_name_attribute: settings.display_name_attribute,
            group_attribute: settings.group_attribute,
            connect_timeout: settings.connect_timeout,
            request_timeout: settings.request_timeout,
            manager: SafeLdapManager(manager),
            pool: Arc::new(OnceLock::new()),
            max_connections,
            custom_ca,
        })
    }

    #[must_use]
    pub const fn id(&self) -> &ProviderId {
        &self.id
    }

    /// `Ok(None)` covers an unknown user, an empty credential, and a rejected bind. Callers receive
    /// errors for directory faults, while invalid credentials share one public response.
    ///
    /// # Errors
    /// Returns [`LdapProviderError`] when TLS, connection, search, response validation, or the request
    /// deadline fails.
    pub async fn authenticate(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<ExternalLogin>, LdapProviderError> {
        if username.is_empty() || password.is_empty() {
            return Ok(None);
        }
        tokio::time::timeout(self.request_timeout, self.authenticate_inner(username, password))
            .await
            .map_err(|_| LdapProviderError::Timeout)?
    }

    async fn authenticate_inner(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<ExternalLogin>, LdapProviderError> {
        let mut ldap = self.pool().get().await.map_err(|_| LdapProviderError::Unavailable)?;
        let entry = match &self.bind {
            RuntimeBindMode::Direct { dn_attribute } => {
                let dn = format!("{dn_attribute}={},{}", dn_escape(username), self.base_dn);
                ldap.discard = true;
                if !bind_user(&mut ldap.ldap, &dn, password).await? {
                    return Ok(None);
                }
                self.search_one(&mut ldap.ldap, &dn, Scope::Base, "(objectClass=*)")
                    .await?
            }
            RuntimeBindMode::Search {
                username_attribute,
                bind_dn,
                bind_password,
            } => {
                bind_identity(&mut ldap.ldap, bind_dn, bind_password).await?;
                let filter = format!("({username_attribute}={})", ldap_escape(username));
                let Some(entry) = self
                    .search_one(&mut ldap.ldap, &self.base_dn, Scope::Subtree, &filter)
                    .await?
                else {
                    return Ok(None);
                };
                ldap.discard = true;
                if !bind_user(&mut ldap.ldap, &entry.dn, password).await? {
                    return Ok(None);
                }
                Some(entry)
            }
        };
        entry.as_ref().map(|entry| self.external_login(entry)).transpose()
    }

    fn pool(&self) -> &Pool<SafeLdapManager> {
        self.pool.get_or_init(|| {
            Pool::builder()
                .max_size(self.max_connections)
                .connection_timeout(self.connect_timeout)
                .retry_connection(false)
                .build_unchecked(self.manager.clone())
        })
    }

    async fn search_one(
        &self,
        ldap: &mut Ldap,
        base: &str,
        scope: Scope,
        filter: &str,
    ) -> Result<Option<SearchEntry>, LdapProviderError> {
        let mut attributes = vec![self.subject_attribute.as_str(), self.display_name_attribute.as_str()];
        if let Some(group_attribute) = &self.group_attribute {
            attributes.push(group_attribute);
        }
        let SearchResult(mut entries, result) = ldap
            .with_search_options(SearchOptions::new().sizelimit(2))
            .search(base, scope, filter, attributes)
            .await
            .map_err(|_| LdapProviderError::Unavailable)?;
        if entries.len() > 1 || result.rc == LDAP_SIZE_LIMIT_EXCEEDED {
            return Err(LdapProviderError::AmbiguousUser);
        }
        if result.rc != 0 {
            return Err(LdapProviderError::Unavailable);
        }
        Ok(entries.pop().map(SearchEntry::construct))
    }

    fn external_login(&self, entry: &SearchEntry) -> Result<ExternalLogin, LdapProviderError> {
        let subject = single_attribute(entry, &self.subject_attribute)?;
        let display_name = single_attribute(entry, &self.display_name_attribute)?;
        if display_name.len() > MAX_DISPLAY_NAME_BYTES {
            return Err(LdapProviderError::InvalidEntry);
        }
        let group_values = self
            .group_attribute
            .as_deref()
            .and_then(|attribute| attribute_values(entry, attribute))
            .unwrap_or_default();
        let mut groups = group_values
            .iter()
            .map(|group| ExternalGroup::new(group).map_err(|_| LdapProviderError::InvalidEntry))
            .collect::<Result<Vec<_>, _>>()?;
        groups.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        groups.dedup();
        ExternalLogin::new(
            ExternalIdentity::new(
                self.id.clone(),
                ExternalSubject::new(subject).map_err(|_| LdapProviderError::InvalidEntry)?,
            ),
            UserName::new(display_name).map_err(|_| LdapProviderError::InvalidEntry)?,
            groups,
        )
        .map_err(|_| LdapProviderError::InvalidEntry)
    }
}

#[derive(Clone)]
pub struct LdapLoginService<S> {
    provider: LdapProvider,
    linker: ExternalIdentityLinker<S>,
    mappings: Arc<[ExternalGroupGrant]>,
}

impl<S> fmt::Debug for LdapLoginService<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LdapLoginService")
            .field("provider", &self.provider)
            .field("group_mappings", &self.mappings.len())
            .finish_non_exhaustive()
    }
}

impl<S: ExternalIdentityStore> LdapLoginService<S> {
    #[must_use]
    pub fn new(provider: LdapProvider, store: S, mappings: Vec<ExternalGroupGrant>) -> Self {
        Self {
            provider,
            linker: ExternalIdentityLinker::new(store),
            mappings: mappings.into(),
        }
    }
}

impl<S: ExternalIdentityStore + Sync> LdapLoginService<S> {
    #[must_use]
    pub const fn id(&self) -> &ProviderId {
        self.provider.id()
    }

    /// Commits local identity state after a successful directory bind.
    ///
    /// # Errors
    /// Returns [`LdapLoginError::Provider`] for directory failures and [`LdapLoginError::Store`] when
    /// the atomic local link cannot commit.
    pub async fn authenticate(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<ExternalIdentityResolution>, LdapLoginError<S::Error>> {
        let Some(login) = self
            .provider
            .authenticate(username, password)
            .await
            .map_err(LdapLoginError::Provider)?
        else {
            return Ok(None);
        };
        self.linker
            .link_or_resolve(&login, &self.mappings)
            .map(Some)
            .map_err(LdapLoginError::Store)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LdapProviderBuildError {
    #[error("LDAP URL must use ldap:// StartTLS without credentials, path, query, or fragment")]
    InvalidUrl,
    #[error("LDAP distinguished names must be non-empty and at most 4096 bytes")]
    InvalidDn,
    #[error("LDAP attribute names must be RFC 4512 key strings or numeric OIDs up to 128 bytes")]
    InvalidAttribute,
    #[error("LDAP service bind password must not be empty")]
    EmptyBindPassword,
    #[error("LDAP connection and request timeouts must be positive")]
    InvalidTimeout,
    #[error("LDAP custom CA file contains invalid PEM")]
    InvalidCa,
    #[error("LDAP custom CA file contains no certificates")]
    EmptyCa,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LdapProviderError {
    #[error("LDAP provider unavailable")]
    Unavailable,
    #[error("LDAP request timed out")]
    Timeout,
    #[error("LDAP user search returned more than one entry")]
    AmbiguousUser,
    #[error("LDAP user entry does not match the configured attribute mapping")]
    InvalidEntry,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum LdapLoginError<E> {
    #[error("LDAP provider failed: {0}")]
    Provider(LdapProviderError),
    #[error("external identity store failed: {0}")]
    Store(E),
}

#[derive(Clone)]
enum RuntimeBindMode {
    Direct {
        dn_attribute: String,
    },
    Search {
        username_attribute: String,
        bind_dn: String,
        bind_password: String,
    },
}

impl fmt::Debug for RuntimeBindMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct { dn_attribute } => formatter
                .debug_struct("Direct")
                .field("dn_attribute", dn_attribute)
                .finish(),
            Self::Search {
                username_attribute,
                bind_dn,
                ..
            } => formatter
                .debug_struct("Search")
                .field("username_attribute", username_attribute)
                .field("bind_dn", bind_dn)
                .field("bind_password", &"[redacted]")
                .finish(),
        }
    }
}

struct SafeLdap {
    ldap: Ldap,
    discard: bool,
}

#[derive(Clone)]
struct SafeLdapManager(LdapConnectionManager);

impl ManageConnection for SafeLdapManager {
    type Connection = SafeLdap;
    type Error = bb8_ldap::ldap3::LdapError;

    async fn connect(&self) -> Result<Self::Connection, Self::Error> {
        self.0.connect().await.map(|ldap| SafeLdap { ldap, discard: false })
    }

    async fn is_valid(&self, connection: &mut Self::Connection) -> Result<(), Self::Error> {
        self.0.is_valid(&mut connection.ldap).await
    }

    fn has_broken(&self, connection: &mut Self::Connection) -> bool {
        connection.discard || self.0.has_broken(&mut connection.ldap)
    }
}

async fn bind_identity(ldap: &mut Ldap, dn: &str, password: &str) -> Result<(), LdapProviderError> {
    ldap.simple_bind(dn, password)
        .await
        .map_err(|_| LdapProviderError::Unavailable)?
        .success()
        .map(|_| ())
        .map_err(|_| LdapProviderError::Unavailable)
}

async fn bind_user(ldap: &mut Ldap, dn: &str, password: &str) -> Result<bool, LdapProviderError> {
    let result = ldap
        .simple_bind(dn, password)
        .await
        .map_err(|_| LdapProviderError::Unavailable)?;
    if result.rc == LDAP_INVALID_CREDENTIALS {
        return Ok(false);
    }
    if result.rc != 0 {
        return Err(LdapProviderError::Unavailable);
    }
    Ok(true)
}

fn single_attribute<'a>(entry: &'a SearchEntry, attribute: &str) -> Result<&'a str, LdapProviderError> {
    let Some([value]) = attribute_values(entry, attribute) else {
        return Err(LdapProviderError::InvalidEntry);
    };
    Ok(value)
}

fn attribute_values<'a>(entry: &'a SearchEntry, attribute: &str) -> Option<&'a [String]> {
    entry
        .attrs
        .iter()
        .find_map(|(name, values)| name.eq_ignore_ascii_case(attribute).then_some(values.as_slice()))
}

fn validate_settings(settings: &LdapProviderSettings) -> Result<(), LdapProviderBuildError> {
    if settings.url.scheme() != "ldap"
        || settings.url.host_str().is_none()
        || !settings.url.username().is_empty()
        || settings.url.password().is_some()
        || settings.url.query().is_some()
        || settings.url.fragment().is_some()
        || !matches!(settings.url.path(), "" | "/")
    {
        return Err(LdapProviderBuildError::InvalidUrl);
    }
    validate_dn(&settings.base_dn)?;
    validate_attribute(&settings.subject_attribute)?;
    validate_attribute(&settings.display_name_attribute)?;
    if let Some(group_attribute) = &settings.group_attribute {
        validate_attribute(group_attribute)?;
    }
    match &settings.bind {
        LdapBindMode::Direct { dn_attribute } => validate_attribute(dn_attribute)?,
        LdapBindMode::Search {
            username_attribute,
            bind_dn,
            bind_password,
        } => {
            validate_attribute(username_attribute)?;
            validate_dn(bind_dn)?;
            if bind_password.is_empty() {
                return Err(LdapProviderBuildError::EmptyBindPassword);
            }
        }
    }
    if settings.connect_timeout.is_zero() || settings.request_timeout.is_zero() {
        return Err(LdapProviderBuildError::InvalidTimeout);
    }
    Ok(())
}

const fn validate_dn(dn: &str) -> Result<(), LdapProviderBuildError> {
    if dn.is_empty() || dn.len() > MAX_DN_BYTES {
        return Err(LdapProviderBuildError::InvalidDn);
    }
    Ok(())
}

fn validate_attribute(attribute: &str) -> Result<(), LdapProviderBuildError> {
    if attribute.is_empty() || attribute.len() > MAX_ATTRIBUTE_BYTES || !valid_attribute(attribute) {
        return Err(LdapProviderBuildError::InvalidAttribute);
    }
    Ok(())
}

fn valid_attribute(attribute: &str) -> bool {
    let bytes = attribute.as_bytes();
    if bytes[0].is_ascii_alphabetic() {
        return bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-');
    }
    attribute
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn tls_config(custom_ca_pem: Option<&[u8]>) -> Result<ClientConfig, LdapProviderBuildError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    if let Some(pem) = custom_ca_pem {
        let certificates = CertificateDer::pem_slice_iter(pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| LdapProviderBuildError::InvalidCa)?;
        if certificates.is_empty() {
            return Err(LdapProviderBuildError::EmptyCa);
        }
        for certificate in certificates {
            roots.add(certificate).map_err(|_| LdapProviderBuildError::InvalidCa)?;
        }
    }
    Ok(ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}
