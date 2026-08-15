use peryx_core::url_encoding::push_component;

#[must_use]
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(crate) fn ui_browse_url(raw_query: &str) -> String {
    let mut url = "/+ui/browse".to_owned();
    if !raw_query.is_empty() {
        url.push('?');
        url.push_str(raw_query);
    }
    url
}

#[must_use]
pub(crate) fn browse_index_url(route: &str) -> String {
    let mut url = "/browse".to_owned();
    QueryAppender::new(&mut url).push("index", route);
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
    append_search_query(
        &mut QueryAppender::new(&mut url),
        query,
        source_type,
        availability,
        page,
        page_size,
    );
    url
}

#[must_use]
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(crate) fn search_api_url(
    query: &str,
    source_type: &str,
    availability: &str,
    page: usize,
    page_size: usize,
) -> String {
    let mut url = "/+search".to_owned();
    let mut appender = QueryAppender::new(&mut url);
    append_search_query(&mut appender, query, source_type, availability, page, page_size);
    url
}

fn append_search_query(
    appender: &mut QueryAppender<'_>,
    query: &str,
    source_type: &str,
    availability: &str,
    page: usize,
    page_size: usize,
) {
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
pub(crate) fn stats_index_url(route: &str) -> String {
    let mut url = "/stats".to_owned();
    QueryAppender::new(&mut url).push("index", route);
    url
}

#[must_use]
pub(crate) fn stats_resource_url(route: &str, resource: &str) -> String {
    let mut url = stats_index_url(route);
    push_query(&mut url, "resource", resource);
    url
}

#[must_use]
#[cfg(all(not(feature = "ssr"), feature = "hydrate"))]
pub(crate) fn stats_api_url(route: Option<&str>, resource: Option<&str>) -> String {
    let mut url = "/+stats".to_owned();
    if let Some(route) = route {
        let mut query = QueryAppender::new(&mut url);
        query.push("index", route);
        if let Some(resource) = resource {
            query.push("resource", resource);
        }
    }
    url
}

struct QueryAppender<'a> {
    url: &'a mut String,
    separator: char,
}

impl<'a> QueryAppender<'a> {
    const fn new(url: &'a mut String) -> Self {
        Self { url, separator: '?' }
    }

    const fn continuing(url: &'a mut String) -> Self {
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
#[path = "../tests/unit/url/tests.rs"]
mod tests;
