use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::simple_client;
use crate::client::UpstreamClient;

fn truncated_then_ok_server(body: &'static [u8], content_type: Option<&'static str>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        write_response(
            listener.accept().unwrap().0,
            &body[..body.len().min(4)],
            body.len() + 16,
            content_type,
        );
        write_response(listener.accept().unwrap().0, body, body.len(), content_type);
    });
    format!("http://{addr}/simple/")
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

#[tokio::test]
async fn test_fetch_bytes_retries_transient_statuses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.whl"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.whl"))
        .and(header("accept-encoding", "identity"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wheelbytes".to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let bytes = client
        .fetch_bytes(&format!("{}/files/pkg.whl", server.uri()))
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"wheelbytes");
}

#[tokio::test]
async fn test_fetch_bytes_retries_body_errors() {
    let base = truncated_then_ok_server(b"wheelbytes", None);
    let client = UpstreamClient::new(&base).unwrap();

    let bytes = client.fetch_bytes(&format!("{base}pkg.whl")).await.unwrap();

    assert_eq!(&bytes[..], b"wheelbytes");
}
