use rstest::rstest;

use crate::{MemberEndpoint, MemberEndpointError};

#[rstest]
#[case::plain("http://host.internal:4460", "http://host.internal:4460/")]
#[case::tls("https://host.internal:8443", "https://host.internal:8443/")]
#[case::root_path("https://host.internal:8443/", "https://host.internal:8443/")]
#[case::uppercase_host("https://Host.Internal:8443", "https://host.internal:8443/")]
#[case::uppercase_scheme("HTTPS://host.internal:8443", "https://host.internal:8443/")]
#[case::default_https_port("https://host.internal:443", "https://host.internal:443/")]
#[case::default_http_port("http://host.internal:80", "http://host.internal:80/")]
#[case::ipv6("http://[::1]:4460", "http://[::1]:4460/")]
#[case::idna("https://ünicode.example:8443", "https://xn--nicode-2ya.example:8443/")]
fn member_endpoint_canonicalizes(#[case] address: &str, #[case] expected: &str) {
    assert_eq!(MemberEndpoint::parse(address).unwrap().as_str(), expected);
}

#[rstest]
#[case::malformed("not a url", MemberEndpointError::Malformed("not a url".to_owned()))]
#[case::scheme("unix:/var/run/peryx.sock", MemberEndpointError::Scheme("unix:/var/run/peryx.sock".to_owned()))]
#[case::missing_port("http://host.internal", MemberEndpointError::MissingPort("http://host.internal".to_owned()))]
#[case::empty_port("http://host.internal:", MemberEndpointError::MissingPort("http://host.internal:".to_owned()))]
#[case::bare_ipv6("http://[::1]", MemberEndpointError::MissingPort("http://[::1]".to_owned()))]
#[case::path(
    "http://host.internal:4460/raft",
    MemberEndpointError::ExtraComponents("http://host.internal:4460/raft".to_owned())
)]
#[case::query(
    "http://host.internal:4460/?dc=east",
    MemberEndpointError::ExtraComponents("http://host.internal:4460/?dc=east".to_owned())
)]
#[case::fragment(
    "http://host.internal:4460/#east",
    MemberEndpointError::ExtraComponents("http://host.internal:4460/#east".to_owned())
)]
#[case::credentials(
    "http://peer:secret@host.internal:4460",
    MemberEndpointError::ExtraComponents("http://peer:secret@host.internal:4460".to_owned())
)]
#[case::password_only(
    "http://:secret@host.internal:4460",
    MemberEndpointError::ExtraComponents("http://:secret@host.internal:4460".to_owned())
)]
#[case::schemeless_authority(
    "http:host.internal:4460",
    MemberEndpointError::MissingPort("http:host.internal:4460".to_owned())
)]
fn member_endpoint_rejects(#[case] address: &str, #[case] expected: MemberEndpointError) {
    assert_eq!(MemberEndpoint::parse(address).unwrap_err(), expected);
}

#[rstest]
#[case::malformed("not a url", "is not a valid URL")]
#[case::scheme("unix:/var/run/peryx.sock", "http or https scheme")]
#[case::missing_port("http://host.internal", "explicit `host:port`")]
#[case::path("http://host.internal:4460/raft", "no path, query, fragment, or credentials")]
fn member_endpoint_error_names_the_rejected_address(#[case] address: &str, #[case] reason: &str) {
    let message = MemberEndpoint::parse(address).unwrap_err().to_string();

    assert!(message.contains(address) && message.contains(reason), "{message}");
}

#[rstest]
#[case::default_port("https://host.internal:443")]
#[case::written_port("http://host.internal:4460")]
fn member_endpoint_parse_is_idempotent(#[case] address: &str) {
    let once = MemberEndpoint::parse(address).unwrap();

    assert_eq!(MemberEndpoint::parse(once.as_str()).unwrap(), once);
}

#[test]
fn member_endpoint_renders_its_canonical_form() {
    let endpoint = MemberEndpoint::parse("https://Host.Internal:8443").unwrap();
    let rendered = endpoint.to_string();

    assert_eq!(rendered, endpoint.into_string());
}

#[test]
fn member_endpoint_orders_and_hashes_by_canonical_form() {
    let spellings = ["https://peer.internal:443", "https://PEER.internal:443/"];
    let endpoints: std::collections::BTreeSet<_> = spellings
        .iter()
        .map(|address| MemberEndpoint::parse(address).unwrap())
        .collect();

    assert_eq!(endpoints.len(), 1);
}
