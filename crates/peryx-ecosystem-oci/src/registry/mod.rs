//! Resolving a `/v2/` request to a configured OCI index and serving the pull path.
//!
//! `<name>` in `/v2/<name>/...` carries the peryx index route as a prefix, so the longest OCI index
//! route that segment-aligns with `<name>` selects the index and the remainder is the upstream
//! repository, the same longest-prefix rule peryx resolves any index by. A proxy pulls through and caches;
//! a hosted index serves only what it stores; a virtual index walks its members hosted-first, so a
//! hosted image shadows the same name upstream (the dependency-confusion defense). Blobs and manifests
//! are content-addressed and immutable, so a cache hit skips the network; a tag is mutable, so an
//! online proxy serves it from cache within a freshness window and revalidates against upstream once
//! that window elapses.
//!
//! Store and blob-io faults propagate through [`ServeError`] so the serving methods read as the happy
//! path, and a single conversion turns a fault into a `502`.
#![allow(
    clippy::result_large_err,
    reason = "write helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use crate::error::{ErrorCode, error_response, gateway_error};
use crate::name::{OciRoute, classify};
use crate::settings::IndexSettings;
use crate::upload_session::UploadStore as _;
use crate::upstream::{Upstream, UpstreamError};
use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt as _;
use parking_lot::RwLock;
use peryx_core::Ecosystem;
use peryx_driver::ServingState;
use peryx_driver::serving::{
    ArtifactMemberDriver, BlobReferenceDriver, BrowseDriver, DriverCapabilities, EcosystemDriver, IdleReclaimer,
    MaintenanceCapabilities, MaintenanceDriver, ManifestDriver, MetricsDriver, RouteMount, TrashDriver,
};
use peryx_events::webhook::{WebhookEvent, WebhookEventKind};
use peryx_identity::{Action, ArtifactDigest, Denial, DigestDecision, Identity};
use peryx_index::{Index, IndexKind};
use peryx_policy::PolicyAction;
use peryx_storage::blob::BlobWrite;
use peryx_storage::meta::MetaError;
use peryx_upstream::UpstreamClient;
use std::borrow::Cow;
use std::collections::hash_map::RandomState;
use std::collections::{HashSet, VecDeque};
use std::hash::BuildHasher;
use std::net::SocketAddr;
use std::sync::Arc;

mod auth;
mod authority;
mod blobs;
mod discovery;
mod manifests;
mod uploads;
pub use blobs::download_blob;
use discovery::serve_catalog;
use manifests::{delete_manifest, put_manifest, restore_manifest};
/// The header a registry returns the canonical content digest in.
const DOCKER_CONTENT_DIGEST: HeaderName = HeaderName::from_static("docker-content-digest");
/// The header carrying an upload session's id.
const DOCKER_UPLOAD_UUID: HeaderName = HeaderName::from_static("docker-upload-uuid");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
/// The media type served for blob bytes.
const OCTET_STREAM: &str = "application/octet-stream";
/// The media type assumed when an upstream manifest response omits its content type.
const DEFAULT_MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
/// The largest manifest body accepted on a push or read through from an upstream. Manifests are small;
/// this only bounds abuse.
pub const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
/// The largest tag-list body read from an upstream. Tag lists are text; this bounds a hostile or
/// broken upstream that would otherwise stream an unbounded body into memory.
const MAX_TAGS_BYTES: usize = 4 * 1024 * 1024;
const BLOB_MEMBERSHIP_CACHE_BYTES: usize = 8 << 20;
/// The most upstream tag-list pages followed when aggregating, so a broken or hostile upstream whose
/// `Link` chain never ends cannot loop forever. Far more than any real repository paginates into.
const MAX_TAG_PAGES: usize = 100;
/// An internal fault while serving: the metadata store, blob io, or an upstream body transfer failed.
/// Client-visible outcomes (unknown manifest, invalid digest) are ordinary responses, not this.
#[derive(Debug)]
pub enum ServeError {
    Store(MetaError),
    Io(std::io::Error),
    Transport(String),
}
impl From<MetaError> for ServeError {
    fn from(err: MetaError) -> Self {
        Self::Store(err)
    }
}
impl From<std::io::Error> for ServeError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}
impl From<reqwest::Error> for ServeError {
    fn from(err: reqwest::Error) -> Self {
        Self::Transport(err.to_string())
    }
}
impl From<peryx_storage::meta::QuotaError> for ServeError {
    fn from(err: peryx_storage::meta::QuotaError) -> Self {
        Self::Transport(err.to_string())
    }
}
impl ServeError {
    /// A user-visible one-line description of the fault, for the web view methods that surface a
    /// message rather than a `/v2/` response.
    fn message(&self) -> String {
        match self {
            Self::Store(err) => format!("metadata store error: {err}"),
            Self::Io(err) => format!("blob io error: {err}"),
            Self::Transport(err) => format!("upstream transfer failed: {err}"),
        }
    }

    /// Every internal fault is a gateway error to the client; the specifics go to the log context.
    fn into_response(self) -> Response {
        match self {
            Self::Store(err) => gateway_error(&format!("metadata store error: {err}")),
            Self::Io(err) => gateway_error(&format!("blob io error: {err}")),
            Self::Transport(err) => gateway_error(&format!("upstream transfer failed: {err}")),
        }
    }
}
/// The web browse methods return a user-visible message, so a fault propagated with `?` reads as its
/// one-line description without a per-call-site closure.
impl From<ServeError> for String {
    fn from(err: ServeError) -> Self {
        err.message()
    }
}

/// The OCI/Docker registry driver.
///
/// Holds one shared upstream fetcher over the process's stores, the per-index settings the
/// composition root compiled, the in-progress blob uploads a hosted push accumulates across its
/// `POST`/`PATCH`/`PUT` requests.
pub type OciRegistry = OciRegistryWithHasher<RandomState>;

#[doc(hidden)]
#[derive(Default)]
pub struct OciRegistryWithHasher<S> {
    upstream: Upstream,
    settings: std::collections::HashMap<String, IndexSettings>,
    blob_memberships: RwLock<BlobMembershipCache<S>>,
    /// Serializes the read-modify-write of one upload session's durable stage, so two concurrent chunk
    /// writes to the same session cannot interleave on disk. Different sessions never contend.
    session_gate: SessionGate,
    /// Whether an authoritative hosted mutation records a typed operation in the driver-transaction
    /// outbox. Set from the availability mode: `false` under single-node `none`, so its write path is
    /// byte-for-byte the pre-outbox behavior.
    journal_outbox: bool,
}

/// A per-session async lock registry. A session's lock is created on first use and dropped once no
/// writer holds it, so the map tracks only in-flight sessions.
#[derive(Default)]
struct SessionGate {
    locks: std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl SessionGate {
    /// The lock guarding `session`, shared with any concurrent writer of the same session.
    fn lock(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().expect("session gate is never poisoned");
        Arc::clone(locks.entry(session.to_owned()).or_default())
    }

    /// Drop `session`'s lock once the caller's guard is the last reference, keeping the map bounded to
    /// sessions with a writer in flight.
    fn release(&self, session: &str) {
        let mut locks = self.locks.lock().expect("session gate is never poisoned");
        if locks.get(session).is_some_and(|lock| Arc::strong_count(lock) == 1) {
            locks.remove(session);
        }
    }
}
#[derive(Default)]
struct BlobMembershipCache<S> {
    entries: HashSet<Arc<str>, S>,
    insertion_order: VecDeque<Arc<str>>,
    key_bytes: usize,
}
impl<S: BuildHasher> BlobMembershipCache<S> {
    fn remove(&mut self, key: &str) {
        if self.entries.remove(key) {
            self.key_bytes -= key.len();
            self.insertion_order.retain(|entry| entry.as_ref() != key);
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.entries.contains(key)
    }

    fn insert(&mut self, key: String) {
        let key: Arc<str> = key.into();
        self.entries.insert(Arc::clone(&key));
        self.key_bytes += key.len();
        self.insertion_order.push_back(key);
        while self.key_bytes > BLOB_MEMBERSHIP_CACHE_BYTES {
            let oldest = self
                .insertion_order
                .pop_front()
                .expect("nonzero cache weight has an entry");
            self.entries.remove(&oldest);
            self.key_bytes -= oldest.len();
        }
    }
}
/// How long an open upload session may sit idle before it is reclaimed. A session untouched by its
/// `POST`, a `PATCH`, or a status `GET` for longer than this has its durable record and staged bytes
/// dropped, so a client that starts uploads and abandons them cannot pin disk forever.
const UPLOAD_SESSION_TTL_SECS: i64 = 3600;
/// The most idle sessions one reclamation pass removes, bounding its scan.
const UPLOAD_RECLAIM_BATCH: usize = 256;
impl<S: BuildHasher + Default + Send + Sync + 'static> OciRegistryWithHasher<S> {
    /// Build the driver with its shared upstream client and each OCI index's settings, keyed by index
    /// name. `journal_outbox` records authoritative hosted mutations for replication.
    #[must_use]
    pub fn new(settings: impl IntoIterator<Item = (String, IndexSettings)>, journal_outbox: bool) -> Self {
        Self {
            settings: settings.into_iter().collect(),
            journal_outbox,
            ..Self::default()
        }
    }

    /// The name `repo` is spelled with upstream on the cached index `index`, which is what the request
    /// path and the token scope must both carry. Everything peryx stores or shows keeps the client's
    /// spelling.
    fn upstream_repo<'a>(&self, index: &str, client: &UpstreamClient, repo: &'a str) -> Cow<'a, str> {
        let prefix = self.settings.get(index).copied().unwrap_or_default().library_prefix;
        crate::settings::upstream_repo(prefix, client.base_url(), repo)
    }

    fn random_session() -> Result<String, ServeError> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
        let mut session = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            session.push(HEX[usize::from(byte >> 4)] as char);
            session.push(HEX[usize::from(byte & 0x0f)] as char);
        }
        Ok(session)
    }
}
#[async_trait]
impl<S: BuildHasher + Default + Send + Sync + 'static> EcosystemDriver for OciRegistryWithHasher<S> {
    fn ecosystem(&self) -> peryx_core::Ecosystem {
        crate::ECOSYSTEM
    }

    fn capabilities(&self) -> DriverCapabilities<'_> {
        DriverCapabilities {
            metrics: Some(self),
            blob_references: Some(self),
            trash: Some(self),
            browse: Some(self),
            manifest: Some(self),
            artifact_members: Some(self),
            ..DriverCapabilities::default()
        }
    }

    fn mount(&self) -> RouteMount {
        RouteMount::Absolute(&["/v2/"])
    }

    async fn serve(&self, state: Arc<ServingState>, mut request: Request) -> Response {
        if state.signer.is_some()
            && (request.headers().contains_key(&X_FORWARDED_HOST) || request.headers().contains_key(&X_FORWARDED_PROTO))
            && !request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .is_some_and(|ConnectInfo(address)| state.rate_limits.trusts_proxy(address.ip()))
        {
            request.headers_mut().remove(&X_FORWARDED_HOST);
            request.headers_mut().remove(&X_FORWARDED_PROTO);
        }
        let path = request.uri().path();
        if matches!(request.method(), &Method::GET | &Method::HEAD) && (path == "/v2/" || path == "/v2") {
            return auth::negotiate_version(&state, request.headers());
        }
        self.serve_request(state, request).await
    }

    fn classify_route(&self, path: &str) -> peryx_driver::rate_limit::RouteClass {
        use peryx_driver::rate_limit::RouteClass;
        // A blob GET streams layer bytes, an artifact download; manifests, tags, referrers, and the
        // layer browser are listings. The version check and writes never reach here.
        match classify(path) {
            Some(OciRoute::Blob { .. }) => RouteClass::Artifact,
            _ => RouteClass::Listing,
        }
    }

    fn rate_limit_principal(
        &self,
        state: &ServingState,
        _position: Option<usize>,
        headers: &HeaderMap,
    ) -> peryx_identity::Principal {
        auth::rate_limit_principal(state, headers)
    }

    fn discover_index(
        &self,
        index: peryx_driver::state::IndexDescription,
        base: Option<&peryx_driver::discovery::BaseUrl>,
    ) -> serde_json::Value {
        crate::discovery::index_entry(index, base)
    }

    fn client_endpoint(&self, route: &str) -> String {
        let mut url = "/v2/".to_owned();
        peryx_core::url_encoding::push_path(&mut url, route);
        url.push('/');
        url
    }
}

impl<S: BuildHasher + Default + Send + Sync + 'static> MetricsDriver for OciRegistryWithHasher<S> {
    fn metric_families(&self) -> &'static [peryx_events::metrics::MetricFamily] {
        crate::quota::QUOTA_FAMILIES
    }
}

impl<S: BuildHasher + Default + Send + Sync + 'static> BlobReferenceDriver for OciRegistryWithHasher<S> {
    fn referenced_blob_digests(
        &self,
        meta: &peryx_storage::meta::MetaStore,
    ) -> Result<std::collections::BTreeSet<String>, String> {
        Ok(crate::referenced_blob_digests(meta).map_err(ServeError::from)?)
    }
}

impl<S: BuildHasher + Default + Send + Sync + 'static> TrashDriver for OciRegistryWithHasher<S> {
    fn trash_records(
        &self,
        meta: &peryx_storage::meta::MetaStore,
        index_names: &[String],
    ) -> Result<Vec<peryx_core::TrashRecord>, String> {
        let mut records = Vec::new();
        for index in index_names {
            records.extend(crate::store::trash_records(meta, index).map_err(ServeError::from)?);
        }
        Ok(records)
    }
}

#[async_trait]
impl<S: BuildHasher + Default + Send + Sync + 'static> BrowseDriver for OciRegistryWithHasher<S> {
    fn project_names(&self, state: &ServingState, position: usize) -> Result<Vec<String>, String> {
        Ok(repositories(state, state.index_at(position)).map_err(ServeError::from)?)
    }

    async fn browse_project(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
    ) -> Result<Option<peryx_core::UiProjectView>, String> {
        let index = state.index_at(position);
        let names = self.repository_tags(&state, index, &project).await?;
        Ok(Some(peryx_core::UiProjectView::References { names }))
    }
}

#[async_trait]
impl<S: BuildHasher + Default + Send + Sync + 'static> ManifestDriver for OciRegistryWithHasher<S> {
    async fn manifest_view(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
        reference: String,
    ) -> Result<Option<peryx_core::UiManifest>, String> {
        let name = full_name(&state.index_at(position).route, &project);
        let Some(reference) = crate::name::parse_reference(&reference) else {
            return Ok(None);
        };
        // The browse view renders the index document itself, so it opts out of the legacy Accept
        // rewrite that would swap an index for its child.
        let response = self.serve_manifest(&state, &name, &reference, false, None).await?;
        if response.status() != StatusCode::OK {
            return Ok(None);
        }
        let bytes = read_body(response.into_body(), MAX_MANIFEST_BYTES).await?;
        let mut manifest = crate::web::manifest_from_bytes(&bytes)?;
        manifest.client_command = Some(crate::web::pull_command(&name, &reference));
        Ok(Some(manifest))
    }
}

#[async_trait]
impl<S: BuildHasher + Default + Send + Sync + 'static> ArtifactMemberDriver for OciRegistryWithHasher<S> {
    async fn artifact_members(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
        digest: String,
    ) -> Result<Vec<peryx_core::UiMember>, String> {
        let name = full_name(&state.index_at(position).route, &project);
        let response = self.serve_layer_contents(&state, &name, &digest, "").await?;
        if !response.status().is_success() {
            return Err(layer_error_message(&name, &digest, response).await);
        }
        let bytes = read_body(response.into_body(), 8 << 20).await?;
        crate::web::members_from_bytes(&bytes)
    }

    async fn artifact_member_chunk(
        &self,
        state: Arc<ServingState>,
        position: usize,
        project: String,
        digest: String,
        member: String,
        offset: u64,
    ) -> Result<peryx_core::UiMemberChunk, String> {
        let name = full_name(&state.index_at(position).route, &project);
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("member", &member)
            .append_pair("offset", &offset.to_string())
            .finish();
        let response = self.serve_layer_contents(&state, &name, &digest, &query).await?;
        if !response.status().is_success() {
            return Err(layer_error_message(&name, &digest, response).await);
        }
        let size = crate::web::header_u64(response.headers(), "x-peryx-member-size");
        let chunk_offset = crate::web::header_u64(response.headers(), "x-peryx-member-offset").unwrap_or_default();
        let next_offset = crate::web::header_u64(response.headers(), "x-peryx-next-offset");
        let bytes = read_body(response.into_body(), 4 << 20).await?;
        Ok(peryx_core::UiMemberChunk {
            text: decode_member_text(&bytes, &member, &name, &digest)?,
            size,
            offset: chunk_offset,
            next_offset,
        })
    }
}
#[async_trait]
impl<S: BuildHasher + Default + Send + Sync + 'static> MaintenanceDriver for OciRegistryWithHasher<S> {
    fn ecosystem(&self) -> Ecosystem {
        crate::ECOSYSTEM
    }

    fn maintenance_capabilities(&self) -> MaintenanceCapabilities<'_> {
        MaintenanceCapabilities {
            idle_reclaimer: Some(self),
            ..MaintenanceCapabilities::default()
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/registry/maintenance_contract_tests.rs"]
mod maintenance_contract_tests;

#[async_trait]
impl<S: BuildHasher + Default + Send + Sync + 'static> IdleReclaimer for OciRegistryWithHasher<S> {
    async fn reclaim_idle(&self, state: Arc<ServingState>) -> usize {
        let cutoff = (state.clock)().saturating_sub(UPLOAD_SESSION_TTL_SECS);
        let expired = state
            .meta
            .reclaim_uploads(cutoff, UPLOAD_RECLAIM_BATCH)
            .unwrap_or_default();
        for session in &expired {
            let _ = state.blobs.discard_upload(session).await;
        }
        expired.len()
    }
}

impl<S: BuildHasher + Default + Send + Sync + 'static> OciRegistryWithHasher<S> {
    async fn serve_request(&self, state: Arc<ServingState>, request: Request) -> Response {
        let method = request.method().clone();
        let (parts, body) = request.into_parts();
        let path = parts.uri.path();
        let read = matches!(method, Method::GET | Method::HEAD);
        if method == Method::GET && (path == "/v2/token" || path == "/v2/token/") {
            return auth::issue_token(&state, &parts.headers, parts.uri.query().unwrap_or_default());
        }
        let Some(route) = classify(path) else {
            return error_response(ErrorCode::NameUnknown, "repository name unknown");
        };
        let governed_read = read
            && matches!(
                &route,
                OciRoute::Manifest { .. }
                    | OciRoute::Blob { .. }
                    | OciRoute::BlobContents { .. }
                    | OciRoute::TagsList { .. }
                    | OciRoute::Referrers { .. }
            );
        if read
            && let Err(response) = match &route {
                OciRoute::Catalog => auth::authorize_catalog(&state, &parts.headers),
                route => read_name(route).map_or(Ok(()), |name| auth::authorize_read(&state, &parts.headers, name)),
            }
        {
            let mut response = response;
            if governed_read {
                apply_revocation_cache_policy(&mut response, parts.headers.contains_key(header::AUTHORIZATION));
            }
            return response;
        }
        let headers = &parts.headers;
        let query = parts.uri.query().unwrap_or_default();
        let head = method == Method::HEAD;
        let authenticated = headers.contains_key(header::AUTHORIZATION);
        let result = match route {
            OciRoute::Manifest { name, reference } if read => {
                let accept = combined_accept(headers);
                self.serve_manifest(&state, &name, &reference, head, accept.as_deref())
                    .await
            }
            OciRoute::Manifest { name, reference } if method == Method::PUT => {
                put_manifest(&state, headers, body, &name, &reference, self.journal_outbox).await
            }
            OciRoute::Manifest { name, reference } if method == Method::DELETE => {
                delete_manifest(&state, headers, &name, &reference, query, self.journal_outbox).await
            }
            OciRoute::ManifestRestore { name, reference } if method == Method::PUT => {
                restore_manifest(&state, headers, &name, &reference, self.journal_outbox).await
            }
            OciRoute::Blob { name, digest } if read => self.serve_blob(&state, &name, &digest, head, headers).await,
            OciRoute::Blob { name, digest } if method == Method::DELETE => {
                self.delete_blob(&state, headers, &name, &digest)
            }
            OciRoute::BlobContents { name, digest } if method == Method::GET => {
                self.serve_layer_contents(&state, &name, &digest, query).await
            }
            OciRoute::Catalog if method == Method::GET => serve_catalog(&state, query),
            OciRoute::TagsList { name } if method == Method::GET => self.serve_tags(&state, &name, query).await,
            OciRoute::Referrers { name, digest } if read => self.serve_referrers(&state, &name, &digest, query).await,
            OciRoute::UploadStart { name } if method == Method::POST => {
                self.start_upload(&state, headers, query, &name, body).await
            }
            OciRoute::UploadSession { name, session } if method == Method::GET => {
                Self::upload_status(&state, headers, &name, &session)
            }
            OciRoute::UploadSession { name, session } if method == Method::PATCH => {
                self.patch_upload(&state, headers, &name, &session, body).await
            }
            OciRoute::UploadSession { name, session } if method == Method::PUT => {
                self.finish_upload(&state, headers, query, &name, &session, body).await
            }
            OciRoute::UploadSession { name, session } if method == Method::DELETE => {
                self.cancel_upload(&state, headers, &name, &session).await
            }
            _ => Ok(error_response(ErrorCode::Unsupported, "operation not supported")),
        };
        let mut response = result.unwrap_or_else(ServeError::into_response);
        if governed_read {
            apply_revocation_cache_policy(&mut response, authenticated);
        }
        response
    }

    /// Every tag of `repo` on `index`, unioned across a virtual index's members and each proxy
    /// member's upstream, sorted and distinct - the same union [`Self::serve_tags`] paginates.
    async fn repository_tags(
        &self,
        state: &ServingState,
        index: &Index,
        repo: &str,
    ) -> Result<Vec<String>, ServeError> {
        let active = state.revocations.has_active()?;
        let members = serving_members(state, index);
        let name = if index.route.is_empty() {
            repo.to_owned()
        } else {
            format!("{}/{repo}", index.route)
        };
        Ok(self
            .visible_tag_names(state, &name, repo, active, &members)
            .await?
            .into_iter()
            .collect())
    }
}

fn digest_decision(state: &ServingState, digest: &str) -> Result<DigestDecision, ServeError> {
    if !state.revocations.has_active()? {
        return Ok(DigestDecision::Clear);
    }
    let Ok(digest) = digest.parse::<ArtifactDigest>() else {
        return Ok(DigestDecision::Clear);
    };
    Ok(state.revocations.decision(&digest)?)
}

fn apply_revocation_cache_policy(response: &mut Response, authenticated: bool) {
    let value = if response.status().is_success() {
        format!(
            "{}, max-age={}, must-revalidate, no-transform",
            if authenticated { "private" } else { "public" },
            peryx_driver::revocations::DECISION_CACHE_TTL_SECS,
        )
    } else {
        "no-store".to_owned()
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_str(&value).expect("cache policy is a valid header"),
    );
}

/// The repositories `index` serves for the web index listing: a cached or hosted index reads its own
/// store; a virtual index unions its members'.
fn repositories(state: &ServingState, index: &Index) -> Result<Vec<String>, MetaError> {
    let mut repos = std::collections::BTreeSet::new();
    collect_repositories(state, index, &mut repos)?;
    Ok(repos.into_iter().collect())
}

fn collect_repositories(
    state: &ServingState,
    index: &Index,
    repos: &mut std::collections::BTreeSet<String>,
) -> Result<(), MetaError> {
    match &index.kind {
        IndexKind::Cached { .. } | IndexKind::Hosted { .. } => {
            repos.extend(crate::store::list_repositories(&state.meta, &index.name)?);
        }
        IndexKind::Virtual { layers, .. } => {
            for &position in layers {
                collect_repositories(state, state.index_at(position), repos)?;
            }
        }
    }
    Ok(())
}

/// The full `/v2/` name for a repository on an index: the index route prefixes the repository, matching
/// how a client addresses it, so [`resolve`] resolves it back to this index.
fn full_name(route: &str, repo: &str) -> String {
    if route.is_empty() {
        repo.to_owned()
    } else {
        format!("{route}/{repo}")
    }
}

/// A user-visible message for a non-success layer-browser response, reading its status and body.
async fn layer_error_message(name: &str, digest: &str, response: Response) -> String {
    let status = response.status();
    match axum::body::to_bytes(response.into_body(), 1 << 20).await {
        Ok(bytes) => format!(
            "layer contents for {digest} on {name:?}: {status}: {}",
            String::from_utf8_lossy(&bytes)
        ),
        Err(err) => format!("layer contents for {digest} on {name:?}: {status}: {err}"),
    }
}
/// Read a response body into memory, capped, mapping an over-cap or transfer failure to a user-visible
/// message. One helper, so the web browse methods carry no per-call-site error closure.
///
/// # Errors
/// Returns a message when the body exceeds `cap` or the transfer fails.
async fn read_body(body: Body, cap: usize) -> Result<bytes::Bytes, String> {
    axum::body::to_bytes(body, cap).await.map_err(|err| err.to_string())
}
/// Decode a previewed layer member's bytes as UTF-8 text, naming the member and layer on failure.
///
/// # Errors
/// Returns a message when the bytes are not valid UTF-8.
fn decode_member_text(bytes: &[u8], member: &str, name: &str, digest: &str) -> Result<String, String> {
    String::from_utf8(bytes.to_vec())
        .map_err(|err| format!("layer member {member:?} on {name:?} for {digest} is not valid UTF-8: {err}"))
}
/// Resolve the writable hosted index behind `name` and authorize `action` on the repository it names,
/// or return a ready error response (unknown name, read-only index, uploads disabled, or a credential
/// the ACL refuses). A virtual index routes the write to its upload-target member.
fn resolve_writable<'a>(
    state: &'a ServingState,
    name: &str,
    headers: &HeaderMap,
    action: Action,
) -> Result<(&'a Index, String, Identity), Response> {
    let Some((index, repo)) = resolve(&state.indexes, name) else {
        return Err(error_response(ErrorCode::NameUnknown, "repository name unknown"));
    };
    let target = match &index.kind {
        IndexKind::Hosted { .. } => index,
        IndexKind::Virtual { upload: Some(pos), .. } => state.index_at(*pos),
        _ => return Err(error_response(ErrorCode::Denied, "index is read-only")),
    };
    if !matches!(target.kind, IndexKind::Hosted { .. }) {
        return Err(error_response(ErrorCode::Denied, "index is read-only"));
    }
    let presented = auth::identify(state, &target.acl, headers);
    match presented.authorize(&target.acl, repo, name, action) {
        Ok(()) => Ok((target, repo.to_owned(), presented.into_identity())),
        Err(Denial::Unavailable) => Err(error_response(ErrorCode::Denied, "uploads are disabled")),
        Err(denial) => Err(auth::resource_challenge(
            state,
            headers,
            name,
            action,
            denial,
            presented.bad_token(),
        )),
    }
}

/// The repository `<name>` a readable route addresses, which its index authorizes the read against
/// before the handler runs; `None` for the registry-wide catalog, which is not repository-scoped.
fn read_name(route: &OciRoute) -> Option<&str> {
    match route {
        OciRoute::Manifest { name, .. }
        | OciRoute::Blob { name, .. }
        | OciRoute::BlobContents { name, .. }
        | OciRoute::TagsList { name }
        | OciRoute::Referrers { name, .. } => Some(name),
        _ => None,
    }
}
/// Whether the index's policy blocks this repository name. A blocked image is hidden on reads (served
/// as absent, like any policy-denied artifact) and refused on writes. The image name is the neutral
/// [`check_project`](peryx_policy::Policy::check_project) input, so an OCI index reuses the neutral
/// allow/block-list machinery every ecosystem shares.
fn policy_blocks(index: &Index, action: PolicyAction, repo: &str) -> bool {
    index.policy.check_project(action, repo).is_err()
}
/// Refuse a blob whose repository is blocked or whose size exceeds the index's `max_file_size_bytes`,
/// the neutral upload rules an OCI index shares with every ecosystem. `None` lets the write proceed.
fn policy_size_denial(index: &Index, repo: &str, size: u64) -> Option<Response> {
    index
        .policy
        .check_size(PolicyAction::Upload, repo, size)
        .err()
        .map(|denial| error_response(ErrorCode::Denied, &denial.to_string()))
}
/// The members a request serves from, in shadowing order; any non-virtual index is its own single
/// member. The order comes from the neutral role engine, so an OCI image shadows upstream by the same
/// rule a `PyPI` wheel does.
pub fn serving_members<'a>(state: &'a ServingState, index: &'a Index) -> Vec<&'a Index> {
    let IndexKind::Virtual { layers, .. } = &index.kind else {
        return vec![index];
    };
    peryx_index::shadow_order(&state.indexes, layers)
        .into_iter()
        .map(|position| state.index_at(position))
        .collect()
}
/// A hosted index's own page serial: the whole-store serial the readable frontier governs. A proxied
/// index reports upstream state the frontier does not govern, and a virtual index carries no serial of
/// its own - [`holds_below_readable_frontier`] derives its effective serial from its hosted members - so
/// both report none here. Mirrors the `PyPI` hosted-page rule.
fn hosted_last_serial(state: &ServingState, index: &Index) -> Result<Option<u64>, ServeError> {
    Ok(match index.kind {
        IndexKind::Hosted { .. } => Some(state.meta.current_serial()?),
        IndexKind::Cached { .. } | IndexKind::Virtual { .. } => None,
    })
}
/// Whether a replica must hold a mutable derived view (a tag resolution, tag list, or referrers list)
/// until its required views catch the authority serial. A hosted index's serial is a local journal
/// serial the frontier governs. A proxied index reports upstream state, which it does not, so it is
/// never held. A virtual index has no serial of its own but may surface a hosted member's manifest, so
/// it inherits its hosted members' serials: the gate holds it until every hosted member it layers is
/// readable. A serial past an unreadable frontier (which reads as zero), and a member whose serial
/// cannot be read, are both held, so a replica fails closed. Mirrors the `PyPI` hosted-page gate exactly.
fn holds_below_readable_frontier(state: &ServingState, index: &Index, last_serial: Option<u64>) -> bool {
    if !state.read_only {
        return false;
    }
    let frontier = state.readable_frontier().unwrap_or_default().serial;
    match &index.kind {
        IndexKind::Hosted { .. } => last_serial.is_some_and(|serial| serial > frontier),
        IndexKind::Cached { .. } => false,
        // A virtual index is readable only up to the least-readable of its members, so it holds until
        // every hosted member it layers is readable: the max over its hosted members' serials past the
        // frontier. Hosted members share the local journal, so that max is the store's current serial,
        // which a member whose read fails counts as past the frontier, failing closed.
        IndexKind::Virtual { layers, .. } => {
            peryx_index::layers_include_hosted(&state.indexes, layers)
                && state.meta.current_serial().map_or(true, |serial| serial > frontier)
        }
    }
}
/// Whether the first member with repository state for a digest has hidden it. A lower tombstone does
/// not override a live digest in a higher member.
fn manifest_trashed_in(state: &ServingState, members: &[&Index], repo: &str, digest: &str) -> Result<bool, ServeError> {
    for member in members {
        if crate::store::manifest_is_trashed(&state.meta, &member.name, repo, digest)? {
            return Ok(true);
        }
        if crate::store::manifest_is_member(&state.meta, &member.name, repo, digest)? {
            return Ok(false);
        }
    }
    Ok(false)
}
/// Enqueue a webhook for an OCI mutation on a hosted index. `version` is the tag when a tagged
/// reference was affected; `digest` is the manifest or blob digest. The webhook subsystem is neutral,
/// so a hosted OCI index delivers push and delete events like any hosted index.
fn emit_webhook(
    state: &Arc<ServingState>,
    request: &Requester<'_>,
    kind: WebhookEventKind,
    index: &Index,
    repo: &str,
    version: Option<String>,
    digest: Option<String>,
) {
    peryx_events::webhook::emit(
        Arc::clone(state),
        &WebhookEvent {
            kind,
            created_at_unix: (state.clock)(),
            index: index.name.clone(),
            route: index.route.clone(),
            hosted_index: index.name.clone(),
            project: repo.to_owned(),
            version,
            filename: digest.clone(),
            digest,
            count: 1,
            actor: peryx_events::security::actor(request.identity),
            request_id: request_id(request.headers),
        },
    );
}

/// Who made a mutating request and which request it was: the two audit facts a webhook carries.
struct Requester<'a> {
    headers: &'a HeaderMap,
    identity: &'a Identity,
}
/// The client-supplied request id, echoed into webhook deliveries for correlation.
fn request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}
/// The API version check's success answer: `200` with the distribution API-version header.
fn version_ok() -> Response {
    (
        [(
            HeaderName::from_static("docker-distribution-api-version"),
            HeaderValue::from_static("registry/2.0"),
        )],
        StatusCode::OK,
    )
        .into_response()
}
/// The per-blob lock concurrent misses share so a single upstream fetch serves them all, the same
/// single-flight coalescing every cached fetch shares. Keyed in its own namespace on the blob digest.
fn flight_gate(state: &ServingState, key: &str) -> peryx_index::serving::FlightGate {
    peryx_index::serving::flight_gate(&state.cache.inflight, key)
}
/// Find the OCI index whose route is the longest segment-aligned prefix of `name`, and the upstream
/// repository (the remainder). An empty route matches at the root, losing every tie to a real prefix.
fn resolve<'a, 'b>(indexes: &'a [Index], name: &'b str) -> Option<(&'a Index, &'b str)> {
    let mut best: Option<(&'a Index, &'b str)> = None;
    for index in indexes {
        if index.ecosystem != crate::ECOSYSTEM {
            continue;
        }
        let repo = if index.route.is_empty() {
            name
        } else {
            match name.strip_prefix(&index.route).and_then(|rest| rest.strip_prefix('/')) {
                Some(rest) if !rest.is_empty() => rest,
                _ => continue,
            }
        };
        if best.is_none_or(|(current, _)| index.route.len() > current.route.len()) {
            best = Some((index, repo));
        }
    }
    best
}
/// Parse a raw query string into its percent-decoded `key=value` pairs. Clients percent-encode the
/// colon in `digest=sha256:…`, so decoding is required, not cosmetic.
fn query_params(query: &str) -> std::collections::HashMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}
/// A bare `202 Accepted` for a completed delete.
fn accepted() -> Response {
    (StatusCode::ACCEPTED, Body::empty()).into_response()
}
/// Combine every `Accept` field line into the one comma-separated media-range list RFC 9110 says
/// repeated field lines form, so a list media type named on a later line still negotiates. `None`
/// when the client sent no parseable `Accept`, its way of expressing no preference.
fn combined_accept(headers: &HeaderMap) -> Option<String> {
    let joined = headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join(", ");
    (!joined.is_empty()).then_some(joined)
}
/// Whether something fetched at `fetched_at` may still answer while the upstream cannot confirm it.
///
/// The same bound a stale `PyPI` page gets: serve past the freshness window while an upstream is
/// down, but not without end. `0` removes the bound.
fn within_stale_bound(state: &ServingState, fetched_at: i64) -> bool {
    peryx_index::serving::within_stale_bound((state.clock)(), state.max_stale_secs, fetched_at, state.ttl_secs)
}

/// Read an upstream response body into memory, refusing one larger than `max`. A caching proxy holds
/// the whole body to hash or re-serve it, so an unbounded read would let a hostile or broken upstream
/// drive peryx out of memory.
pub async fn bounded_body(response: reqwest::Response, max: usize) -> Result<bytes::Bytes, ServeError> {
    let mut stream = response.bytes_stream();
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > max {
            return Err(ServeError::Transport(format!("upstream body exceeds {max} bytes")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(bytes::Bytes::from(body))
}
/// Turn an upstream fetch failure into a client response. A rate limit becomes a `429` carrying the
/// upstream's `Retry-After`, so the client backs off instead of hammering; a `401` says the upstream
/// refused peryx's credentials, which is the client's real cause, not a missing artifact; anything
/// else is a `502`, a fault between peryx and its upstream.
fn upstream_error_response(err: &UpstreamError, what: &str) -> Response {
    match err {
        UpstreamError::RateLimited(retry_after) => {
            let mut response = error_response(ErrorCode::TooManyRequests, "upstream rate limit reached; retry later");
            if let Some(value) = retry_after
                .as_deref()
                .and_then(|value| HeaderValue::from_str(value).ok())
            {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
        UpstreamError::Status(StatusCode::UNAUTHORIZED) => error_response(
            ErrorCode::Unauthorized,
            &format!("upstream registry refused authentication for this {what}"),
        ),
        _ => gateway_error(&format!("upstream {what} fetch failed: {err}")),
    }
}
/// Whether an upstream status means "this member does not have it" rather than a fault: `404`, and
/// also `403` because a registry answers that for a repository it will not show anonymously; either
/// way the member cannot serve it, so a virtual index walks on and a lone proxy reports the artifact
/// unknown. A `401` is not absence: it is the upstream refusing peryx, and folding it in here would
/// report a legible auth failure as `manifest unknown`.
fn absent_upstream(status: StatusCode) -> bool {
    matches!(status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN)
}
#[cfg(test)]
#[path = "../../tests/unit/registry/tests.rs"]
mod tests;
