use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread::JoinHandle;

use async_trait::async_trait;
use url::Url;

use crate::OidcHttpTransport;

pub fn transport(destination: &str) -> Arc<dyn OidcHttpTransport> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Arc::new(WiremockTransport {
        logical_origin: Url::parse(&secure_origin(destination)).unwrap(),
        destination: Url::parse(destination).unwrap(),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap(),
    })
}

pub fn secure_origin(origin: &str) -> String {
    let mut url = Url::parse(origin).unwrap();
    url.set_scheme("https").unwrap();
    url.to_string().trim_end_matches('/').to_owned()
}

#[derive(Clone, Copy, Debug)]
pub enum MalformedDiscoveryBody {
    OversizedChunked { limit: usize },
    Truncated,
}

pub struct MalformedDiscoveryServer {
    address: SocketAddr,
    thread: Option<JoinHandle<()>>,
}

impl MalformedDiscoveryServer {
    pub fn start(body: MalformedDiscoveryBody) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        Self {
            address,
            thread: Some(std::thread::spawn(move || serve_once(&listener, body))),
        }
    }

    pub fn origin(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for MalformedDiscoveryServer {
    fn drop(&mut self) {
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_once(listener: &TcpListener, body: MalformedDiscoveryBody) {
    let (mut socket, _) = listener.accept().unwrap();
    let mut request = [0; 1024];
    let _ = socket.read(&mut request);
    let response = match body {
        MalformedDiscoveryBody::OversizedChunked { limit } => {
            let body = "x".repeat(limit + 1);
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:X}\r\n{body}\r\n0\r\n\r\n",
                body.len()
            )
        }
        MalformedDiscoveryBody::Truncated => {
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 8\r\nconnection: close\r\n\r\n{}"
                .to_owned()
        }
    };
    let _ = socket.write_all(response.as_bytes());
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
