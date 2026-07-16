use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use bytes::Bytes;
use futures_util::StreamExt as _;

use crate::{RangeError, UpstreamClient, UpstreamError};

/// Artifact client for one metadata source and its optional mirror.
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

    /// Stream an artifact from its mirror, falling back to its advertised URL when configured.
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

    /// Whether either eligible artifact source may support byte ranges.
    #[must_use]
    pub fn may_support_ranges(&self) -> bool {
        self.mirror.as_ref().is_some_and(UpstreamClient::may_support_ranges)
            || (self.fallback || self.mirror.is_none()) && self.origin.may_support_ranges()
    }

    /// Stop range attempts for every eligible artifact source during this process.
    pub fn disable_ranges(&self) {
        if let Some(mirror) = &self.mirror {
            mirror.disable_ranges();
        }
        self.origin.disable_ranges();
    }

    /// Fetch artifact headers from the mirror, then the advertised URL when configured.
    ///
    /// # Errors
    /// Returns [`RangeError`] if no eligible source provides usable range metadata.
    pub async fn head_file_for_range(&self, url: &str) -> Result<crate::FileHead, RangeError> {
        if let Some(mirror) = &self.mirror {
            let mirror_url = Self::mirror_url(mirror, url)?;
            match mirror.head_file_for_range(mirror_url.as_str()).await {
                Ok(head) => return Ok(head),
                Err(err) if !self.fallback => return Err(err),
                Err(_) => {}
            }
        }
        self.origin.head_file_for_range(url).await
    }

    /// Fetch an artifact range from the mirror, then the advertised URL when configured.
    ///
    /// # Errors
    /// Returns [`RangeError`] if no eligible source provides the requested range.
    pub async fn fetch_range(&self, url: &str, start: u64, end: u64) -> Result<Bytes, RangeError> {
        if let Some(mirror) = &self.mirror {
            let mirror_url = Self::mirror_url(mirror, url)?;
            match mirror.fetch_range(mirror_url.as_str(), start, end).await {
                Ok(bytes) => return Ok(bytes),
                Err(err) if !self.fallback => return Err(err),
                Err(_) => {}
            }
        }
        self.origin.fetch_range(url, start, end).await
    }
}

impl From<UpstreamClient> for ArtifactClient {
    fn from(client: UpstreamClient) -> Self {
        Self::direct(client)
    }
}

/// The result of the latest completed request to one configured source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamHealth {
    /// No request has completed since process start.
    Configured,
    /// The latest request found a usable source.
    Healthy,
    /// The latest request found a transport, protocol, authentication, rate-limit, or server failure.
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

    /// Stable status text for operator surfaces.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Healthy => "healthy",
            Self::Unhealthy => "unhealthy",
        }
    }
}

/// One configured upstream and the name recorded as its source.
#[derive(Debug, Clone)]
pub struct NamedUpstream {
    name: String,
    client: UpstreamClient,
    artifacts: ArtifactClient,
    health: Arc<AtomicU8>,
}

impl NamedUpstream {
    /// Pair a configuration name with its client.
    #[must_use]
    pub fn new(name: impl Into<String>, client: UpstreamClient) -> Self {
        Self {
            name: name.into(),
            artifacts: ArtifactClient::direct(client.clone()),
            client,
            health: Arc::new(AtomicU8::new(UpstreamHealth::Configured.value())),
        }
    }

    /// Route artifacts through `mirror`, falling back to their advertised URLs when enabled.
    #[must_use]
    pub fn with_artifact_mirror(mut self, mirror: UpstreamClient, fallback: bool) -> Self {
        self.artifacts = ArtifactClient::with_mirror(self.client.clone(), mirror, fallback);
        self
    }

    /// The stable source name used in records and operator output.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The client for this source.
    #[must_use]
    pub const fn client(&self) -> &UpstreamClient {
        &self.client
    }

    /// The artifact client for this metadata source.
    #[must_use]
    pub const fn artifacts(&self) -> &ArtifactClient {
        &self.artifacts
    }

    /// Read the result of the latest completed request to this source.
    #[must_use]
    pub fn health(&self) -> UpstreamHealth {
        match self.health.load(Ordering::Acquire) {
            1 => UpstreamHealth::Healthy,
            2 => UpstreamHealth::Unhealthy,
            _ => UpstreamHealth::Configured,
        }
    }

    /// Record a request that found a usable source.
    pub fn mark_healthy(&self) {
        self.health.store(UpstreamHealth::Healthy.value(), Ordering::Release);
    }

    /// Record a request that could not use this source.
    pub fn mark_unhealthy(&self) {
        self.health.store(UpstreamHealth::Unhealthy.value(), Ordering::Release);
    }
}

/// Invalid upstream routing configuration.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RouteError {
    #[error("an upstream route needs at least one source")]
    Empty,
    #[error("upstream source names must not be empty")]
    EmptyName,
    #[error("duplicate upstream source {0:?}")]
    DuplicateName(String),
    #[error("project pins must not be empty")]
    EmptyProject,
    #[error("cannot pin project {project:?} to unknown upstream {upstream:?}")]
    UnknownPin { project: String, upstream: String },
}

/// Ordered upstream selection with strict package pins and fallback controls.
#[derive(Debug, Clone)]
pub struct UpstreamRouter {
    upstreams: Vec<NamedUpstream>,
    positions: HashMap<String, usize>,
    pins: HashMap<String, usize>,
    protected: HashSet<String>,
    fallback: bool,
}

impl UpstreamRouter {
    /// Build an ordered route. The first source is the primary.
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

    /// Enable or disable fallback after the primary source.
    #[must_use]
    pub const fn with_fallback(mut self, fallback: bool) -> Self {
        self.fallback = fallback;
        self
    }

    /// Route one canonical project name only to `upstream`.
    ///
    /// # Errors
    /// Returns [`RouteError`] if the project is empty or the source is not part of this route.
    pub fn pin(mut self, project: impl Into<String>, upstream: &str) -> Result<Self, RouteError> {
        let project = project.into();
        if project.is_empty() {
            return Err(RouteError::EmptyProject);
        }
        let Some(&position) = self.positions.get(upstream) else {
            return Err(RouteError::UnknownPin {
                project,
                upstream: upstream.to_owned(),
            });
        };
        self.pins.insert(project, position);
        Ok(self)
    }

    /// Prevent one canonical project name from falling past the primary source.
    ///
    /// # Errors
    /// Returns [`RouteError::EmptyProject`] if the project is empty.
    pub fn protect(mut self, project: impl Into<String>) -> Result<Self, RouteError> {
        let project = project.into();
        if project.is_empty() {
            return Err(RouteError::EmptyProject);
        }
        self.protected.insert(project);
        Ok(self)
    }

    /// Sources eligible for `project`, in request order.
    pub fn candidates<'a>(&'a self, project: &'a str) -> impl Iterator<Item = &'a NamedUpstream> + 'a {
        let pinned = self.pins.get(project).copied();
        let fallback = self.fallback && !self.protected.contains(project);
        self.upstreams
            .iter()
            .enumerate()
            .filter(move |(position, _)| pinned.map_or(fallback || *position == 0, |pin| *position == pin))
            .map(|(_, upstream)| upstream)
    }

    /// Every configured source in operator order, independent of package routing rules.
    pub fn sources(&self) -> impl Iterator<Item = &NamedUpstream> {
        self.upstreams.iter()
    }

    /// The configured source named `name`.
    #[must_use]
    pub fn source(&self, name: &str) -> Option<&NamedUpstream> {
        self.positions.get(name).map(|&position| &self.upstreams[position])
    }
}
