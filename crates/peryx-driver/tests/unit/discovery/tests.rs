use axum::http::{HeaderMap, HeaderValue, Uri};
use rstest::rstest;

use super::{BaseUrl, browse_path};

#[test]
fn test_base_url_rejects_credentials_query_and_fragment() {
    for url in [
        "not a url",
        "file:///tmp/simple",
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
        HeaderValue::from_static("packages.example, proxy.local"),
    );
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    let base = BaseUrl::from_request(&headers, &Uri::from_static("/+api"), true).unwrap();
    assert_eq!(
        base.join("/root/alpha/simple/"),
        "https://packages.example/root/alpha/simple/"
    );
}

#[rstest]
#[case::no_headers(None, None)]
#[case::unsupported_scheme(Some("packages.example"), Some("ssh"))]
#[case::credentials_in_host(Some("user@packages.example"), Some("https"))]
#[case::invalid_host(Some("packages example"), Some("https"))]
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
    headers.insert("host", HeaderValue::from_static("packages.example"));
    let base = BaseUrl::from_request(&headers, &Uri::from_static("/+api"), false).unwrap();
    assert_eq!(
        base.join("/root/alpha/simple/"),
        "http://packages.example/root/alpha/simple/"
    );
}

#[test]
fn test_base_url_from_request_ignores_untrusted_forwarded_origin() {
    let mut headers = HeaderMap::new();
    headers.insert("host", HeaderValue::from_static("packages.example"));
    headers.insert("x-forwarded-host", HeaderValue::from_static("attacker.example"));
    headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
    let base = BaseUrl::from_request(&headers, &Uri::from_static("/+api"), false).unwrap();
    assert_eq!(base.join("/+api"), "http://packages.example/+api");
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
