use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use bytes::Bytes;
use futures_util::StreamExt as _;

use crate::{RangeError, RangeSession, UpstreamClient, UpstreamError};

#[derive(Debug, Clone)]
pub struct ArtifactClient {
    origin: UpstreamClient,
    mirror: Option<UpstreamClient>,
    fallback: bool,
}

impl ArtifactClient {
    const fn direct(origin: UpstreamClient) -> Self {
        Self {
            origin,
            mirror: None,
            fallback: false,
        }
    }

    const fn with_mirror(origin: UpstreamClient, mirror: UpstreamClient, fallback: bool) -> Self {
        Self {
            origin,
            mirror: Some(mirror),
            fallback,
        }
    }

    fn mirror_url(mirror: &UpstreamClient, url: &str) -> Result<url::Url, UpstreamError> {
        let original = url::Url::parse(url)?;
        Ok(mirror.base().join(original.path().trim_start_matches('/'))?)
    }

    /// Tries the mirror before the advertised URL when `fallback` is true.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] if no eligible source starts a successful response.
    pub async fn stream_bytes(
        &self,
        url: &str,
    ) -> Result<futures_util::stream::BoxStream<'static, Result<Bytes, UpstreamError>>, UpstreamError> {
        if let Some(mirror) = &self.mirror {
            let mirror_url = Self::mirror_url(mirror, url)?;
            match mirror.stream_bytes(mirror_url.as_str()).await {
                Ok(stream) => return Ok(stream.boxed()),
                Err(err) if !self.fallback => return Err(err),
                Err(_) => {}
            }
        }
        Ok(self.origin.stream_bytes(url).await?.boxed())
    }

    /// Tries the mirror before the advertised URL when `fallback` is true. Source selection happens
    /// once per session, so every range of one read comes from the representation this call pinned.
    ///
    /// # Errors
    /// Returns [`RangeError`] if no eligible source pins a representation.
    pub async fn range_session(&self, url: &str) -> Result<RangeSession, RangeError> {
        if let Some(mirror) = &self.mirror {
            let mirror_url = Self::mirror_url(mirror, url)?;
            match mirror.range_session(mirror_url.as_str()).await {
                Ok(session) => return Ok(session),
                Err(err) if !self.fallback => return Err(err),
                Err(_) => {}
            }
        }
        self.origin.range_session(url).await
    }
}

impl From<UpstreamClient> for ArtifactClient {
    fn from(client: UpstreamClient) -> Self {
        Self::direct(client)
    }
}

/// Health state from the latest completed request to one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamHealth {
    /// No request has completed in this process.
    Configured,
    /// The latest request found the source usable.
    Healthy,
    /// The latest request failed to use the source.
    Unhealthy,
}

impl UpstreamHealth {
    const fn value(self) -> u8 {
        match self {
            Self::Configured => 0,
            Self::Healthy => 1,
            Self::Unhealthy => 2,
        }
    }

    /// Returns stable text for operator surfaces.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NamedUpstream {
    name: String,
    client: UpstreamClient,
    artifacts: ArtifactClient,
    health: Arc<AtomicU8>,
}

impl NamedUpstream {
    #[must_use]
    pub fn new(name: impl Into<String>, client: UpstreamClient) -> Self {
        Self {
            name: name.into(),
            artifacts: ArtifactClient::direct(client.clone()),
            client,
            health: Arc::new(AtomicU8::new(UpstreamHealth::Configured.value())),
        }
    }

    /// Tries `mirror` before advertised artifact URLs when `fallback` is true.
    #[must_use]
    pub fn with_artifact_mirror(mut self, mirror: UpstreamClient, fallback: bool) -> Self {
        self.artifacts = ArtifactClient::with_mirror(self.client.clone(), mirror, fallback);
        self
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn client(&self) -> &UpstreamClient {
        &self.client
    }

    #[must_use]
    pub const fn artifacts(&self) -> &ArtifactClient {
        &self.artifacts
    }

    #[must_use]
    pub fn health(&self) -> UpstreamHealth {
        match self.health.load(Ordering::Acquire) {
            0 => UpstreamHealth::Configured,
            1 => UpstreamHealth::Healthy,
            _ => UpstreamHealth::Unhealthy,
        }
    }

    pub fn mark_healthy(&self) {
        self.health.store(UpstreamHealth::Healthy.value(), Ordering::Release);
    }

    pub fn mark_unhealthy(&self) {
        self.health.store(UpstreamHealth::Unhealthy.value(), Ordering::Release);
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RouteError {
    #[error("an upstream route needs at least one source")]
    Empty,
    #[error("upstream source names must not be empty")]
    EmptyName,
    #[error("duplicate upstream source {0:?}")]
    DuplicateName(String),
    #[error("route keys must not be empty")]
    EmptyKey,
    #[error("cannot pin key {key:?} to unknown upstream {upstream:?}")]
    UnknownPin { key: String, upstream: String },
}

/// Selects upstreams in operator order while enforcing key pins and fallback policy.
#[derive(Debug, Clone)]
pub struct UpstreamRouter {
    upstreams: Vec<NamedUpstream>,
    positions: HashMap<String, usize>,
    pins: HashMap<String, usize>,
    protected: HashSet<String>,
    fallback: bool,
}

impl UpstreamRouter {
    /// Treats the first source as primary.
    ///
    /// # Errors
    /// Returns [`RouteError`] if there are no sources or their names are empty or duplicated.
    pub fn new(upstreams: Vec<NamedUpstream>) -> Result<Self, RouteError> {
        if upstreams.is_empty() {
            return Err(RouteError::Empty);
        }
        let mut positions = HashMap::with_capacity(upstreams.len());
        for (position, upstream) in upstreams.iter().enumerate() {
            if upstream.name.is_empty() {
                return Err(RouteError::EmptyName);
            }
            if positions.insert(upstream.name.clone(), position).is_some() {
                return Err(RouteError::DuplicateName(upstream.name.clone()));
            }
        }
        Ok(Self {
            upstreams,
            positions,
            pins: HashMap::new(),
            protected: HashSet::new(),
            fallback: true,
        })
    }

    #[must_use]
    pub const fn with_fallback(mut self, fallback: bool) -> Self {
        self.fallback = fallback;
        self
    }

    /// Restricts `key` to `upstream`.
    ///
    /// # Errors
    /// Returns [`RouteError`] if the key is empty or the source is not part of this route.
    pub fn pin(mut self, key: impl Into<String>, upstream: &str) -> Result<Self, RouteError> {
        let key = key.into();
        if key.is_empty() {
            return Err(RouteError::EmptyKey);
        }
        let Some(&position) = self.positions.get(upstream) else {
            return Err(RouteError::UnknownPin {
                key,
                upstream: upstream.to_owned(),
            });
        };
        self.pins.insert(key, position);
        Ok(self)
    }

    /// Restricts `key` to the primary source.
    ///
    /// # Errors
    /// Returns [`RouteError::EmptyKey`] if the key is empty.
    pub fn protect(mut self, key: impl Into<String>) -> Result<Self, RouteError> {
        let key = key.into();
        if key.is_empty() {
            return Err(RouteError::EmptyKey);
        }
        self.protected.insert(key);
        Ok(self)
    }

    /// Yields sources eligible for `key` in request order.
    pub fn candidates<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a NamedUpstream> + 'a {
        let pinned = self.pins.get(key).copied();
        let fallback = self.fallback && !self.protected.contains(key);
        self.upstreams
            .iter()
            .enumerate()
            .filter(move |(position, _)| pinned.map_or(fallback || *position == 0, |pin| *position == pin))
            .map(|(_, upstream)| upstream)
    }

    /// Yields all configured sources in operator order, ignoring key routing rules.
    pub fn sources(&self) -> impl Iterator<Item = &NamedUpstream> {
        self.upstreams.iter()
    }

    #[must_use]
    pub fn source(&self, name: &str) -> Option<&NamedUpstream> {
        self.positions.get(name).map(|&position| &self.upstreams[position])
    }
}
