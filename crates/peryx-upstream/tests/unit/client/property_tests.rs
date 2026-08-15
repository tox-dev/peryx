use std::net::Ipv4Addr;
use std::path::Path;

use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::{Auth, Netrc, UpstreamClient, UpstreamError};

#[test]
fn test_netrc_lookup_depends_only_on_url_origin() {
    for (machine, origin) in [
        ("resources.example", "https://resources.example"),
        ("resources.example:8443", "https://resources.example:8443"),
        ("[2001:db8::7]:8443", "https://[2001:db8::7]:8443"),
    ] {
        let (_directory, netrc) = load_netrc(&format!("machine {machine} login reader password secret\n"));
        for suffix in ["/", "/api/key/", "/files/artifact.bin?read=1", "/api/key/#revision"] {
            assert_eq!(
                netrc.auth_for(&Url::parse(&format!("{origin}{suffix}")).unwrap()),
                Auth::Basic {
                    username: "reader".to_owned(),
                    password: "secret".to_owned(),
                },
                "{origin}{suffix}"
            );
        }
    }
}

#[tokio::test]
async fn test_private_ipv4_literals_never_reach_transport() {
    let client = UpstreamClient::new("https://public.example/api/").unwrap();
    let mut addresses = Vec::new();
    for host in [0, 1, 127, 254, 255] {
        addresses.extend([
            Ipv4Addr::new(10, host, host, host),
            Ipv4Addr::new(127, host, host, host),
            Ipv4Addr::new(169, 254, host, host),
            Ipv4Addr::new(192, 168, host, host),
        ]);
    }
    for second in 16..=31 {
        addresses.push(Ipv4Addr::new(172, second, 0, 1));
        addresses.push(Ipv4Addr::new(172, second, 255, 254));
    }
    for second in 64..=127 {
        addresses.push(Ipv4Addr::new(100, second, 0, 1));
        addresses.push(Ipv4Addr::new(100, second, 255, 254));
    }

    for address in addresses {
        let error = client
            .fetch_bytes(&format!("http://{address}:1/artifact"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, UpstreamError::BlockedDestination { .. }),
            "{address}: {error:?}"
        );
    }
}

#[tokio::test]
async fn test_head_range_parses_byte_tokens_and_lengths() {
    let server = MockServer::start().await;
    let client = UpstreamClient::new(&format!("{}/api/", server.uri())).unwrap();

    for (case, accept_ranges, length) in [
        ("lower", "bytes", 0),
        ("upper", "BYTES", 1),
        ("list-first", "bytes, none", 65_535),
        ("list-last", "none, bytes", u64::from(u32::MAX)),
    ] {
        let route = format!("/files/{case}");
        Mock::given(method("HEAD"))
            .and(path(route.clone()))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("accept-ranges", accept_ranges)
                    .insert_header("content-length", length.to_string()),
            )
            .mount(&server)
            .await;

        assert_eq!(
            client
                .head_file_for_range(&format!("{}{route}", server.uri()))
                .await
                .unwrap()
                .len,
            length,
            "{case}"
        );
    }
}

#[tokio::test]
async fn test_range_response_round_trips_generated_spans() {
    let server = MockServer::start().await;
    let client = UpstreamClient::new(&format!("{}/api/", server.uri())).unwrap();

    for (case, start, length, known_total) in [
        ("first", 0, 1, Some(1)),
        ("middle", 7, 4, Some(32)),
        ("large-offset", u64::from(u32::MAX), 8, Some(u64::from(u32::MAX) + 9)),
        ("unknown-total", 19, 16, None),
    ] {
        let route = format!("/files/{case}");
        let end = start + length - 1;
        let total = known_total.map_or_else(|| "*".to_owned(), |value| value.to_string());
        let body = vec![u8::try_from(length).unwrap(); usize::try_from(length).unwrap()];
        Mock::given(method("GET"))
            .and(path(route.clone()))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("content-range", format!("bytes {start}-{end}/{total}"))
                    .set_body_bytes(body.clone()),
            )
            .mount(&server)
            .await;

        assert_eq!(
            client
                .fetch_range(&format!("{}{route}", server.uri()), start, end)
                .await
                .unwrap()
                .as_ref(),
            body,
            "{case}"
        );
    }
}

fn load_netrc(contents: &str) -> (tempfile::TempDir, Netrc) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.netrc");
    write_netrc(&path, contents);
    let netrc = Netrc::from_path(&path).unwrap();
    (directory, netrc)
}

fn write_netrc(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
