//! Tag listing, `_catalog`, and the referrers API, aggregated across virtual members.

use super::*;
use crate::error::{ErrorCode, error_response};
use crate::store::{self};
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use velodex_format::Ecosystem;
use velodex_http::AppState;
use velodex_upstream::UpstreamClient;

impl OciRegistry {
    /// Serve the tag list. A lone online proxy passes upstream through verbatim; every other case
    /// (a hosted index, or a virtual index) unions its members' tags under the requested name, then
    /// applies the `n`/`last` pagination the spec defines.
    pub(super) async fn serve_tags(&self, state: &AppState, name: &str, query: &str) -> Result<Response, ServeError> {
        let Some((index, repo)) = resolve(&state.indexes, name) else {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        };
        let members = serving_members(state, index);
        if let [member] = members.as_slice()
            && let Some(client) = proxy_client(&member.kind)
        {
            return match self.upstream.tags(client.base_url(), client.auth(), repo, query).await {
                Ok(response) => passthrough_json(response).await,
                Err(err) => Ok(upstream_error_response(&err, "tags")),
            };
        }
        let mut tags = std::collections::BTreeSet::new();
        for member in &members {
            match proxy_client(&member.kind) {
                Some(client) => {
                    if let Some(names) = self.fetch_tag_names(client, repo).await {
                        tags.extend(names);
                    }
                }
                None => tags.extend(store::list_tags(&state.meta, &member.name, repo)?),
            }
        }
        Ok(tag_list_response(name, tags, query))
    }

    /// Fetch a proxy member's tag names for aggregation, or `None` on any upstream failure so one
    /// unreachable member does not fail the whole list.
    async fn fetch_tag_names(&self, client: &UpstreamClient, repo: &str) -> Option<Vec<String>> {
        let mut names = Vec::new();
        let mut query = String::new();
        let mut page = 0;
        loop {
            let response = self
                .upstream
                .tags(client.base_url(), client.auth(), repo, &query)
                .await
                .ok()?;
            let next = next_page_query(response.headers());
            let bytes = bounded_body(response, MAX_TAGS_BYTES).await.ok()?;
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
    async fn upstream_referrers(&self, client: &UpstreamClient, repo: &str, digest: &str) -> Vec<serde_json::Value> {
        let Ok(response) = self
            .upstream
            .referrers(client.base_url(), client.auth(), repo, digest)
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
        state: &AppState,
        name: &str,
        digest: &str,
        query: &str,
    ) -> Result<Response, ServeError> {
        let Some((index, repo)) = resolve(&state.indexes, name) else {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        };
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
            if let Some(client) = proxy_client(&member.kind) {
                for descriptor in self.upstream_referrers(client, repo, digest).await {
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

/// Build a `tags/list` response over a sorted tag set, applying `n`/`last` pagination and a `Link`
/// header to the next page when the set is truncated.
/// Apply distribution-spec `n`/`last` pagination to a sorted set: the page after `last`, truncated to
/// `n`, and the `(n, last-of-page)` cursor for a `Link` when more remains.
fn paginate(items: std::collections::BTreeSet<String>, query: &str) -> (Vec<String>, Option<(usize, String)>) {
    let params = query_params(query);
    let last = params.get("last").map_or("", String::as_str);
    let limit = params.get("n").and_then(|value| value.parse::<usize>().ok());
    let mut page: Vec<String> = items.into_iter().filter(|item| item.as_str() > last).collect();
    let next = limit.filter(|&n| page.len() > n).map(|n| {
        page.truncate(n);
        (n, page.last().cloned().unwrap_or_default())
    });
    (page, next)
}

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
pub(super) fn serve_catalog(state: &AppState, query: &str) -> Result<Response, ServeError> {
    let mut repositories = std::collections::BTreeSet::new();
    for index in &state.indexes {
        if index.ecosystem != Ecosystem::Oci {
            continue;
        }
        for repo in store::list_repositories(&state.meta, &index.name)? {
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

/// Pass an upstream JSON response through, preserving its status and content type.
async fn passthrough_json(response: reqwest::Response) -> Result<Response, ServeError> {
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let bytes = bounded_body(response, MAX_TAGS_BYTES).await?;
    Ok((status, [(header::CONTENT_TYPE, content_type)], bytes).into_response())
}

/// The query string of an upstream tag-list `Link: <…?…>; rel="next"` header, or `None` when the page
/// is the last. Aggregation follows it so a paginating upstream's tags are not silently truncated to
/// the first page.
fn next_page_query(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let link = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    if !link.contains("rel=\"next\"") {
        return None;
    }
    let start = link.find('<')? + 1;
    let end = link[start..].find('>')? + start;
    link[start..end].split_once('?').map(|(_, query)| query.to_owned())
}
