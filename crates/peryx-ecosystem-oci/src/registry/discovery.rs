//! Tag listing, `_catalog`, and the referrers API, aggregated across virtual members.

use super::*;
use crate::error::{ErrorCode, error_response};
use crate::store::{self};
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_core::Ecosystem;
use peryx_driver::ServingState;
use peryx_upstream::UpstreamClient;

impl<S: BuildHasher + Default + Send + Sync + 'static> OciRegistryWithHasher<S> {
    /// Serve the tag list. A lone online proxy passes upstream through verbatim; every other case
    /// (a hosted index, or a virtual index) unions its members' tags under the requested name, then
    /// applies the `n`/`last` pagination the spec defines.
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
        let members = serving_members(state, index);
        if let [member] = members.as_slice()
            && let Some(client) = member.proxy_client()
        {
            return self.proxy_tags(state, name, &member.name, client, repo, query).await;
        }
        let mut tags = std::collections::BTreeSet::new();
        for member in &members {
            match member.proxy_client() {
                Some(client) => {
                    if let Some(names) = self.fetch_tag_names(state, name, &member.name, client, repo).await {
                        tags.extend(names);
                    }
                }
                None => tags.extend(store::list_tags(&state.meta, &member.name, repo)?),
            }
        }
        Ok(tag_list_response(name, tags, query))
    }

    /// Serve a lone proxy's tag list, from the store while it is fresh.
    ///
    /// A tag list is mutable upstream, so it is trusted for `ttl_secs` and revalidated after. Passing
    /// every request through made a `tags/list` cost an upstream round trip rather than the registry,
    /// and made a burst of them cost the upstream once per client. When revalidation fails the last
    /// list still answers, bounded exactly as a stale tag or a stale `PyPI` page is.
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
    ) -> Option<Vec<String>> {
        let mut names = Vec::new();
        let mut query = String::new();
        let mut page = 0;
        loop {
            // Each page is cached under its own query, so a virtual index that unions several proxies
            // no longer re-walks every upstream's pagination on every request.
            let response = self.proxy_tags(state, name, index, client, repo, &query).await.ok()?;
            let (parts, body) = response.into_parts();
            if !parts.status.is_success() {
                return None;
            }
            let next = parts.headers.get(header::LINK).and_then(next_page_query_of);
            let bytes = axum::body::to_bytes(body, MAX_TAGS_BYTES).await.ok()?;
            let parsed: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            let tags = parsed["tags"].as_array().into_iter().flatten();
            names.extend(tags.filter_map(|tag| tag.as_str().map(str::to_owned)));
            page += 1;
            match next {
                Some(next) if page < MAX_TAG_PAGES => query = next,
                _ => break,
            }
        }
        Some(names)
    }

    /// The referrer descriptors upstream records for `repo`/`digest`, or empty on any failure (a
    /// registry predating the referrers API answers `404`, which must not fail the whole response).
    async fn upstream_referrers(
        &self,
        index: &str,
        client: &UpstreamClient,
        repo: &str,
        digest: &str,
    ) -> Vec<serde_json::Value> {
        let Ok(response) = self
            .upstream
            .referrers(
                client.base_url(),
                client.auth(),
                &self.upstream_repo(index, client, repo),
                digest,
            )
            .await
        else {
            return Vec::new();
        };
        bounded_body(response, MAX_MANIFEST_BYTES)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|document| document["manifests"].as_array().cloned())
            .unwrap_or_default()
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
        let mut seen = std::collections::HashSet::new();
        let mut manifests = Vec::new();
        let mut add = |descriptor: serde_json::Value| {
            if descriptor["digest"]
                .as_str()
                .is_some_and(|digest| seen.insert(digest.to_owned()))
            {
                manifests.push(descriptor);
            }
        };
        for member in serving_members(state, index) {
            for descriptor in store::list_referrers(&state.meta, &member.name, repo, digest)? {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&descriptor) {
                    add(value);
                }
            }
            if let Some(client) = member.proxy_client() {
                for descriptor in self.upstream_referrers(&member.name, client, repo, digest).await {
                    add(descriptor);
                }
            }
        }
        let filter = query_params(query).remove("artifactType");
        if let Some(artifact_type) = &filter {
            manifests.retain(|descriptor| descriptor["artifactType"].as_str() == Some(artifact_type));
        }
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
        Ok(response)
    }
}

/// Apply distribution-spec `n`/`last` pagination to a sorted set: the page after `last`, truncated to
/// `n`, and the `(n, last-of-page)` cursor for a `Link` when more remains.
fn paginate(items: std::collections::BTreeSet<String>, query: &str) -> (Vec<String>, Option<(usize, String)>) {
    let params = query_params(query);
    let last = params.get("last").map_or("", String::as_str);
    let limit = params.get("n").and_then(|value| value.parse::<usize>().ok());
    // The spec requires `n=0` to return an empty list with no `Link`; without this special case
    // truncate(0) empties the page while `page.len() > 0` still asks for a next cursor, so the marker
    // falls back to `""` and the self-referencing `Link` loops a following client forever.
    if limit == Some(0) {
        return (Vec::new(), None);
    }
    let mut page: Vec<String> = items.into_iter().filter(|item| item.as_str() > last).collect();
    let next = limit.filter(|&n| page.len() > n).map(|n| {
        page.truncate(n);
        (n, page.last().cloned().unwrap_or_default())
    });
    (page, next)
}

/// Build a `tags/list` response over a sorted tag set, applying `n`/`last` pagination and a `Link`
/// header to the next page when the set is truncated.
fn tag_list_response(name: &str, tags: std::collections::BTreeSet<String>, query: &str) -> Response {
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

/// Serve `GET /v2/_catalog`: the union of every OCI index's repositories as clients address them (the
/// index route prefixes the upstream repository), with `n`/`last` pagination.
pub(super) fn serve_catalog(state: &ServingState, query: &str) -> Result<Response, ServeError> {
    let mut repositories = std::collections::BTreeSet::new();
    for index in &state.indexes {
        if index.ecosystem != Ecosystem::Oci {
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
    let (page, next) = paginate(repositories, query);
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
/// peryx and 404; only its query carries over.
fn tag_page_response(name: &str, upstream_link: Option<&str>, body: Vec<u8>) -> Response {
    let mut response = ([(header::CONTENT_TYPE, "application/json")], body).into_response();
    if let Some(query) = upstream_link.and_then(next_page_query)
        && let Ok(value) = HeaderValue::from_str(&format!("</v2/{name}/tags/list?{query}>; rel=\"next\""))
    {
        response.headers_mut().insert(header::LINK, value);
    }
    response
}

/// `next_page_query`, reading an already-parsed header value.
fn next_page_query_of(value: &HeaderValue) -> Option<String> {
    next_page_query(value.to_str().ok()?)
}

/// The query string of the `rel="next"` link in an RFC 8288 `Link` header. A header may carry several
/// comma-separated link-values (`rel="prev"`, `rel="next"`, …); the `next` one drives pagination, so
/// picking the first `<...>` blindly can walk backwards.
fn next_page_query(link: &str) -> Option<String> {
    let target = link.split(',').find(|value| value.contains("rel=\"next\""))?;
    let start = target.find('<')? + 1;
    let end = target[start..].find('>')? + start;
    target[start..end].split_once('?').map(|(_, query)| query.to_owned())
}
