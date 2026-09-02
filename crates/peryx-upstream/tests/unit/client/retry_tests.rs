use std::fs::File;
use std::io::{Read as _, Seek as _, Write as _};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use rstest::rstest;
use tracing::dispatcher::DefaultGuard;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::guarded_client;
use crate::client::UpstreamClient;
use crate::client::retry::{MAX_HONORED_RETRY_AFTER, MAX_RETRIES, retry_after, retry_after_at};

#[rstest]
#[case::seconds(Some(b"5".as_slice()), Some(Duration::from_secs(5)))]
#[case::zero(Some(b"0".as_slice()), Some(Duration::from_secs(0)))]
#[case::at_cap(Some(b"30".as_slice()), Some(Duration::from_secs(30)))]
#[case::over_budget(Some(b"120".as_slice()), Some(Duration::from_mins(2)))]
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
fn test_retry_after_reads_a_future_http_date_from_receipt_time() {
    let future = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
    let received_at = future - Duration::from_mins(2);
    let mut headers = HeaderMap::new();
    headers.insert(
        RETRY_AFTER,
        HeaderValue::from_str(&httpdate::fmt_http_date(future)).unwrap(),
    );

    assert_eq!(retry_after_at(&headers, received_at), Some(Duration::from_mins(2)));
}

#[test]
fn test_retry_after_ignores_a_past_http_date() {
    let past = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let mut headers = HeaderMap::new();
    headers.insert(
        RETRY_AFTER,
        HeaderValue::from_str(&httpdate::fmt_http_date(past)).unwrap(),
    );

    assert_eq!(retry_after_at(&headers, SystemTime::now()), None);
}

/// Reads the header from the cap itself, so raising the cap moves this test with it and a call site that
/// reached for a different 30-second constant would show up here instead of staying invisible.
#[tokio::test]
async fn test_retry_after_above_budget_returns_the_original_response() {
    let refused = MAX_HONORED_RETRY_AFTER.as_secs() + 1;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", refused.to_string().as_str()))
        .expect(1)
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    let response = client
        .send_conditional(
            url::Url::parse(&format!("{}/simple/", server.uri())).unwrap(),
            "application/json",
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        (response.status(), response.headers()[RETRY_AFTER].to_str().unwrap()),
        (reqwest::StatusCode::TOO_MANY_REQUESTS, refused.to_string().as_str())
    );
}

#[tokio::test]
async fn test_fetch_bytes_honors_retry_after_on_a_retryable_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    let bytes = client
        .fetch_bytes(&format!("{}/files/artifact.bin", server.uri()))
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
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
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
        .and(path("/files/artifact.bin"))
        .and(query_param("token", "secret"))
        .respond_with(ResponseTemplate::new(408).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .and(query_param("token", "secret"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .mount(&server)
        .await;
    let client = guarded_client(&server);
    let (capture, guard) = capture_debug_events();

    client
        .fetch_bytes(&format!("{}/files/artifact.bin?token=secret", server.uri()))
        .await
        .unwrap();

    let event = captured_event(capture, guard, "upstream returned retryable status");
    assert_eq!(
        event["fields"],
        serde_json::json!({
            "message": "upstream returned retryable status",
            "url": format!("{}/files/artifact.bin", server.uri()),
            "status": "408 Request Timeout",
            "delay_ms": "0",
        })
    );
}

#[tokio::test]
async fn test_fetch_bytes_retries_transient_statuses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(ResponseTemplate::new(500).insert_header("retry-after", "0"))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .and(header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"artifactbytes".to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    let bytes = client
        .fetch_bytes(&format!("{}/files/artifact.bin", server.uri()))
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
}

#[rstest]
#[case::unbounded(None)]
#[case::bounded(Some(32))]
#[tokio::test]
async fn test_fetch_bytes_retries_body_errors(#[case] limit: Option<usize>) {
    let server = truncated_then_ok_server(b"artifactbytes");
    let client = UpstreamClient::new(server.base()).unwrap();
    let url = format!("{}artifact.bin", server.base());
    let bytes = if let Some(limit) = limit {
        client.fetch_bytes_limited(&url, limit).await
    } else {
        client.fetch_bytes(&url).await
    }
    .unwrap();

    assert_eq!(&bytes[..], b"artifactbytes");
    server.finish();
}

#[tokio::test]
async fn test_fetch_bytes_limited_reports_exhausted_body_errors() {
    let body = b"artifactbytes";
    let server = response_server(vec![
        (&body[..4], body.len() + 16);
        usize::try_from(MAX_RETRIES).unwrap() + 1
    ]);
    let client = UpstreamClient::new(server.base()).unwrap();

    let err = client
        .fetch_bytes_limited(&format!("{}artifact.bin", server.base()), 32)
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream response could not be decoded");
    server.finish();
}

#[tokio::test]
async fn test_fetch_bytes_limited_rejects_chunked_body_over_limit() {
    let server = chunked_server();
    let client = UpstreamClient::new(server.base()).unwrap();

    let err = client
        .fetch_bytes_limited(&format!("{}artifact.bin", server.base()), 9)
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream response exceeds the 9-byte limit");
    server.finish();
}

fn truncated_then_ok_server(body: &'static [u8]) -> TestServer {
    response_server(vec![(&body[..body.len().min(4)], body.len() + 16), (body, body.len())])
}

fn response_server(responses: Vec<(&'static [u8], usize)>) -> TestServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        for (body, content_length) in responses {
            let mut socket = listener.accept().unwrap().0;
            read_request(&mut socket);
            write_response(socket, body, content_length);
        }
    });
    TestServer::new(addr, thread)
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

fn write_response(mut socket: std::net::TcpStream, body: &[u8], content_length: usize) {
    let headers = format!("HTTP/1.1 200 OK\r\ncontent-length: {content_length}\r\nconnection: close\r\n");
    socket.write_all(headers.as_bytes()).unwrap();
    socket.write_all(b"\r\n").unwrap();
    socket.write_all(body).unwrap();
}

fn chunked_server() -> TestServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let thread = std::thread::spawn(move || {
        let mut socket = listener.accept().unwrap().0;
        read_request(&mut socket);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n8\r\nartifact\r\n5\r\nbytes\r\n0\r\n\r\n",
            )
            .unwrap();
    });
    TestServer::new(addr, thread)
}

struct TestServer {
    base: String,
    thread: std::thread::JoinHandle<()>,
}

impl TestServer {
    fn new(addr: std::net::SocketAddr, thread: std::thread::JoinHandle<()>) -> Self {
        Self {
            base: format!("http://{addr}/api/"),
            thread,
        }
    }

    fn base(&self) -> &str {
        &self.base
    }

    fn finish(self) {
        self.thread.join().expect("server thread joins");
    }
}

fn read_request(socket: &mut std::net::TcpStream) {
    let mut buffer = [0; 1024];
    let mut request = Vec::new();
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = socket.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "request ended before its headers");
        request.extend_from_slice(&buffer[..read]);
    }
}
