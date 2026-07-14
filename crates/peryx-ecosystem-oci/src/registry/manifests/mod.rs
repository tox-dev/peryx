//! Manifest pull and tag revalidation: the read path. The write path (push, referrers, delete) is in
//! the `write` submodule.

mod write;
pub(super) use write::{delete_manifest, put_manifest};

use super::*;
use crate::error::{ErrorCode, error_response};
use crate::name::Reference;
use crate::store::{self, Manifest};
use crate::upstream::UpstreamError;
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use peryx_driver::ServingState;
use peryx_events::metrics::Event;
use peryx_index::Index;
use peryx_policy::PolicyAction;
use peryx_storage::blob::Digest;
use peryx_upstream::UpstreamClient;

impl<S: BuildHasher + Default + Send + Sync + 'static> OciRegistryWithHasher<S> {
    /// Serve a manifest by tag or digest. A virtual index walks its members hosted-first, so a hosted
    /// image shadows the same name upstream; a single hosted or proxy index is the one-member case.
    pub(super) async fn serve_manifest(
        &self,
        state: &ServingState,
        name: &str,
        reference: &Reference,
        head: bool,
        accept: Option<&str>,
    ) -> Result<Response, ServeError> {
        let Some((index, repo)) = resolve(&state.indexes, name) else {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        };
        if policy_blocks(index, PolicyAction::Serve, repo) {
            return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
        }
        let response = match reference {
            Reference::Digest(digest) => {
                let members = serving_members(state, index);
                let mut served = if manifest_authorized(state, &members, repo, digest)? {
                    store::get_manifest(&state.meta, digest)?.map(|manifest| manifest_response(manifest, digest, head))
                } else {
                    None
                };
                if served.is_none() {
                    for member in &members {
                        if let Some(client) = member.proxy_client() {
                            served = self
                                .pull_manifest_by_digest(state, client, &member.name, repo, digest, head)
                                .await?;
                            if served.is_some() {
                                break;
                            }
                        }
                    }
                }
                served.unwrap_or_else(|| error_response(ErrorCode::ManifestUnknown, "manifest unknown"))
            }
            Reference::Tag(tag) => {
                let mut served = None;
                for member in serving_members(state, index) {
                    if served.is_some() {
                        break;
                    }
                    served = self.member_tag(state, member, repo, tag, head).await?;
                }
                served.unwrap_or_else(|| error_response(ErrorCode::ManifestUnknown, "manifest unknown"))
            }
        };
        let response = self
            .negotiate_manifest(state, index, repo, accept, response, head)
            .await?;
        // A served manifest is this ecosystem's index document, so it counts as a page like a Simple
        // page does; a HEAD is a metadata check and carries no body, so it does not.
        if !head && response.status() == StatusCode::OK {
            state.metrics.record(Event::Page {
                route: index.route.clone(),
                project: repo.to_owned(),
            });
        }
        Ok(response)
    }

    /// Rewrite an index response to its `linux/amd64` child when the client will not accept a list
    /// media type, the substitution `distribution` makes for legacy Docker (< 17.06) that sends only
    /// the schema-2 image type. An `Accept` that is absent or lists an index type, a non-index
    /// response, or an index without a `linux/amd64` child all serve the resolved manifest unchanged.
    async fn negotiate_manifest(
        &self,
        state: &ServingState,
        index: &Index,
        repo: &str,
        accept: Option<&str>,
        response: Response,
        head: bool,
    ) -> Result<Response, ServeError> {
        if response.status() != StatusCode::OK
            || !accept.is_some_and(accept_excludes_list_types)
            || !response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(is_list_media_type)
        {
            return Ok(response);
        }
        let digest = response
            .headers()
            .get(DOCKER_CONTENT_DIGEST)
            .and_then(|value| value.to_str().ok())
            .expect("a served manifest carries its content digest")
            .to_owned();
        let list = store::get_manifest(&state.meta, &digest)?.expect("a served manifest is stored under its digest");
        let Some(child) = store::linux_amd64_child(&list.bytes) else {
            return Ok(response);
        };
        if let Some(manifest) = store::get_manifest(&state.meta, &child)? {
            return Ok(manifest_response(manifest, &child, head));
        }
        for member in serving_members(state, index) {
            if let Some(client) = member.proxy_client()
                && let Some(served) = self
                    .pull_manifest_by_digest(state, client, &member.name, repo, &child, head)
                    .await?
            {
                return Ok(served);
            }
        }
        Ok(response)
    }

    /// Try one member for a manifest by digest. `None` means this member does not have it (a `404`),
    /// so the caller moves to the next; `Some` is a served manifest or a real error to surface.
    ///
    /// A manifest by digest is immutable, so concurrent pulls of the same digest (the fan-out into an
    /// image index's per-platform children) single-flight through the gate: the first fetches and
    /// stores it, the rest re-read the store and skip the upstream round trip.
    async fn pull_manifest_by_digest(
        &self,
        state: &ServingState,
        client: &UpstreamClient,
        index: &str,
        repo: &str,
        digest: &str,
        head: bool,
    ) -> Result<Option<Response>, ServeError> {
        let gate_key = format!("oci\u{0}manifest\u{0}{digest}");
        let gate = flight_gate(state, &gate_key);
        let _guard = gate.lock().await;
        if let Some(manifest) = store::get_manifest(&state.meta, digest)? {
            return Ok(Some(manifest_response(manifest, digest, head)));
        }
        let fetched = self
            .fetch_manifest_by_digest(state, client, index, repo, digest, head)
            .await;
        state.cache.forget_flight(&gate_key);
        fetched
    }

    /// Fetch a manifest by digest from one member and store it, verifying the upstream bytes hash to
    /// the requested digest. The single-flight gate around this lives in [`Self::pull_manifest_by_digest`].
    async fn fetch_manifest_by_digest(
        &self,
        state: &ServingState,
        client: &UpstreamClient,
        index: &str,
        repo: &str,
        digest: &str,
        head: bool,
    ) -> Result<Option<Response>, ServeError> {
        let response = match self
            .upstream
            .manifest(
                client.base_url(),
                client.auth(),
                &self.upstream_repo(index, client, repo),
                digest,
            )
            .await
        {
            Ok(response) => response,
            Err(UpstreamError::Status(status)) if absent_upstream(status) => return Ok(None),
            Err(err) => return Ok(Some(upstream_manifest_error(&err))),
        };
        let (manifest, canonical) = store_manifest(state, index, repo, None, response).await?;
        // A pull by sha256 digest must hash to it, or the upstream bytes are corrupt. A digest in
        // another algorithm the spec permits content-addresses upstream and peryx cannot recompute it,
        // so the bytes are served under the requested digest rather than rejected as a mismatch.
        Ok(Some(if digest.starts_with("sha256:") && canonical != digest {
            error_response(
                ErrorCode::ManifestInvalid,
                &format!("upstream digest {canonical} does not match requested {digest}"),
            )
        } else {
            manifest_response(manifest, digest, head)
        }))
    }

    /// Try one member for a manifest by tag. A hosted member reads its cached tag; an online proxy
    /// serves the tag from cache while it is fresh and revalidates once the freshness window elapses.
    /// `None` means a miss, so the caller tries the next member.
    async fn member_tag(
        &self,
        state: &ServingState,
        member: &Index,
        repo: &str,
        tag: &str,
        head: bool,
    ) -> Result<Option<Response>, ServeError> {
        let Some(client) = member.proxy_client() else {
            return Ok(match store::get_tag(&state.meta, &member.name, repo, tag)? {
                Some(digest) => store::get_manifest(&state.meta, &digest)?
                    .map(|manifest| manifest_response(manifest, &digest, head)),
                None => None,
            });
        };
        if let Some(response) = fresh_tag(state, &member.name, repo, tag, head)? {
            return Ok(Some(response));
        }
        // Single-flight the revalidation: a burst of pulls of the same stale tag makes one upstream
        // request, and the followers re-read the tag the leader just refreshed.
        let gate_key = format!("oci\u{0}tag\u{0}{}\u{0}{repo}\u{0}{tag}", member.name);
        let gate = flight_gate(state, &gate_key);
        let _guard = gate.lock().await;
        if let Some(response) = fresh_tag(state, &member.name, repo, tag, head)? {
            return Ok(Some(response));
        }
        let fetched = self.revalidate_tag(state, client, &member.name, repo, tag, head).await;
        state.cache.forget_flight(&gate_key);
        fetched
    }

    /// Fetch a proxy tag from upstream and store it, returning the served manifest.
    async fn revalidate_tag(
        &self,
        state: &ServingState,
        client: &UpstreamClient,
        index: &str,
        repo: &str,
        tag: &str,
        head: bool,
    ) -> Result<Option<Response>, ServeError> {
        if let Some(response) = self.unchanged_tag(state, client, index, repo, tag, head).await? {
            return Ok(Some(response));
        }
        match self
            .upstream
            .manifest(
                client.base_url(),
                client.auth(),
                &self.upstream_repo(index, client, repo),
                tag,
            )
            .await
        {
            Ok(response) => {
                let (manifest, canonical) = store_manifest(state, index, repo, Some(tag), response).await?;
                Ok(Some(manifest_response(manifest, &canonical, head)))
            }
            // A `404` is upstream saying the tag is gone, which is an answer. Everything else is a
            // failure to get one, and a failure to confirm a tag is not a reason to forget it: an
            // expired token draws a `401` from Docker Hub, which must serve the cached image rather
            // than report it unknown — and, with nothing cached, report the auth failure itself.
            Err(UpstreamError::Status(StatusCode::NOT_FOUND)) => {
                if store::delete_tag(&state.meta, index, repo, tag)? {
                    state.bump_search_epoch();
                }
                Ok(None)
            }
            Err(UpstreamError::Status(status)) if absent_upstream(status) => stale_tag(state, index, repo, tag, head),
            Err(err) => Ok(Some(
                stale_tag(state, index, repo, tag, head)?.unwrap_or_else(|| upstream_manifest_error(&err)),
            )),
        }
    }

    /// Confirm a stale tag still points where it did, without fetching what it points at.
    ///
    /// A `HEAD` answers with the digest and no body, so the common revalidation — a tag that has not
    /// moved — costs one round trip rather than a manifest. Anything unexpected (no digest header, a
    /// moved tag, an upstream that will not answer a `HEAD`) returns `None`, and the caller fetches.
    async fn unchanged_tag(
        &self,
        state: &ServingState,
        client: &UpstreamClient,
        index: &str,
        repo: &str,
        tag: &str,
        head: bool,
    ) -> Result<Option<Response>, ServeError> {
        let Some((_, cached)) = store::tag_freshness(&state.meta, index, repo, tag)? else {
            return Ok(None);
        };
        let Ok(Some(upstream)) = self
            .upstream
            .manifest_digest(
                client.base_url(),
                client.auth(),
                &self.upstream_repo(index, client, repo),
                tag,
            )
            .await
        else {
            return Ok(None);
        };
        if upstream != cached {
            return Ok(None);
        }
        let Some(manifest) = store::get_manifest(&state.meta, &cached)? else {
            return Ok(None);
        };
        store::set_tag_freshness(&state.meta, index, repo, tag, &cached, (state.clock)())?;
        Ok(Some(manifest_response(manifest, &cached, head)))
    }
}

/// Whether any member the resolved index serves from records this digest as one it holds. A manifest
/// lives once in the global content store, shared across every index for dedup, so its presence there is
/// not authority to serve it by digest under an arbitrary repository name: the read authorizes only when
/// a member tagged it, pushed or pulled it, mirrored it, recorded it as a referrer, or serves an index
/// that names it as a child. A proxy member still pulls an unauthorized miss through — its upstream,
/// scoped to this repository, is the authority there — so this only gates answering from the cache.
fn manifest_authorized(state: &ServingState, members: &[&Index], repo: &str, digest: &str) -> Result<bool, ServeError> {
    for member in members {
        if store::manifest_is_member(&state.meta, &member.name, repo, digest)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Serve a proxy tag past its freshness window while the upstream cannot confirm it, bounded by
/// `max_stale_secs` exactly as a cached `PyPI` page is. `0` removes the bound.
///
/// Only reached once revalidation has already failed: a tag whose upstream answered is never stale.
fn stale_tag(
    state: &ServingState,
    index: &str,
    repo: &str,
    tag: &str,
    head: bool,
) -> Result<Option<Response>, ServeError> {
    let Some((fetched_at, digest)) = store::tag_freshness(&state.meta, index, repo, tag)? else {
        return Ok(None);
    };
    if !within_stale_bound(state, fetched_at) {
        return Ok(None);
    }
    Ok(store::get_manifest(&state.meta, &digest)?.map(|manifest| manifest_response(manifest, &digest, head)))
}

/// Read an upstream manifest response into storage, keyed by the sha256 of its exact bytes, updating
/// the tag mapping when the pull was by tag. Returns the stored manifest and its canonical digest.
pub async fn store_manifest(
    state: &ServingState,
    index: &str,
    repo: &str,
    tag: Option<&str>,
    response: reqwest::Response,
) -> Result<(Manifest, String), ServeError> {
    let media_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or(DEFAULT_MANIFEST_TYPE)
        .to_owned();
    let advertised = response
        .headers()
        .get(DOCKER_CONTENT_DIGEST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = bounded_body(response, MAX_MANIFEST_BYTES).await?;
    let canonical = format!("sha256:{}", Digest::of(&bytes).as_str());
    // A corrupting proxy or CDN between peryx and upstream could return altered bytes; a sha256 digest
    // upstream advertises must hash to it, or the manifest is not stored. The spec permits another
    // algorithm (e.g. sha512), which peryx cannot recompute but still content-addresses under its own
    // sha256, so a non-sha256 advertisement is not a mismatch to reject.
    if let Some(advertised) = advertised
        && advertised.starts_with("sha256:")
        && advertised != canonical
    {
        return Err(ServeError::Transport(format!(
            "upstream digest {advertised} does not match manifest content {canonical}"
        )));
    }
    let manifest = Manifest {
        media_type,
        bytes: bytes.to_vec(),
    };
    store::record_manifest(&state.meta, index, repo, &canonical, &manifest)?;
    if let Some(tag) = tag {
        if store::put_tag(&state.meta, index, repo, tag, &canonical)? {
            state.bump_search_epoch();
        }
        store::set_tag_freshness(&state.meta, index, repo, tag, &canonical, (state.clock)())?;
    }
    Ok((manifest, canonical))
}

/// The OCI image index media type.
const OCI_INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";
/// The Docker v2 manifest-list media type, the schema-2 equivalent of an OCI index.
const DOCKER_MANIFEST_LIST_TYPE: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

/// A media type stripped of its parameters (`;q=`, `;charset=`), so a comparison keys on the type
/// alone.
fn media_type_base(value: &str) -> &str {
    value.split(';').next().unwrap_or(value).trim()
}
/// Whether a media type names an image index or manifest list, the two list types a manifest read may
/// have to negotiate against the client's `Accept`.
fn is_list_media_type(media_type: &str) -> bool {
    let base = media_type_base(media_type);
    base == OCI_INDEX_TYPE || base == DOCKER_MANIFEST_LIST_TYPE
}
/// Whether the client's `Accept` names neither list type, the signal that it cannot parse an index and
/// wants the `linux/amd64` child instead — the same explicit-accept test `distribution` applies.
fn accept_excludes_list_types(accept: &str) -> bool {
    !accept.split(',').any(is_list_media_type)
}
/// Build the response for a stored manifest, headers-only for a `HEAD`. The content length is set the
/// same either way, so a `HEAD` reports the size a `GET` would return.
fn manifest_response(manifest: Manifest, digest: &str, head: bool) -> Response {
    let builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, &manifest.media_type)
        .header(DOCKER_CONTENT_DIGEST, digest)
        .header(header::CONTENT_LENGTH, manifest.bytes.len());
    let body = if head {
        Body::empty()
    } else {
        Body::from(manifest.bytes)
    };
    builder
        .body(body)
        .expect("manifest response builds from validated header parts")
}

/// Serve a proxy tag from cache while its recorded fetch is still within the freshness window, or
/// `None` to force a revalidation. A tag is mutable upstream, so it is trusted only for `ttl_secs`
/// after the last fetch; a manifest missing under a still-fresh record also forces a revalidation.
fn fresh_tag(
    state: &ServingState,
    index: &str,
    repo: &str,
    tag: &str,
    head: bool,
) -> Result<Option<Response>, ServeError> {
    let Some((fetched_at, digest)) = store::tag_freshness(&state.meta, index, repo, tag)? else {
        return Ok(None);
    };
    if (state.clock)().saturating_sub(fetched_at) >= state.ttl_secs {
        return Ok(None);
    }
    Ok(store::get_manifest(&state.meta, &digest)?.map(|manifest| manifest_response(manifest, &digest, head)))
}

/// A gateway fault for an upstream manifest failure. Callers treat an "absent" status as a member
/// miss before reaching here, so anything left is a real transport, server, or rate-limit error.
fn upstream_manifest_error(err: &UpstreamError) -> Response {
    upstream_error_response(err, "manifest")
}
