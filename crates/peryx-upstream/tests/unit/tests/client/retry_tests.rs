use std::fs::File;
use std::io::{Read as _, Seek as _};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use rstest::rstest;
use tracing::dispatcher::DefaultGuard;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::simple_client;
use crate::client::UpstreamClient;
use crate::client::retry::{MAX_RETRIES, retry_after};

#[rstest]
#[case::seconds(Some(b"5".as_slice()), Some(Duration::from_secs(5)))]
#[case::zero(Some(b"0".as_slice()), Some(Duration::from_secs(0)))]
#[case::at_cap(Some(b"30".as_slice()), Some(Duration::from_secs(30)))]
#[case::over_cap(Some(b"120".as_slice()), Some(Duration::from_secs(30)))]
#[case::padded(Some(b" 5 ".as_slice()), Some(Duration::from_secs(5)))]
#[case::malformed(Some(b"soon".as_slice()), None)]
#[case::non_ascii(Some(b"\xff".as_slice()), None)]
#[case::absent(None, None)]
fn test_retry_after_reads_the_header(#[case] value: Option<&[u8]>, #[case] expected: Option<Duration>) {
    let mut headers = HeaderMap::new();
    if let Some(value) = value {
        headers.insert(RETRY_AFTER, HeaderValue::from_bytes(value).unwrap());
    }

    assert_eq!(retry_after(&headers), expected);
}

#[test]
fn test_retry_after_caps_a_future_http_date() {
    let future = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
    let mut headers = HeaderMap::new();
    headers.insert(
        RETRY_AFTER,
        HeaderValue::from_str(&httpdate::fmt_http_date(future)).unwrap(),
    );

    assert_eq!(retry_after(&headers), Some(Duration::from_secs(30)));
}

#[test]
fn test_retry_after_ignores_a_past_http_date() {
    let past = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let mut headers = HeaderMap::new();
    headers.insert(
        RETRY_AFTER,
        HeaderValue::from_str(&httpdate::fmt_http_date(past)).unwrap(),
    );

    assert_eq!(retry_after(&headers), None);
}

#[tokio::test]
async fn test_fetch_bytes_honors_retry_after_on_a_retryable_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let bytes = client
        .fetch_bytes(&format!("{}/files/pkg.bin", server.uri()))
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
}

#[tokio::test(start_paused = true)]
async fn test_sleep_before_retry_logs_a_redacted_url_and_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let error = reqwest::get(server.uri())
        .await
        .unwrap()
        .error_for_status()
        .unwrap_err();
    let url = url::Url::parse("https://user:secret@example.test/private?token=signed#fragment").unwrap();
    let (capture, guard) = capture_debug_events();

    crate::retry::sleep_before_retry(&url, 0, &error).await;

    let mut event = captured_event(capture, guard, "upstream retry");
    let delay: u64 = event["fields"]["delay_ms"].as_str().unwrap().parse().unwrap();
    assert!((50..=100).contains(&delay));
    event["fields"]["delay_ms"] = "jitter".into();
    assert_eq!(
        event["fields"],
        serde_json::json!({
            "message": "upstream retry",
            "url": "https://example.test/private",
            "error": "503",
            "delay_ms": "jitter",
        })
    );
}

#[tokio::test]
async fn test_status_retry_logs_a_redacted_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .and(query_param("token", "secret"))
        .respond_with(ResponseTemplate::new(408))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .and(query_param("token", "secret"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&server)
        .await;
    let client = simple_client(&server);
    let (capture, guard) = capture_debug_events();

    client
        .fetch_bytes(&format!("{}/files/pkg.bin?token=secret", server.uri()))
        .await
        .unwrap();

    let mut event = captured_event(capture, guard, "upstream returned retryable status");
    event["fields"]["delay_ms"] = "jitter".into();
    assert_eq!(
        event["fields"],
        serde_json::json!({
            "message": "upstream returned retryable status",
            "url": format!("{}/files/pkg.bin", server.uri()),
            "status": "408 Request Timeout",
            "delay_ms": "jitter",
        })
    );
}

#[tokio::test]
async fn test_fetch_bytes_retries_transient_statuses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .and(header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let bytes = client
        .fetch_bytes(&format!("{}/files/pkg.bin", server.uri()))
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
}

#[tokio::test]
async fn test_fetch_bytes_retries_body_errors() {
    let base = truncated_then_ok_server(b"artifactbytes", None);
    let client = UpstreamClient::new(&base).unwrap();

    let bytes = client.fetch_bytes(&format!("{base}pkg.bin")).await.unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
}

#[tokio::test]
async fn test_fetch_bytes_limited_retries_body_errors() {
    let base = truncated_then_ok_server(b"artifactbytes", None);
    let client = UpstreamClient::new(&base).unwrap();

    let bytes = client.fetch_bytes_limited(&format!("{base}pkg.bin"), 32).await.unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
}

#[tokio::test]
async fn test_fetch_bytes_limited_reports_exhausted_body_errors() {
    let body = b"artifactbytes";
    let base = response_server(
        vec![(&body[..4], body.len() + 16); usize::try_from(MAX_RETRIES).unwrap() + 1],
        None,
    );
    let client = UpstreamClient::new(&base).unwrap();

    let err = client
        .fetch_bytes_limited(&format!("{base}pkg.bin"), 32)
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream response could not be decoded");
}

#[tokio::test]
async fn test_fetch_bytes_limited_rejects_chunked_body_over_limit() {
    let base = chunked_server();
    let client = UpstreamClient::new(&base).unwrap();

    let err = client
        .fetch_bytes_limited(&format!("{base}pkg.bin"), 9)
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream response exceeds the 9-byte limit");
}

fn truncated_then_ok_server(body: &'static [u8], content_type: Option<&'static str>) -> String {
    response_server(
        vec![(&body[..body.len().min(4)], body.len() + 16), (body, body.len())],
        content_type,
    )
}

fn response_server(responses: Vec<(&'static [u8], usize)>, content_type: Option<&'static str>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for (body, content_length) in responses {
            write_response(listener.accept().unwrap().0, body, content_length, content_type);
        }
    });
    format!("http://{addr}/simple/")
}

fn capture_debug_events() -> (File, DefaultGuard) {
    let capture = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(Mutex::new(capture.try_clone().unwrap()))
        .finish();
    (capture, tracing::subscriber::set_default(subscriber))
}

fn captured_event(mut capture: File, guard: DefaultGuard, message: &str) -> serde_json::Value {
    drop(guard);
    capture.rewind().unwrap();
    let mut text = String::new();
    capture.read_to_string(&mut text).unwrap();
    text.lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|event| event["fields"]["message"] == message)
        .unwrap()
}

fn write_response(mut socket: std::net::TcpStream, body: &[u8], content_length: usize, content_type: Option<&str>) {
    use std::io::{Read as _, Write as _};

    let mut buffer = [0; 1024];
    let _ = socket.read(&mut buffer);
    let mut headers = format!("HTTP/1.1 200 OK\r\ncontent-length: {content_length}\r\nconnection: close\r\n");
    if let Some(content_type) = content_type {
        headers.push_str("content-type: ");
        headers.push_str(content_type);
        headers.push_str("\r\n");
    }
    socket.write_all(headers.as_bytes()).unwrap();
    socket.write_all(b"\r\n").unwrap();
    socket.write_all(body).unwrap();
}

fn chunked_server() -> String {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut socket = listener.accept().unwrap().0;
        let mut buffer = [0; 1024];
        let _ = socket.read(&mut buffer);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n8\r\nartifact\r\n5\r\nbytes\r\n0\r\n\r\n",
            )
            .unwrap();
    });
    format!("http://{addr}/simple/")
}
