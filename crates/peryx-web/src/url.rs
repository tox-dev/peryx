use peryx_core::url_encoding::{push_component, push_path};

/// The client-facing API endpoint an index is served at: the Simple index for a `PyPI` index, the
/// `/v2/` registry namespace for an `OCI` one. The status card shows this so a client knows where to
/// point, and it must not be a `PyPI` `/simple/` URL for a registry that answers `/v2/`.
#[must_use]
pub(crate) fn index_endpoint(route: &str, ecosystem: &str) -> String {
    if ecosystem == "oci" {
        let mut url = "/v2/".to_owned();
        push_path(&mut url, route);
        url.push('/');
        url
    } else {
        simple_index_url(route)
    }
}

#[must_use]
pub(crate) fn simple_index_url(route: &str) -> String {
    let mut url = String::with_capacity(route.len() + 9);
    url.push('/');
    push_path(&mut url, route);
    url.push_str("/simple/");
    url
}

#[must_use]
#[cfg(any(feature = "hydrate", test))]
pub(crate) fn simple_project_url(route: &str, project: &str) -> String {
    let mut url = simple_index_url(route);
    push_component(&mut url, project);
    url.push('/');
    url
}

/// The OCI tag-list endpoint for one repository under an index route: `/v2/<route>/<repo>/tags/list`.
#[must_use]
pub(crate) fn oci_tags_url(route: &str, repo: &str) -> String {
    let mut url = "/v2/".to_owned();
    push_path(&mut url, route);
    url.push('/');
    push_path(&mut url, repo);
    url.push_str("/tags/list");
    url
}

/// The OCI manifest endpoint for one reference: `/v2/<route>/<repo>/manifests/<reference>`.
#[must_use]
pub(crate) fn oci_manifest_url(route: &str, repo: &str, reference: &str) -> String {
    let mut url = "/v2/".to_owned();
    push_path(&mut url, route);
    url.push('/');
    push_path(&mut url, repo);
    url.push_str("/manifests/");
    push_component(&mut url, reference);
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

/// The browse URL for one tag's manifest under an OCI repository (`?index&project&ref`).
#[must_use]
pub(crate) fn browse_oci_ref_url(route: &str, repo: &str, reference: &str) -> String {
    let mut url = browse_project_url(route, repo);
    push_query(&mut url, "ref", reference);
    url
}

/// The browse URL for one layer's contents under an OCI manifest (`?index&project&ref&layer`).
#[must_use]
pub(crate) fn browse_oci_layer_url(route: &str, repo: &str, reference: &str, digest: &str) -> String {
    let mut url = browse_oci_ref_url(route, repo, reference);
    push_query(&mut url, "layer", digest);
    url
}

/// The browse URL for one member inside a layer, carrying the paging offset when past the first chunk.
#[must_use]
pub(crate) fn browse_oci_layer_member_url(
    route: &str,
    repo: &str,
    reference: &str,
    digest: &str,
    member: &str,
    offset: u64,
) -> String {
    let mut url = browse_oci_layer_url(route, repo, reference, digest);
    let mut query = QueryAppender::continuing(&mut url);
    query.push("member", member);
    if offset > 0 {
        query.push("offset", &offset.to_string());
    }
    url
}

/// The peryx layer-browser endpoint: `/v2/<route>/<repo>/blobs/<digest>/contents`, listing the
/// layer's members or (with `member`) previewing one.
#[must_use]
pub(crate) fn oci_layer_inspect_url(
    route: &str,
    repo: &str,
    digest: &str,
    member: Option<&str>,
    offset: u64,
) -> String {
    let mut url = "/v2/".to_owned();
    push_path(&mut url, route);
    url.push('/');
    push_path(&mut url, repo);
    url.push_str("/blobs/");
    // A digest is `algorithm:hex`, all URL-path-safe, and the `/v2/` route parser matches the literal
    // colon, so it must not be percent-encoded the way an arbitrary path component would be.
    url.push_str(digest);
    url.push_str("/contents");
    if let Some(member) = member {
        let mut query = QueryAppender::new(&mut url);
        query.push("member", member);
        query.push("offset", &offset.to_string());
    }
    url
}

#[must_use]
pub(crate) fn browse_project_file_search_url(route: &str, project: &str, pattern: &str, regex: bool) -> String {
    let mut url = browse_project_url(route, project);
    if !pattern.is_empty() {
        push_query(&mut url, "filename", pattern);
    }
    if regex {
        push_query(&mut url, "filename_match", "regex");
    }
    url
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
pub(crate) fn search_page_url(query: &str, source_type: &str, page: usize, page_size: usize) -> String {
    let mut url = "/search".to_owned();
    append_search_query(&mut url, None, query, source_type, page, page_size);
    url
}

#[must_use]
#[cfg(any(feature = "hydrate", test))]
pub(crate) fn search_api_url(
    route: Option<&str>,
    query: &str,
    source_type: &str,
    page: usize,
    page_size: usize,
) -> String {
    let mut url = "/+search".to_owned();
    append_search_query(&mut url, route, query, source_type, page, page_size);
    url
}

fn append_search_query(
    url: &mut String,
    route: Option<&str>,
    query: &str,
    source_type: &str,
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
    if page > 1 {
        appender.push("page", &page.to_string());
    }
    appender.push("page_size", &page_size.to_string());
}

#[must_use]
#[cfg(any(feature = "hydrate", test))]
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
#[cfg(any(feature = "hydrate", test))]
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

#[cfg(test)]
mod tests {
    use super::{
        admin_project_url, admin_version_url, browse_archive_listing_url, browse_archive_member_url,
        browse_archive_url, browse_index_url, browse_project_file_search_url, browse_project_url, inspect_url,
        search_api_url, search_page_url, simple_index_url, simple_project_url, stats_api_url, stats_index_url,
        stats_project_url,
    };

    #[test]
    fn test_package_urls_encode_paths_and_queries() {
        assert_eq!(simple_index_url("root/pypi"), "/root/pypi/simple/");
        assert_eq!(
            simple_project_url("root/pypi", "pkg name"),
            "/root/pypi/simple/pkg%20name/"
        );
        assert_eq!(browse_index_url("root/pypi"), "/browse?index=root%2Fpypi");
        assert_eq!(
            browse_project_url("root/pypi", "pkg name"),
            "/browse?index=root%2Fpypi&project=pkg%20name"
        );
        assert_eq!(
            browse_archive_url("root/pypi", "pkg name", "aa", "pkg 1.0#x?.whl"),
            "/browse?index=root%2Fpypi&project=pkg%20name&sha256=aa&file=pkg%201.0%23x%3F.whl"
        );
        assert_eq!(
            browse_project_file_search_url("root/pypi", "pkg name", "cp313.*\\.whl", true),
            "/browse?index=root%2Fpypi&project=pkg%20name&filename=cp313.%2A%5C.whl&filename_match=regex"
        );
    }

    #[test]
    fn test_archive_urls_encode_nested_members() {
        let containers = vec!["vendor/inner #1.zip".to_owned()];
        assert_eq!(
            browse_archive_listing_url("root/pypi", "pkg", "aa", "pkg.whl", &containers),
            "/browse?index=root%2Fpypi&project=pkg&sha256=aa&file=pkg.whl&container=vendor%2Finner%20%231.zip"
        );
        assert_eq!(
            browse_archive_member_url("root/pypi", "pkg", "aa", "pkg.whl", &containers, "pkg/mod #1.py", 1024),
            "/browse?index=root%2Fpypi&project=pkg&sha256=aa&file=pkg.whl&container=vendor%2Finner%20%231.zip&member=pkg%2Fmod%20%231.py&offset=1024"
        );
        assert_eq!(
            inspect_url(
                "root/pypi",
                "aa",
                "pkg 1.0.whl",
                &containers,
                Some("pkg/mod #1.py"),
                1024
            ),
            "/root/pypi/inspect/aa/pkg%201.0.whl?container=vendor%2Finner%20%231.zip&member=pkg%2Fmod%20%231.py&offset=1024"
        );
    }

    #[test]
    fn test_stats_and_admin_urls_encode_arguments() {
        assert_eq!(
            search_page_url("flask cache", "override", 2, 50),
            "/search?q=flask%20cache&type=override&page=2&page_size=50"
        );
        assert_eq!(
            search_api_url(Some("root/pypi"), "flask", "all", 1, 25),
            "/+search?route=root%2Fpypi&q=flask&page_size=25"
        );
        assert_eq!(stats_index_url("root/pypi"), "/stats?index=root%2Fpypi");
        assert_eq!(
            stats_project_url("root/pypi", "pkg name"),
            "/stats?index=root%2Fpypi&project=pkg%20name"
        );
        assert_eq!(
            stats_api_url(Some("root/pypi"), Some("pkg name")),
            "/+stats?index=root%2Fpypi&project=pkg%20name"
        );
        assert_eq!(admin_project_url("root/pypi", "pkg name"), "/root/pypi/pkg%20name/");
        assert_eq!(
            admin_version_url("root/pypi", "pkg name", "1.0+local", Some("yank")),
            "/root/pypi/pkg%20name/1.0%2Blocal/yank"
        );
    }
}
