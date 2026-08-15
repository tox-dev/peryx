use axum::http::{HeaderMap, HeaderValue, Uri};
use rstest::rstest;
use serde_json::json;

use super::{BaseUrl, browse_path, index_envelope, link, minimal_entry, root_envelope, stats_path};
use crate::state::IndexDescription;

#[test]
fn test_base_url_rejects_credentials_query_and_fragment() {
    for url in [
        "not a url",
        "file:///tmp/artifacts",
        "https://user@example.test/",
        "https://example.test/?x=1",
        "https://example.test/#frag",
    ] {
        assert!(BaseUrl::parse(url).is_err(), "{url}");
    }
}

#[test]
fn test_base_url_from_request_uses_forwarded_origin() {
    let mut headers = HeaderMap::new();
    headers.insert("host", HeaderValue::from_static("internal.test"));
    headers.insert(
        "x-forwarded-host",
        HeaderValue::from_static("catalog.example, proxy.local"),
    );
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    let base = BaseUrl::from_request(&headers, &Uri::from_static("/+api"), true).unwrap();
    assert_eq!(
        base.join("/root/alpha/items/"),
        "https://catalog.example/root/alpha/items/"
    );
}

#[rstest]
#[case::no_headers(None, None)]
#[case::unsupported_scheme(Some("catalog.example"), Some("ssh"))]
#[case::credentials_in_host(Some("user@catalog.example"), Some("https"))]
#[case::invalid_host(Some("catalog example"), Some("https"))]
fn test_base_url_from_request_rejects_invalid_forwarded_origin(
    #[case] host: Option<&str>,
    #[case] proto: Option<&str>,
) {
    let mut headers = HeaderMap::new();
    if let Some(host) = host {
        headers.insert("x-forwarded-host", HeaderValue::from_str(host).unwrap());
    }
    if let Some(proto) = proto {
        headers.insert("x-forwarded-proto", HeaderValue::from_str(proto).unwrap());
    }
    assert!(BaseUrl::from_request(&headers, &Uri::from_static("/+api"), true).is_none());
}

#[test]
fn test_base_url_from_request_uses_host_header_without_proxy_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("host", HeaderValue::from_static("catalog.example"));
    let base = BaseUrl::from_request(&headers, &Uri::from_static("/+api"), false).unwrap();
    assert_eq!(
        base.join("/root/alpha/items/"),
        "http://catalog.example/root/alpha/items/"
    );
}

#[test]
fn test_base_url_from_request_ignores_untrusted_forwarded_origin() {
    let mut headers = HeaderMap::new();
    headers.insert("host", HeaderValue::from_static("catalog.example"));
    headers.insert("x-forwarded-host", HeaderValue::from_static("attacker.example"));
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    let base = BaseUrl::from_request(&headers, &Uri::from_static("/+api"), false).unwrap();
    assert_eq!(base.join("/+api"), "http://catalog.example/+api");
}

#[test]
fn test_host_port_strips_scheme() {
    let base = BaseUrl::parse("https://registry.example:5000/cache/").unwrap();
    assert_eq!(base.host_port(), "registry.example:5000");
}

#[test]
fn test_browse_url_percent_encodes_route_query() {
    assert_eq!(browse_path("root/alpha"), "/browse?index=root%2Falpha");
}

#[test]
fn test_stats_url_percent_encodes_route_query() {
    assert_eq!(stats_path("root/alpha"), "/stats?index=root%2Falpha");
}

#[test]
fn test_link_uses_relative_path_without_base() {
    assert_eq!(link(None, "/+api"), "/+api");
}

#[test]
fn test_discovery_documents_keep_neutral_index_data() {
    let entry = minimal_entry(&IndexDescription {
        name: "catalog".to_owned(),
        route: "team/catalog".to_owned(),
        ecosystem: "example".to_owned(),
        kind: "mirror",
        layers: Vec::new(),
        precedence: Vec::new(),
        uploads: false,
        volatile_deletes: false,
        upload_to: None,
        upstream: None,
        hosted: None,
    });

    assert_eq!(entry["name"], "catalog");
    assert_eq!(entry["route"], "team/catalog");
    assert_eq!(entry["ecosystem"], "example");
    assert_eq!(index_envelope(entry.clone())["index"], entry);
}

#[test]
fn test_root_envelope_renders_absolute_service_urls() {
    let base = BaseUrl::parse("https://catalog.example/prefix/").unwrap();
    let document = root_envelope(Some(&base), vec![json!({"name": "catalog"})]);

    assert_eq!(document["urls"]["api"], "https://catalog.example/prefix/+api");
    assert_eq!(document["urls"]["health"], "https://catalog.example/prefix/+health");
    assert_eq!(document["urls"]["readiness"], "https://catalog.example/prefix/+ready");
    assert_eq!(document["urls"]["status"], "https://catalog.example/prefix/+status");
    assert_eq!(document["urls"]["stats"], "https://catalog.example/prefix/+stats");
    assert_eq!(
        document["urls"]["openapi"],
        "https://catalog.example/prefix/api-docs/openapi.json"
    );
    assert_eq!(document["urls"]["web"], "https://catalog.example/prefix/");
    assert_eq!(document["indexes"], json!([{"name": "catalog"}]));
}

#[test]
fn test_request_uri_origin_precedes_forwarded_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-host", HeaderValue::from_static("proxy.example"));
    headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    let uri = Uri::from_static("https://catalog.example/+api");

    assert_eq!(
        BaseUrl::from_request(&headers, &uri, true).unwrap().host_port(),
        "catalog.example"
    );
}

#[test]
fn test_empty_forwarded_value_falls_back_to_host() {
    let mut headers = HeaderMap::new();
    headers.insert("host", HeaderValue::from_static("catalog.example"));
    headers.insert("x-forwarded-host", HeaderValue::from_static(" , proxy.example"));

    assert_eq!(
        BaseUrl::from_request(&headers, &Uri::from_static("/+api"), true)
            .unwrap()
            .host_port(),
        "catalog.example"
    );
}
