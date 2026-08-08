use peryx_core::url_encoding::{push_component, push_path};

#[must_use]
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(crate) fn browser_http_origin(protocol: &str, hostname: &str, port: &str) -> Option<String> {
    if hostname.is_empty() || !matches!(protocol, "http:" | "https:") {
        return None;
    }
    let port = (!port.is_empty() && !matches!((protocol, port), ("http:", "80") | ("https:", "443"))).then_some(port);
    let mut origin = String::with_capacity(protocol.len() + hostname.len() + port.map_or(2, |value| value.len() + 3));
    origin.push_str(protocol);
    origin.push_str("//");
    origin.push_str(hostname);
    if let Some(port) = port {
        origin.push(':');
        origin.push_str(port);
    }
    Some(origin)
}

/// The neutral browse-data endpoint for one index's project names: `/+ui/projects?index=<route>`.
#[must_use]
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(crate) fn ui_projects_url(route: &str) -> String {
    let mut url = "/+ui/projects".to_owned();
    QueryAppender::new(&mut url).push("index", route);
    url
}

/// The neutral browse-data endpoint for one project's view: `/+ui/project?index&project`.
#[must_use]
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(crate) fn ui_project_url(route: &str, project: &str) -> String {
    let mut url = "/+ui/project".to_owned();
    let mut query = QueryAppender::new(&mut url);
    query.push("index", route);
    query.push("project", project);
    url
}

/// The neutral browse-data endpoint for one manifest view: `/+ui/manifest?index&project&ref`.
#[must_use]
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(crate) fn ui_manifest_url(route: &str, repo: &str, reference: &str) -> String {
    let mut url = "/+ui/manifest".to_owned();
    let mut query = QueryAppender::new(&mut url);
    query.push("index", route);
    query.push("project", repo);
    query.push("ref", reference);
    url
}

/// The neutral browse-data endpoint listing a nested item's members: `/+ui/members?index&project&digest`.
#[must_use]
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(crate) fn ui_members_url(route: &str, repo: &str, digest: &str) -> String {
    let mut url = "/+ui/members".to_owned();
    let mut query = QueryAppender::new(&mut url);
    query.push("index", route);
    query.push("project", repo);
    query.push("digest", digest);
    url
}

/// The neutral browse-data endpoint for one member chunk: `/+ui/member?index&project&digest&member&offset`.
#[must_use]
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(crate) fn ui_member_url(route: &str, repo: &str, digest: &str, member: &str, offset: u64) -> String {
    let mut url = "/+ui/member".to_owned();
    let mut query = QueryAppender::new(&mut url);
    query.push("index", route);
    query.push("project", repo);
    query.push("digest", digest);
    query.push("member", member);
    query.push("offset", &offset.to_string());
    url
}

#[must_use]
pub(crate) fn browse_index_url(route: &str) -> String {
    let mut url = "/browse".to_owned();
    QueryAppender::new(&mut url).push("index", route);
    url
}

#[must_use]
pub(crate) fn browse_project_url(route: &str, project: &str) -> String {
    let mut url = browse_index_url(route);
    push_query(&mut url, "project", project);
    url
}

/// The browse URL for one reference's manifest under a repository (`?index&project&ref`).
#[must_use]
pub(crate) fn browse_ref_url(route: &str, repo: &str, reference: &str) -> String {
    let mut url = browse_project_url(route, repo);
    push_query(&mut url, "ref", reference);
    url
}

/// The browse URL for one layer's contents under a manifest (`?index&project&ref&layer`).
#[must_use]
pub(crate) fn browse_layer_url(route: &str, repo: &str, reference: &str, digest: &str) -> String {
    let mut url = browse_ref_url(route, repo, reference);
    push_query(&mut url, "layer", digest);
    url
}

/// The browse URL for one member inside a layer, carrying the paging offset when past the first chunk.
#[must_use]
pub(crate) fn browse_layer_member_url(
    route: &str,
    repo: &str,
    reference: &str,
    digest: &str,
    member: &str,
    offset: u64,
) -> String {
    let mut url = browse_layer_url(route, repo, reference, digest);
    let mut query = QueryAppender::continuing(&mut url);
    query.push("member", member);
    if offset > 0 {
        query.push("offset", &offset.to_string());
    }
    url
}

#[must_use]
pub(crate) fn browse_project_file_search_url(
    route: &str,
    project: &str,
    version: Option<&str>,
    pattern: &str,
    regex: bool,
) -> String {
    let mut url = browse_project_url(route, project);
    append_project_file_search(&mut url, version, pattern, regex);
    url
}

/// One release on a project page, with an optional filename filter that remains part of the direct
/// link.
#[must_use]
pub(crate) fn browse_project_release_url(
    route: &str,
    project: &str,
    version: &str,
    pattern: &str,
    regex: bool,
) -> String {
    let mut url = browse_project_url(route, project);
    append_project_file_search(&mut url, Some(version), pattern, regex);
    url
}

fn append_project_file_search(url: &mut String, version: Option<&str>, pattern: &str, regex: bool) {
    if let Some(version) = version {
        push_query(url, "version", version);
    }
    if !pattern.is_empty() {
        push_query(url, "filename", pattern);
    }
    if regex {
        push_query(url, "filename_match", "regex");
    }
}

#[must_use]
pub(crate) fn browse_archive_url(route: &str, project: &str, sha256: &str, filename: &str) -> String {
    let mut url = browse_project_url(route, project);
    push_query(&mut url, "sha256", sha256);
    push_query(&mut url, "file", filename);
    url
}

#[must_use]
pub(crate) fn browse_archive_listing_url(
    route: &str,
    project: &str,
    sha256: &str,
    filename: &str,
    containers: &[String],
) -> String {
    let mut url = browse_archive_url(route, project, sha256, filename);
    for container in containers {
        push_query(&mut url, "container", container);
    }
    url
}

#[must_use]
pub(crate) fn browse_archive_member_url(
    route: &str,
    project: &str,
    sha256: &str,
    filename: &str,
    containers: &[String],
    member: &str,
    offset: u64,
) -> String {
    let mut url = browse_archive_listing_url(route, project, sha256, filename, containers);
    let mut query = QueryAppender::continuing(&mut url);
    query.push("member", member);
    if offset > 0 {
        query.push("offset", &offset.to_string());
    }
    url
}

#[must_use]
pub(crate) fn search_page_url(
    query: &str,
    source_type: &str,
    availability: &str,
    page: usize,
    page_size: usize,
) -> String {
    let mut url = "/search".to_owned();
    append_search_query(&mut url, None, query, source_type, availability, page, page_size);
    url
}

#[must_use]
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(crate) fn search_api_url(
    route: Option<&str>,
    query: &str,
    source_type: &str,
    availability: &str,
    page: usize,
    page_size: usize,
) -> String {
    let mut url = "/+search".to_owned();
    append_search_query(&mut url, route, query, source_type, availability, page, page_size);
    url
}

fn append_search_query(
    url: &mut String,
    route: Option<&str>,
    query: &str,
    source_type: &str,
    availability: &str,
    page: usize,
    page_size: usize,
) {
    let mut appender = QueryAppender::new(url);
    if let Some(route) = route {
        appender.push("route", route);
    }
    if !query.is_empty() {
        appender.push("q", query);
    }
    if !source_type.is_empty() && source_type != "all" {
        appender.push("type", source_type);
    }
    if !availability.is_empty() && availability != "all" {
        appender.push("availability", availability);
    }
    if page > 1 {
        appender.push("page", &page.to_string());
    }
    appender.push("page_size", &page_size.to_string());
}

#[must_use]
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(crate) fn inspect_url(
    route: &str,
    sha256: &str,
    filename: &str,
    containers: &[String],
    member: Option<&str>,
    offset: u64,
) -> String {
    let mut url = String::with_capacity(route.len() + sha256.len() + filename.len() + 11);
    url.push('/');
    push_path(&mut url, route);
    url.push_str("/inspect/");
    push_component(&mut url, sha256);
    url.push('/');
    push_component(&mut url, filename);
    let mut query = QueryAppender::new(&mut url);
    for container in containers {
        query.push("container", container);
    }
    if let Some(member) = member {
        query.push("member", member);
        query.push("offset", &offset.to_string());
    }
    url
}

#[must_use]
pub(crate) fn stats_index_url(route: &str) -> String {
    let mut url = "/stats".to_owned();
    QueryAppender::new(&mut url).push("index", route);
    url
}

#[must_use]
pub(crate) fn stats_project_url(route: &str, project: &str) -> String {
    let mut url = stats_index_url(route);
    push_query(&mut url, "project", project);
    url
}

#[must_use]
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(crate) fn stats_api_url(route: Option<&str>, project: Option<&str>) -> String {
    let mut url = "/+stats".to_owned();
    if let Some(route) = route {
        let mut query = QueryAppender::new(&mut url);
        query.push("index", route);
        if let Some(project) = project {
            query.push("project", project);
        }
    }
    url
}

#[must_use]
pub(crate) fn admin_project_url(route: &str, project: &str) -> String {
    let mut url = String::with_capacity(route.len() + project.len() + 3);
    url.push('/');
    push_path(&mut url, route);
    url.push('/');
    push_component(&mut url, project);
    url.push('/');
    url
}

#[must_use]
pub(crate) fn admin_version_url(route: &str, project: &str, version: &str, action: Option<&str>) -> String {
    let mut url = String::with_capacity(route.len() + project.len() + version.len() + action.map_or(0, str::len) + 4);
    url.push('/');
    push_path(&mut url, route);
    url.push('/');
    push_component(&mut url, project);
    url.push('/');
    push_component(&mut url, version);
    url.push('/');
    if let Some(action) = action {
        url.push_str(action);
    }
    url
}

struct QueryAppender<'a> {
    url: &'a mut String,
    separator: char,
}

impl<'a> QueryAppender<'a> {
    fn new(url: &'a mut String) -> Self {
        Self { url, separator: '?' }
    }

    fn continuing(url: &'a mut String) -> Self {
        Self { url, separator: '&' }
    }

    fn push(&mut self, key: &str, value: &str) {
        self.url.push(self.separator);
        self.url.push_str(key);
        self.url.push('=');
        push_component(self.url, value);
        self.separator = '&';
    }
}

fn push_query(url: &mut String, key: &str, value: &str) {
    QueryAppender::continuing(url).push(key, value);
}

#[cfg(all(test, not(feature = "ssr"), feature = "hydrate"))]
#[path = "../tests/unit/url/tests.rs"]
mod tests;
