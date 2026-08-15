use super::*;
use crate::error::{ErrorCode, error_response};
use crate::store::{self};
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_driver::ServingState;
use peryx_upstream::UpstreamClient;

const TAG_RESOLUTION_CONCURRENCY: usize = 8;

impl<S: BuildHasher + Default + Send + Sync + 'static> OciRegistryWithHasher<S> {
    /// Serve the tag list. With no active revocations a lone online proxy passes upstream through;
    /// every other case filters and unions member tags before applying `n`/`last` pagination.
    pub(super) async fn serve_tags(
        &self,
        state: &ServingState,
        name: &str,
        query: &str,
    ) -> Result<Response, ServeError> {
        let Some((index, repo)) = resolve(&state.indexes, name) else {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        };
        if policy_blocks(index, PolicyAction::Serve, repo) {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        }
        // A tag list is a mutable derived view, so a replica hides a hosted index's until the search
        // view catches the serial that changed it.
        if holds_below_readable_frontier(state, index, hosted_last_serial(state, index)?) {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        }
        let active = state.revocations.has_active()?;
        let members = serving_members(state, index);
        if let [member] = members.as_slice()
            && let Some(client) = member.proxy_client()
        {
            let response = self.proxy_tags(state, name, &member.name, client, repo, query).await?;
            return if active {
                self.filter_proxy_tag_page(state, name, &member.name, client, repo, response)
                    .await
            } else {
                serve_proxy_tag_page(name, response).await
            };
        }
        let tags = self.visible_tag_names(state, name, repo, active, &members).await?;
        Ok(tag_list_response(name, &tags, query))
    }

    /// Collect tag names in member-shadowing order, with tombstones masking only their own or lower
    /// layers. The second tombstone pass closes a delete race while upstream pages are fetched.
    pub(super) async fn visible_tag_names(
        &self,
        state: &ServingState,
        name: &str,
        repo: &str,
        active: bool,
        members: &[&Index],
    ) -> Result<std::collections::BTreeSet<String>, ServeError> {
        if let [member] = members
            && member.proxy_client().is_none()
        {
            let mut tags = stored_tag_names(state, &member.name, repo, active)?
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>();
            for tag in store::list_trashed_tags(&state.meta, &member.name, repo)? {
                tags.remove(&tag);
            }
            return Ok(tags);
        }
        let mut tags = std::collections::BTreeMap::new();
        let mut hidden = std::collections::BTreeMap::new();
        for (position, member) in members.iter().enumerate() {
            let names = match member.proxy_client() {
                Some(client) => self
                    .fetch_tag_names(state, name, &member.name, client, repo, active)
                    .await?
                    .unwrap_or_default(),
                None => stored_tag_names(state, &member.name, repo, active)?,
            };
            for tag in names {
                tags.entry(tag).or_insert(position);
            }
            for tag in store::list_trashed_tags(&state.meta, &member.name, repo)? {
                hidden.entry(tag).or_insert(position);
            }
        }
        for (position, member) in members.iter().enumerate() {
            for tag in store::list_trashed_tags(&state.meta, &member.name, repo)? {
                hidden.entry(tag).or_insert(position);
            }
        }
        tags.retain(|tag, source| hidden.get(tag).is_none_or(|tombstone| tombstone > source));
        Ok(tags.into_keys().collect())
    }

    /// Serve a lone proxy's tag list, from the store while it is fresh.
    ///
    /// A tag list is mutable upstream, so it is trusted for `ttl_secs` and revalidated after. Passing
    /// every request through made a `tags/list` cost an upstream round trip rather than the registry,
    /// and made a burst of them cost the upstream once per client. When revalidation fails, the last
    /// tag list remains available until its stale-cache limit.
    async fn proxy_tags(
        &self,
        state: &ServingState,
        name: &str,
        index: &str,
        client: &UpstreamClient,
        repo: &str,
        query: &str,
    ) -> Result<Response, ServeError> {
        let now = (state.clock)();
        let cached = store::tag_page(&state.meta, index, repo, query)?;
        if let Some((fetched_at, link, body)) = &cached
            && now.saturating_sub(*fetched_at) < state.ttl_secs
        {
            return Ok(tag_page_response(name, link.as_deref(), body.clone()));
        }
        match self
            .upstream
            .tags(
                client.base_url(),
                client.auth(),
                &self.upstream_repo(index, client, repo),
                query,
            )
            .await
        {
            Ok(response) => {
                let link = response
                    .headers()
                    .get(reqwest::header::LINK)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let body = bounded_body(response, MAX_TAGS_BYTES).await?;
                store::set_tag_page(&state.meta, index, repo, query, now, link.as_deref(), &body)?;
                Ok(tag_page_response(name, link.as_deref(), body.to_vec()))
            }
            Err(err) => match cached {
                Some((fetched_at, link, body)) if within_stale_bound(state, fetched_at) => {
                    Ok(tag_page_response(name, link.as_deref(), body))
                }
                _ => Ok(upstream_error_response(&err, "tags")),
            },
        }
    }

    /// Fetch a proxy member's tag names for aggregation, or `None` on any upstream failure so one
    /// unreachable member does not fail the whole list.
    pub(super) async fn fetch_tag_names(
        &self,
        state: &ServingState,
        name: &str,
        index: &str,
        client: &UpstreamClient,
        repo: &str,
        active: bool,
    ) -> Result<Option<Vec<String>>, ServeError> {
        let mut names = Vec::new();
        let mut query = String::new();
        let mut page = 0;
        loop {
            // Each page is cached under its own query, so a virtual index that unions several proxies
            // no longer re-walks every upstream's pagination on every request.
            let response = self.proxy_tags(state, name, index, client, repo, &query).await?;
            let response = if active {
                self.filter_proxy_tag_page(state, name, index, client, repo, response)
                    .await?
            } else {
                response
            };
            let (parts, body) = response.into_parts();
            if !parts.status.is_success() {
                return Ok(None);
            }
            let next = parts.headers.get(header::LINK).and_then(next_page_query_of);
            let bytes = axum::body::to_bytes(body, MAX_TAGS_BYTES)
                .await
                .expect("proxy tag pages are bounded before caching");
            let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                return Ok(None);
            };
            let tags = parsed["tags"].as_array().into_iter().flatten();
            names.extend(tags.filter_map(|tag| tag.as_str().map(str::to_owned)));
            page += 1;
            match next {
                Some(next) if page < MAX_TAG_PAGES => query = next,
                _ => break,
            }
        }
        Ok(Some(names))
    }

    async fn filter_proxy_tag_page(
        &self,
        state: &ServingState,
        name: &str,
        index: &str,
        client: &UpstreamClient,
        repo: &str,
        response: Response,
    ) -> Result<Response, ServeError> {
        if !response.status().is_success() {
            return Ok(response);
        }
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, MAX_TAGS_BYTES)
            .await
            .expect("proxy tag pages are bounded before filtering");
        let document = serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|err| ServeError::Transport(format!("upstream tag list is invalid: {err}")))?;
        let tags = match &document["tags"] {
            serde_json::Value::Array(tags) => tags.iter().filter_map(|tag| tag.as_str().map(str::to_owned)).collect(),
            serde_json::Value::Null => Vec::new(),
            _ => return Err(ServeError::Transport("upstream tag list is invalid".to_owned())),
        };
        let tags = self.visible_proxy_tags(state, index, client, repo, tags).await?;
        Ok(tag_page_response(
            name,
            parts.headers.get(header::LINK).and_then(|value| value.to_str().ok()),
            serde_json::json!({ "name": name, "tags": tags })
                .to_string()
                .into_bytes(),
        ))
    }

    async fn visible_proxy_tags(
        &self,
        state: &ServingState,
        index: &str,
        client: &UpstreamClient,
        repo: &str,
        tags: Vec<String>,
    ) -> Result<std::collections::BTreeSet<String>, ServeError> {
        let checked = futures_util::stream::iter(tags.into_iter().map(|tag| async move {
            let digest = self.proxy_tag_digest(state, index, client, repo, &tag).await?;
            let visible = match digest {
                Some(digest) => digest_decision(state, &digest)? == DigestDecision::Clear,
                None => false,
            };
            Ok::<_, ServeError>((tag, visible))
        }))
        .buffer_unordered(TAG_RESOLUTION_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
        let mut visible = std::collections::BTreeSet::new();
        for checked in checked {
            let (tag, clear) = checked?;
            if clear {
                visible.insert(tag);
            }
        }
        Ok(visible)
    }

    async fn proxy_tag_digest(
        &self,
        state: &ServingState,
        index: &str,
        client: &UpstreamClient,
        repo: &str,
        tag: &str,
    ) -> Result<Option<String>, ServeError> {
        if let Some((fetched_at, digest)) = store::tag_freshness(&state.meta, index, repo, tag)?
            && (state.clock)().saturating_sub(fetched_at) < state.ttl_secs
        {
            return Ok(Some(digest));
        }
        let Ok(Some(digest)) = self
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
        if store::put_tag(&state.meta, index, repo, tag, &digest)? {
            state.bump_search_epoch();
        }
        store::set_tag_freshness(&state.meta, index, repo, tag, &digest, (state.clock)())?;
        Ok(Some(digest))
    }

    /// The referrer descriptors upstream records for `repo`/`digest`. A registry predating the referrers
    /// API answers `404`; the spec then directs a fallback to the referrers tag schema, an image index
    /// tagged after the subject digest, so a signature or SBOM pushed before the API existed stays
    /// discoverable through the cache. Any other failure yields empty rather than failing the response.
    async fn upstream_referrers(
        &self,
        index: &str,
        client: &UpstreamClient,
        repo: &str,
        digest: &str,
    ) -> Vec<serde_json::Value> {
        let repo = self.upstream_repo(index, client, repo);
        match self
            .upstream
            .referrers(client.base_url(), client.auth(), &repo, digest)
            .await
        {
            Ok(response) => referrer_manifests(response).await,
            Err(crate::upstream::UpstreamError::Status(StatusCode::NOT_FOUND)) => {
                let Ok(response) = self
                    .upstream
                    .manifest(
                        client.base_url(),
                        client.auth(),
                        &repo,
                        &crate::name::referrers_tag(digest),
                    )
                    .await
                else {
                    return Vec::new();
                };
                referrer_manifests(response).await
            }
            Err(_) => Vec::new(),
        }
    }

    /// Serve `GET /v2/<name>/referrers/<digest>`: the manifests that declare the digest their subject,
    /// unioning what each member stored with what an online proxy's upstream reports, so a signature or
    /// SBOM pushed upstream is discoverable through a cached image. `artifactType` filters the result
    /// and is echoed in `OCI-Filters-Applied`.
    pub(super) async fn serve_referrers(
        &self,
        state: &ServingState,
        name: &str,
        digest: &str,
        query: &str,
    ) -> Result<Response, ServeError> {
        let Some((index, repo)) = resolve(&state.indexes, name) else {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        };
        if policy_blocks(index, PolicyAction::Serve, repo) {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        }
        if !crate::name::valid_content_digest(digest) {
            return Ok(error_response(
                ErrorCode::DigestInvalid,
                "referrers digest is malformed",
            ));
        }
        let filter = query_params(query).remove("artifactType");
        // The referrers list is a mutable derived view, so a replica hides a hosted index's until the
        // search view catches the serial that changed it, reporting an empty set as the spec's response
        // to a subject with none.
        if holds_below_readable_frontier(state, index, hosted_last_serial(state, index)?) {
            return Ok(referrers_response(&[], filter.as_deref()));
        }
        let active = state.revocations.has_active()?;
        if active && digest_decision(state, digest)? == DigestDecision::Revoked {
            return Ok(referrers_response(&[], filter.as_deref()));
        }
        let members = serving_members(state, index);
        if manifest_trashed_in(state, &members, repo, digest)? {
            return Ok(referrers_response(&[], filter.as_deref()));
        }
        let mut seen = std::collections::HashSet::new();
        let mut manifests = Vec::new();
        for member in &members {
            for descriptor in store::list_referrers(&state.meta, &member.name, repo, digest)? {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&descriptor) {
                    add_referrer(state, &members, repo, active, value, &mut seen, &mut manifests)?;
                }
            }
            if let Some(client) = member.proxy_client() {
                for descriptor in self.upstream_referrers(&member.name, client, repo, digest).await {
                    add_referrer(state, &members, repo, active, descriptor, &mut seen, &mut manifests)?;
                }
            }
        }
        if let Some(artifact_type) = &filter {
            manifests.retain(|descriptor| descriptor["artifactType"].as_str() == Some(artifact_type));
        }
        if manifest_trashed_in(state, &members, repo, digest)? {
            manifests.clear();
        }
        Ok(referrers_response(&manifests, filter.as_deref()))
    }
}

pub(super) fn stored_tag_names(
    state: &ServingState,
    index: &str,
    repo: &str,
    active: bool,
) -> Result<Vec<String>, ServeError> {
    if !active {
        return Ok(store::list_tags(&state.meta, index, repo)?);
    }
    let mut names = Vec::new();
    for (tag, digest) in store::list_tag_targets(&state.meta, index, repo)? {
        if digest_decision(state, &digest)? == DigestDecision::Clear {
            names.push(tag);
        }
    }
    Ok(names)
}

async fn referrer_manifests(response: reqwest::Response) -> Vec<serde_json::Value> {
    bounded_body(response, MAX_MANIFEST_BYTES)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|document| document["manifests"].as_array().cloned())
        .unwrap_or_default()
}

fn add_referrer(
    state: &ServingState,
    members: &[&Index],
    repo: &str,
    active: bool,
    descriptor: serde_json::Value,
    seen: &mut std::collections::HashSet<String>,
    manifests: &mut Vec<serde_json::Value>,
) -> Result<(), ServeError> {
    let Some(digest) = descriptor["digest"].as_str() else {
        return Ok(());
    };
    if (!active || digest_decision(state, digest)? == DigestDecision::Clear)
        && !manifest_trashed_in(state, members, repo, digest)?
        && seen.insert(digest.to_owned())
    {
        manifests.push(descriptor);
    }
    Ok(())
}

fn referrers_response(manifests: &[serde_json::Value], filter: Option<&str>) -> Response {
    let document = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": manifests,
    });
    let mut response = (
        [(header::CONTENT_TYPE, "application/vnd.oci.image.index.v1+json")],
        document.to_string(),
    )
        .into_response();
    if filter.is_some() {
        response
            .headers_mut()
            .insert("oci-filters-applied", HeaderValue::from_static("artifactType"));
    }
    response
}

/// Apply distribution-spec `n`/`last` pagination to a sorted set: the page after `last`, truncated to
/// `n`, and the `(n, last-of-page)` cursor for a `Link` when more remains.
fn paginate(items: &std::collections::BTreeSet<String>, query: &str) -> (Vec<String>, Option<(usize, String)>) {
    let params = query_params(query);
    let last = params.get("last").map_or("", String::as_str);
    let limit = params.get("n").and_then(|value| value.parse::<usize>().ok());
    // The spec requires `n=0` to return an empty list with no `Link`; without this special case
    // truncate(0) empties the page while `page.len() > 0` still asks for a next cursor, so the marker
    // falls back to `""` and the self-referencing `Link` loops a following client forever.
    if limit == Some(0) {
        return (Vec::new(), None);
    }
    // The set is sorted, so `range` seeks past `last` and yields the tail lazily; only `n`/`n+1`
    // members are ever visited, keeping peak memory proportional to the page rather than the set.
    let mut rest = items.range::<str, _>((std::ops::Bound::Excluded(last), std::ops::Bound::Unbounded));
    let Some(n) = limit else {
        return (rest.cloned().collect(), None);
    };
    let page: Vec<String> = rest.by_ref().take(n).cloned().collect();
    let next = rest.next().and_then(|_| page.last()).map(|marker| (n, marker.clone()));
    (page, next)
}

fn tag_list_response(name: &str, tags: &std::collections::BTreeSet<String>, query: &str) -> Response {
    let (page, next) = paginate(tags, query);
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some((n, marker)) = next {
        builder = builder.header(
            header::LINK,
            format!("</v2/{name}/tags/list?n={n}&last={marker}>; rel=\"next\""),
        );
    }
    builder
        .body(Body::from(
            serde_json::json!({ "name": name, "tags": page }).to_string(),
        ))
        .expect("tag list response builds from validated parts")
}

pub(super) fn serve_catalog(state: &ServingState, query: &str) -> Result<Response, ServeError> {
    let mut repositories = std::collections::BTreeSet::new();
    for index in &state.indexes {
        if index.ecosystem != crate::ECOSYSTEM {
            continue;
        }
        for repo in store::list_repositories(&state.meta, &index.name)? {
            if policy_blocks(index, PolicyAction::Serve, &repo) {
                continue;
            }
            repositories.insert(if index.route.is_empty() {
                repo
            } else {
                format!("{}/{repo}", index.route)
            });
        }
    }
    let (page, next) = paginate(&repositories, query);
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some((n, marker)) = next {
        builder = builder.header(
            header::LINK,
            format!("</v2/_catalog?n={n}&last={marker}>; rel=\"next\""),
        );
    }
    Ok(builder
        .body(Body::from(serde_json::json!({ "repositories": page }).to_string()))
        .expect("catalog response builds from validated parts"))
}

/// A tag-list page as this registry answers it: the upstream body, and a `Link` to the next page
/// rewritten to this registry's client-facing name. The upstream's `Link` names the upstream
/// repository (`/v2/library/nginx/...`, no index route), which a client would resolve back against
/// peryx and 404; only its query carries over. The body's `name` is the upstream repository too and is
/// rewritten by [`serve_proxy_tag_page`] on the client-facing path; the aggregation path reads only the
/// `tags` and ignores it.
fn tag_page_response(name: &str, upstream_link: Option<&str>, body: Vec<u8>) -> Response {
    let mut response = ([(header::CONTENT_TYPE, "application/json")], body).into_response();
    if let Some(query) = upstream_link.and_then(next_page_query)
        && let Ok(value) = HeaderValue::from_str(&format!("</v2/{name}/tags/list?{query}>; rel=\"next\""))
    {
        response.headers_mut().insert(header::LINK, value);
    }
    response
}

/// Rewrite a served proxy tag page's body `name` to the client-facing repository. `proxy_tags` caches
/// and forwards the upstream body verbatim, whose `name` is the upstream repository (`library/nginx`) a
/// client cannot address and which the cached, filtered, and virtual paths never emit; the single
/// online-proxy serve path swaps it here. Tag order and count carry over, the already-rewritten `Link`
/// stays, and an upstream error passes through untouched. A success body that is not a tag list is a
/// gateway fault, not a listing.
async fn serve_proxy_tag_page(name: &str, response: Response) -> Result<Response, ServeError> {
    if !response.status().is_success() {
        return Ok(response);
    }
    let (mut parts, body) = response.into_parts();
    let body = axum::body::to_bytes(body, MAX_TAGS_BYTES)
        .await
        .expect("proxy tag pages are bounded before serving");
    let body = rewrite_tag_page_name(name, &body)?;
    parts.headers.remove(header::CONTENT_LENGTH);
    Ok(Response::from_parts(parts, Body::from(body)))
}

/// Rewrite a proxied tag-list body's `name` to the client-facing repository, preserving tag order and
/// count. Upstream answers under its own repository name, so forwarding it unchanged leaks a name the
/// client cannot address. A body that is not a tag-list object with a string-array (or absent) `tags`
/// is rejected rather than served as one.
fn rewrite_tag_page_name(name: &str, body: &[u8]) -> Result<Vec<u8>, ServeError> {
    let invalid = || ServeError::Transport("upstream tag list is invalid".to_owned());
    let document = serde_json::from_slice::<serde_json::Value>(body)
        .map_err(|err| ServeError::Transport(format!("upstream tag list is invalid: {err}")))?;
    let serde_json::Value::Object(fields) = &document else {
        return Err(invalid());
    };
    let tags = match fields.get("tags") {
        Some(serde_json::Value::Array(tags)) if tags.iter().all(serde_json::Value::is_string) => tags.clone(),
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(_) => return Err(invalid()),
    };
    Ok(serde_json::json!({ "name": name, "tags": tags })
        .to_string()
        .into_bytes())
}

fn next_page_query_of(value: &HeaderValue) -> Option<String> {
    next_page_query(value.to_str().ok()?)
}

/// The query string of the `rel="next"` link in an RFC 8288 `Link` header. A header may carry several
/// comma-separated link-values (`rel="prev"`, `rel="next"`, …); the `next` one drives pagination, so
/// picking the first `<...>` blindly can walk backwards.
fn next_page_query(link: &str) -> Option<String> {
    let target = link_values(link)
        .into_iter()
        .find(|value| value.contains("rel=\"next\""))?;
    let start = target.find('<')? + 1;
    let end = target[start..].find('>')? + start;
    target[start..end].split_once('?').map(|(_, query)| query.to_owned())
}

/// Split an RFC 8288 `Link` header into its link-values. A comma separates link-values, but is also a
/// legal unencoded query sub-delimiter (RFC 3986) inside the angle-bracketed target, so a comma within
/// `<…>` belongs to that target rather than ending it. Splitting on every comma would break a cursor
/// that carries one, drop the `next` link-value, and silently truncate the listing.
fn link_values(link: &str) -> Vec<&str> {
    let mut values = Vec::new();
    let mut start = 0;
    let mut in_target = false;
    for (index, byte) in link.bytes().enumerate() {
        match byte {
            b'<' => in_target = true,
            b'>' => in_target = false,
            b',' if !in_target => {
                values.push(&link[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    values.push(&link[start..]);
    values
}
