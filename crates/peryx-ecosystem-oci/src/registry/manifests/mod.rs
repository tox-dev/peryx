mod write;
pub(super) use write::{delete_manifest, put_manifest, restore_manifest};

use super::*;
use crate::error::{ErrorCode, error_response};
use crate::name::Reference;
use crate::store::{self, Manifest};
use crate::upstream::UpstreamError;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use peryx_driver::ServingState;
use peryx_events::metrics::Observation;
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
        let members = policy_serving_members(state, index, repo);
        let response = match reference {
            Reference::Digest(digest) => self.manifest_by_digest(state, &members, repo, digest, head).await?,
            Reference::Tag(tag) => {
                // A tag is a mutable name→digest resolution, so on a replica it stays hidden until the
                // search view catches the serial that published it; a by-digest read above is
                // content-addressed and never held.
                if holds_below_readable_frontier(state, index, hosted_last_serial(state, index)?) {
                    return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
                }
                let mut served = None;
                let mut checked = members.len();
                for (position, member) in members.iter().enumerate() {
                    if store::tag_is_trashed(&state.meta, &member.name, repo, tag)? {
                        return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
                    }
                    served = self.member_tag(state, member, repo, tag, head).await?;
                    if served.is_some() {
                        checked = position + 1;
                        break;
                    }
                }
                for member in members.iter().take(checked) {
                    if store::tag_is_trashed(&state.meta, &member.name, repo, tag)? {
                        return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
                    }
                }
                if let Some(digest) = served.as_ref().and_then(|response| {
                    response
                        .headers()
                        .get(DOCKER_CONTENT_DIGEST)
                        .and_then(|value| value.to_str().ok())
                }) && manifest_trashed_in(state, &members[..checked], repo, digest)?
                {
                    return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
                }
                served.unwrap_or_else(|| error_response(ErrorCode::ManifestUnknown, "manifest unknown"))
            }
        };
        let mut response = self
            .negotiate_manifest(state, &members, repo, accept, response, head)
            .await?;
        if response.status() == StatusCode::OK {
            // The same tag can hand back the index or its child depending on Accept, so a shared cache
            // keyed on the URL alone would mis-serve one client the other's body.
            response
                .headers_mut()
                .insert(header::VARY, HeaderValue::from_static("accept"));
            // Manifest bodies count as page traffic; metadata-only HEAD responses do not.
            if !head {
                state.metrics.record(Observation::Page {
                    repository: index.route.clone(),
                    resource: repo.to_owned(),
                });
            }
        }
        Ok(response)
    }

    /// Rewrite a Docker manifest list to its `linux/amd64` child for legacy Docker (< 17.06), which
    /// sends only the schema-2 image type. OCI indexes and unusable Docker fallbacks are rejected
    /// because the client did not advertise any representation the registry can serve.
    async fn negotiate_manifest(
        &self,
        state: &ServingState,
        members: &[&Index],
        repo: &str,
        accept: Option<&str>,
        response: Response,
        head: bool,
    ) -> Result<Response, ServeError> {
        let Some(content_type) = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .filter(|value| is_list_media_type(value))
            .map(str::to_owned)
        else {
            return Ok(response);
        };
        if response.status() != StatusCode::OK
            || accept.is_none_or(|accept| media_type_acceptable(accept, &content_type))
        {
            return Ok(response);
        }
        if media_type_base(&content_type) == OCI_INDEX_TYPE {
            return Ok(error_response(
                ErrorCode::ManifestUnknown,
                "OCI index found, but accept header does not support OCI indexes",
            ));
        }
        let accept = accept.expect("an unacceptable list has an Accept header");
        let digest = response
            .headers()
            .get(DOCKER_CONTENT_DIGEST)
            .and_then(|value| value.to_str().ok())
            .expect("a served manifest carries its content digest")
            .to_owned();
        let list = store::get_manifest(&state.meta, &digest)?.expect("a served manifest is stored under its digest");
        let Some(child) = store::linux_amd64_child(&list.bytes) else {
            return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
        };
        let served = self.manifest_by_digest(state, members, repo, &child, head).await?;
        Ok(acceptable_manifest_response(served, accept))
    }

    /// Resolve one manifest by digest across the serving members, hosted-first.
    ///
    /// The manifest store is content-addressed across every repository, so holding the bytes is not
    /// permission to serve them: a member answers only for a digest recorded under `repo`, and a member
    /// that trashed the digest shadows the members behind it. A proxy member without membership fetches
    /// the digest from its upstream, which records it for `repo`. The trash check repeats after a serve
    /// because a delete can land while a proxy member is on the wire.
    async fn manifest_by_digest(
        &self,
        state: &ServingState,
        members: &[&Index],
        repo: &str,
        digest: &str,
        head: bool,
    ) -> Result<Response, ServeError> {
        if digest_decision(state, digest)? == DigestDecision::Revoked {
            return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
        }
        let mut served = None;
        let mut checked = members.len();
        for (position, member) in members.iter().enumerate() {
            if store::manifest_is_trashed(&state.meta, &member.name, repo, digest)? {
                return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
            }
            if store::manifest_is_member(&state.meta, &member.name, repo, digest)? {
                served =
                    store::get_manifest(&state.meta, digest)?.map(|manifest| manifest_response(manifest, digest, head));
            }
            if served.is_none()
                && let Some(client) = member.proxy_client()
            {
                served = self
                    .pull_manifest_by_digest(state, client, &member.name, repo, digest, head)
                    .await?;
            }
            if served.is_some() {
                checked = position + 1;
                break;
            }
        }
        for member in members.iter().take(checked) {
            if store::manifest_is_trashed(&state.meta, &member.name, repo, digest)? {
                return Ok(error_response(ErrorCode::ManifestUnknown, "manifest unknown"));
            }
        }
        Ok(served.unwrap_or_else(|| error_response(ErrorCode::ManifestUnknown, "manifest unknown")))
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
        // A by-digest read authorizes against the requesting repository, not the content-addressed
        // store every index shares for dedup. The gate keys on the digest alone, so a concurrent pull
        // for another repository may have populated the byte record while this one waited: re-check
        // membership here, or a cache hit would serve one repository's private bytes under another.
        if store::manifest_is_member(&state.meta, index, repo, digest)?
            && let Some(manifest) = store::get_manifest(&state.meta, digest)?
        {
            return Ok(Some(manifest_response(manifest, digest, head)));
        }
        let fetched = self
            .fetch_manifest_by_digest(state, client, index, repo, digest, head)
            .await;
        state.cache.forget_flight(&gate_key);
        fetched
    }

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
            .manifest(client, &self.upstream_repo(index, client, repo), digest)
            .await
        {
            Ok(response) => response,
            Err(UpstreamError::Status(status)) if absent_upstream(status) => return Ok(None),
            Err(err) => return Ok(Some(upstream_manifest_error(&err))),
        };
        Ok(Some(
            match store_manifest(state, index, repo, None, Some(digest), response).await? {
                StoredManifest::Stored(manifest, _) => manifest_response(manifest, digest, head),
                StoredManifest::Revoked => error_response(ErrorCode::ManifestUnknown, "manifest unknown"),
                StoredManifest::Mismatch(canonical) => error_response(
                    ErrorCode::ManifestInvalid,
                    &format!("upstream digest {canonical} does not match requested {digest}"),
                ),
            },
        ))
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
                Some(digest) if digest_decision(state, &digest)? == DigestDecision::Revoked => {
                    Some(error_response(ErrorCode::ManifestUnknown, "manifest unknown"))
                }
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

    async fn revalidate_tag(
        &self,
        state: &ServingState,
        client: &UpstreamClient,
        index: &str,
        repo: &str,
        tag: &str,
        head: bool,
    ) -> Result<Option<Response>, ServeError> {
        let upstream = self
            .upstream
            .manifest_digest(client, &self.upstream_repo(index, client, repo), tag)
            .await
            .ok()
            .flatten();
        if let Some(digest) = upstream.as_deref()
            && digest_decision(state, digest)? == DigestDecision::Revoked
        {
            return Ok(Some(error_response(ErrorCode::ManifestUnknown, "manifest unknown")));
        }
        if let Some(response) = unchanged_tag(state, index, repo, tag, upstream.as_deref(), head)? {
            return Ok(Some(response));
        }
        match self
            .upstream
            .manifest(client, &self.upstream_repo(index, client, repo), tag)
            .await
        {
            Ok(response) => Ok(Some(
                match store_manifest(state, index, repo, Some(tag), None, response).await? {
                    StoredManifest::Stored(manifest, canonical) => manifest_response(manifest, &canonical, head),
                    StoredManifest::Revoked | StoredManifest::Mismatch(_) => {
                        error_response(ErrorCode::ManifestUnknown, "manifest unknown")
                    }
                },
            )),
            // Only 404 proves a tag is absent. Preserve cached tags for authentication and transport failures.
            Err(UpstreamError::Status(StatusCode::NOT_FOUND)) => {
                if store::delete_tag(&state.meta, index, repo, tag)? {
                    state.invalidate_search_resource(repo);
                }
                Ok(None)
            }
            Err(UpstreamError::Status(status)) if absent_upstream(status) => stale_tag(state, index, repo, tag, head),
            Err(err) => Ok(Some(
                stale_tag(state, index, repo, tag, head)?.unwrap_or_else(|| upstream_manifest_error(&err)),
            )),
        }
    }
}

fn acceptable_manifest_response(response: Response, accept: &str) -> Response {
    if response.status() != StatusCode::OK {
        return response;
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .expect("a served manifest carries its content type");
    if media_type_acceptable(accept, content_type) {
        response
    } else {
        error_response(ErrorCode::ManifestInvalid, "schema-2 manifest not supported by client")
    }
}

fn unchanged_tag(
    state: &ServingState,
    index: &str,
    repo: &str,
    tag: &str,
    upstream: Option<&str>,
    head: bool,
) -> Result<Option<Response>, ServeError> {
    let Some((_, cached)) = store::tag_freshness(&state.meta, index, repo, tag)? else {
        return Ok(None);
    };
    if upstream != Some(&cached) {
        return Ok(None);
    }
    let Some(manifest) = store::get_manifest(&state.meta, &cached)? else {
        return Ok(None);
    };
    store::set_tag_freshness(&state.meta, index, repo, tag, &cached, (state.clock)())?;
    Ok(Some(manifest_response(manifest, &cached, head)))
}

/// Serve a proxy tag past its freshness window while the upstream cannot confirm it. `max_stale_secs`
/// bounds the stale interval; `0` removes the bound.
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
    if digest_decision(state, &digest)? == DigestDecision::Revoked {
        return Ok(Some(error_response(ErrorCode::ManifestUnknown, "manifest unknown")));
    }
    if !within_stale_bound(state, fetched_at) {
        return Ok(None);
    }
    Ok(store::get_manifest(&state.meta, &digest)?.map(|manifest| manifest_response(manifest, &digest, head)))
}

/// The outcome of reading an upstream manifest response, before or without committing it.
enum StoredManifest {
    /// The bytes were recorded under `canonical` (carried alongside the manifest).
    Stored(Manifest, String),
    /// The canonical digest is revoked, so nothing was recorded.
    Revoked,
    /// A by-digest pull's bytes hashed to `canonical`, not the requested digest, so nothing was
    /// recorded.
    Mismatch(String),
}

/// Read an upstream manifest response into storage, keyed by the sha256 of its exact bytes, updating
/// the tag mapping when the pull was by tag. `expected` is the requested sha256 digest on a by-digest
/// pull: the bytes must hash to it, or nothing is recorded and [`StoredManifest::Mismatch`] is
/// returned, so a faulty or hostile upstream cannot poison the cache under a digest peryx rejects.
async fn store_manifest(
    state: &ServingState,
    index: &str,
    repo: &str,
    tag: Option<&str>,
    expected: Option<&str>,
    response: reqwest::Response,
) -> Result<StoredManifest, ServeError> {
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
    // Verify an advertised SHA-256 digest; retain other algorithms as upstream content addresses.
    if let Some(advertised) = advertised
        && advertised.starts_with("sha256:")
        && advertised != canonical
    {
        return Err(ServeError::Transport(format!(
            "upstream digest {advertised} does not match manifest content {canonical}"
        )));
    }
    // Verify a requested SHA-256 digest before publishing repository membership.
    if let Some(expected) = expected
        && expected.starts_with("sha256:")
        && canonical != expected
    {
        return Ok(StoredManifest::Mismatch(canonical));
    }
    if digest_decision(state, &canonical)? == DigestDecision::Revoked {
        return Ok(StoredManifest::Revoked);
    }
    let manifest = Manifest {
        media_type,
        bytes: bytes.to_vec(),
    };
    store::record_manifest(&state.meta, index, repo, &canonical, &manifest)?;
    store::record_content_placement(&state.meta, &canonical, store::OciArtifactOrigin::Mirrored, true)?;
    let search_invalidation = crate::search_oci::SearchInvalidationGuard::arm(state, repo);
    if let Some(tag) = tag {
        store::put_tag(&state.meta, index, repo, tag, &canonical)?;
        store::set_tag_freshness(&state.meta, index, repo, tag, &canonical, (state.clock)())?;
    }
    drop(search_invalidation);
    Ok(StoredManifest::Stored(manifest, canonical))
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
fn is_list_media_type(media_type: &str) -> bool {
    let base = media_type_base(media_type);
    base == OCI_INDEX_TYPE || base == DOCKER_MANIFEST_LIST_TYPE
}
/// One media range from an `Accept` list: a type/subtype pair (either side possibly the `*` wildcard)
/// and its quality weight.
struct MediaRange<'a> {
    kind: &'a str,
    sub: &'a str,
    quality: f32,
}

/// Parse one `Accept` list entry into a media range, or `None` when it is malformed: no `type/subtype`
/// shape, a concrete subtype under a wildcard type (`*/json`), or a `q` weight outside `0..=1`. A bare
/// `*` is read as `*/*`, the shorthand legacy clients send. Parameters other than `q` are ignored, so
/// specificity keys on the type pair alone.
fn parse_media_range(entry: &str) -> Option<MediaRange<'_>> {
    let mut parts = entry.split(';');
    let (kind, sub) = match parts.next()?.trim() {
        "" => return None,
        "*" => ("*", "*"),
        base => base.split_once('/')?,
    };
    let (kind, sub) = (kind.trim(), sub.trim());
    if kind.is_empty() || sub.is_empty() || (kind == "*" && sub != "*") {
        return None;
    }
    let mut quality = 1.0_f32;
    for param in parts {
        if let Some((name, value)) = param.split_once('=')
            && name.trim().eq_ignore_ascii_case("q")
        {
            quality = value
                .trim()
                .parse()
                .ok()
                .filter(|weight| (0.0..=1.0).contains(weight))?;
        }
    }
    Some(MediaRange { kind, sub, quality })
}

/// Whether `media_type` is acceptable under the combined `Accept` list per RFC 9110: its effective
/// quality - the weight of the most specific matching range, exact over `type/*` over `*/*` - is
/// positive. A `q=0` on the most specific match rejects the type even when a broader range would
/// accept it. An `Accept` with no parseable range expresses no preference and accepts anything.
fn media_type_acceptable(accept: &str, media_type: &str) -> bool {
    let (kind, sub) = media_type_base(media_type)
        .split_once('/')
        .expect("a list media type carries a type and a subtype");
    let mut ranges = accept.split(',').filter_map(parse_media_range).peekable();
    if ranges.peek().is_none() {
        return true;
    }
    let mut best: Option<(u8, f32)> = None;
    for range in ranges {
        let specificity = if range.kind.eq_ignore_ascii_case(kind) && range.sub.eq_ignore_ascii_case(sub) {
            3
        } else if range.kind.eq_ignore_ascii_case(kind) && range.sub == "*" {
            2
        } else if range.kind == "*" && range.sub == "*" {
            1
        } else {
            continue;
        };
        best = Some(match best {
            Some((seen, quality)) if seen > specificity => (seen, quality),
            Some((seen, quality)) if seen == specificity => (seen, quality.max(range.quality)),
            _ => (specificity, range.quality),
        });
    }
    best.is_some_and(|(_, quality)| quality > 0.0)
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
    if digest_decision(state, &digest)? == DigestDecision::Revoked {
        return Ok(Some(error_response(ErrorCode::ManifestUnknown, "manifest unknown")));
    }
    Ok(store::get_manifest(&state.meta, &digest)?.map(|manifest| manifest_response(manifest, &digest, head)))
}

/// A gateway fault for an upstream manifest failure. Callers treat an "absent" status as a member
/// miss before reaching here, so anything left is a real transport, server, or rate-limit error.
fn upstream_manifest_error(err: &UpstreamError) -> Response {
    upstream_error_response(err, "manifest")
}
