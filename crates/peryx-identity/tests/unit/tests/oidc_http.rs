use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread::JoinHandle;

use async_trait::async_trait;
use serde_json::{Value, json};
use url::Url;

use crate::OidcHttpTransport;

pub const MAX_DISCOVERY_BYTES: usize = 65_536;
pub const MAX_JWKS_BYTES: usize = 1_048_576;

pub fn transport(destination: &str) -> Arc<dyn OidcHttpTransport> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    Arc::new(WiremockTransport {
        logical_origin: Url::parse(&secure_origin(destination)).unwrap(),
        destination: Url::parse(destination).unwrap(),
        client: http_client(),
    })
}

pub fn insecure_transport() -> Arc<dyn OidcHttpTransport> {
    Arc::new(InsecureTransport(http_client()))
}

pub fn secure_origin(origin: &str) -> String {
    let mut url = Url::parse(origin).unwrap();
    url.set_scheme("https").unwrap();
    url.to_string().trim_end_matches('/').to_owned()
}

#[derive(Clone, Copy, Debug)]
pub enum TestResponseBody {
    ExactChunked { limit: usize },
    OversizedChunked { limit: usize },
    ExactContentLength { limit: usize },
    OversizedContentLength { limit: usize },
    Truncated,
}

pub struct TestHttpServer {
    address: SocketAddr,
    thread: Option<JoinHandle<()>>,
}

impl TestHttpServer {
    pub fn start(body: TestResponseBody) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        Self {
            address,
            thread: Some(std::thread::spawn(move || serve_once(&listener, address, body))),
        }
    }

    pub fn origin(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_once(listener: &TcpListener, address: SocketAddr, body: TestResponseBody) {
    let (mut socket, _) = listener.accept().unwrap();
    let mut request = [0; 1024];
    let _ = socket.read(&mut request);
    let response = match body {
        TestResponseBody::ExactChunked { limit } | TestResponseBody::OversizedChunked { limit } => {
            let size = if matches!(body, TestResponseBody::ExactChunked { .. }) {
                limit
            } else {
                limit + 1
            };
            let body = padded_json(
                json!({
                    "issuer": format!("https://{address}"),
                    "jwks_uri": format!("https://{address}/keys"),
                    "id_token_signing_alg_values_supported": ["RS256"],
                }),
                size,
            );
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:X}\r\n{body}\r\n0\r\n\r\n",
                body.len()
            )
        }
        TestResponseBody::ExactContentLength { limit } | TestResponseBody::OversizedContentLength { limit } => {
            let length = if matches!(body, TestResponseBody::ExactContentLength { .. }) {
                limit
            } else {
                limit + 1
            };
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n{{}}"
            )
        }
        TestResponseBody::Truncated => {
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 8\r\nconnection: close\r\n\r\n{}"
                .to_owned()
        }
    };
    let _ = socket.write_all(response.as_bytes());
}

pub fn padded_json(mut value: Value, size: usize) -> String {
    value["padding"] = Value::String(String::new());
    let base = serde_json::to_string(&value).unwrap();
    value["padding"] = Value::String("x".repeat(size - base.len()));
    let body = serde_json::to_string(&value).unwrap();
    assert_eq!(body.len(), size);
    body
}

#[derive(Debug)]
struct WiremockTransport {
    logical_origin: Url,
    destination: Url,
    client: reqwest::Client,
}

#[async_trait]
impl OidcHttpTransport for WiremockTransport {
    async fn execute(&self, mut request: reqwest::Request) -> Result<reqwest::Response, reqwest::Error> {
        if request.url().scheme() == self.logical_origin.scheme()
            && request.url().host_str() == self.logical_origin.host_str()
            && request.url().port_or_known_default() == self.logical_origin.port_or_known_default()
        {
            request.url_mut().set_scheme(self.destination.scheme()).unwrap();
            request.url_mut().set_host(self.destination.host_str()).unwrap();
            request.url_mut().set_port(self.destination.port()).unwrap();
        }
        self.client.execute(request).await
    }
}

#[derive(Debug)]
struct InsecureTransport(reqwest::Client);

#[async_trait]
impl OidcHttpTransport for InsecureTransport {
    async fn execute(&self, mut request: reqwest::Request) -> Result<reqwest::Response, reqwest::Error> {
        request.url_mut().set_scheme("http").unwrap();
        self.0.execute(request).await
    }
}

fn http_client() -> reqwest::Client {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap()
}
