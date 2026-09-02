use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;

use crate::client::{BOUNDED_READ_TIMEOUT, UpstreamClient, UpstreamError};

const OK_CHUNKED: &[u8] = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";

/// Bounds a wait for server work the client has already been asked to drive, so a request that never
/// arrives fails its test instead of blocking the suite. The clock runs across every one of these waits,
/// so the bound is real time.
const SERVER_STEP: Duration = Duration::from_secs(30);

#[tokio::test(start_paused = true)]
async fn test_bounded_read_deadline_stops_periodic_chunks() {
    let mut server = ControlledServer::start().await;
    let url = url::Url::parse(&server.url()).unwrap();
    let client = UpstreamClient::new(url.as_str()).unwrap();
    let started = tokio::time::Instant::now();
    let deadline = started + BOUNDED_READ_TIMEOUT;
    let read = client.bounded_read();
    let (response, ()) = tokio::join!(read.send_conditional(url, "application/json", None), async {
        server.requested().await;
        server.write(OK_CHUNKED).await;
    });
    let response = response.unwrap();
    let (body, ()) = tokio::join!(
        read.run(async move { response.bytes().await.map_err(UpstreamError::from) }),
        async {
            for (offset, chunk) in [b"1\r\na\r\n".as_slice(), b"1\r\nb\r\n"].into_iter().enumerate() {
                server
                    .advance_to(started + Duration::from_secs(10 * (offset as u64 + 1)))
                    .await;
                server.write(chunk).await;
            }
            server.advance_to(deadline).await;
        }
    );

    let error = body.unwrap_err();
    assert_eq!(
        (
            tokio::time::Instant::now() - started,
            error.status(),
            error.user_message()
        ),
        (BOUNDED_READ_TIMEOUT, None, "upstream request timed out".to_owned(),)
    );
    assert!(matches!(error, UpstreamError::DeadlineExceeded));
}

#[tokio::test(start_paused = true)]
async fn test_bounded_read_accepts_a_body_near_the_deadline() {
    let mut server = ControlledServer::start().await;
    let url = url::Url::parse(&server.url()).unwrap();
    let client = UpstreamClient::new(url.as_str()).unwrap();
    let started = tokio::time::Instant::now();
    let deadline = started + BOUNDED_READ_TIMEOUT;
    let read = client.bounded_read();
    let (response, ()) = tokio::join!(read.send_conditional(url, "application/json", None), async {
        server.requested().await;
        server.write(OK_CHUNKED).await;
    });
    let response = response.unwrap();
    let (body, ()) = tokio::join!(
        read.run(async move { response.bytes().await.map_err(UpstreamError::from) }),
        async {
            server.advance_to(deadline - Duration::from_secs(1)).await;
            server.write(b"3\r\nabc\r\n0\r\n\r\n").await;
        }
    );

    assert_eq!(body.unwrap(), bytes::Bytes::from_static(b"abc"));
    assert!(tokio::time::Instant::now() < deadline);
}

#[tokio::test(start_paused = true)]
async fn test_bounded_read_retries_with_the_remaining_budget() {
    let mut server = ControlledServer::start().await;
    let url = url::Url::parse(&server.url()).unwrap();
    let client = UpstreamClient::new(url.as_str()).unwrap();
    let started = tokio::time::Instant::now();
    let deadline = started + BOUNDED_READ_TIMEOUT;
    let read = client.bounded_read();
    let (response, ()) = tokio::join!(read.send_conditional(url, "application/json", None), async {
        server.requested().await;
        server.advance_to(started + Duration::from_secs(20)).await;
        server
            .write(
                b"HTTP/1.1 503 Service Unavailable\r\nretry-after: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            )
            .await;
        server.next_response();
        server.requested().await;
        assert!(tokio::time::Instant::now() < deadline);
        server.write(OK_CHUNKED).await;
    });
    let response = response.unwrap();
    let (body, ()) = tokio::join!(
        read.run(async move { response.bytes().await.map_err(UpstreamError::from) }),
        async {
            server.advance_to(deadline - Duration::from_secs(1)).await;
            server.write(b"1\r\na\r\n").await;
            server.advance_to(deadline).await;
        }
    );

    assert!(matches!(body, Err(UpstreamError::DeadlineExceeded)));
    assert_eq!(tokio::time::Instant::now() - started, BOUNDED_READ_TIMEOUT);
}

#[tokio::test(start_paused = true)]
async fn test_bounded_read_run_reports_its_deadline() {
    let client = UpstreamClient::new("https://upstream.example/").unwrap();

    let error = client
        .bounded_read()
        .run(std::future::pending::<Result<(), UpstreamError>>())
        .await
        .unwrap_err();

    assert_eq!(
        (error.status(), error.user_message(), error.to_string()),
        (
            None,
            "upstream request timed out".to_owned(),
            "upstream bounded read deadline exceeded".to_owned(),
        )
    );
}

enum ServerCommand {
    Write(&'static [u8]),
    NextResponse,
}

enum ServerEvent {
    Requested,
    Written,
}

struct ControlledServer {
    address: SocketAddr,
    commands: mpsc::UnboundedSender<ServerCommand>,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    clock_running: bool,
    task: tokio::task::JoinHandle<()>,
}

impl ControlledServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (commands, received_commands) = mpsc::unbounded_channel();
        let (events, received_events) = mpsc::unbounded_channel();
        Self {
            address,
            commands,
            events: received_events,
            clock_running: false,
            task: tokio::spawn(serve(listener, received_commands, events)),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/artifact.bin", self.address)
    }

    async fn requested(&mut self) {
        self.run_clock();
        let event = self.next_event("the client sends its request").await;
        assert!(matches!(event, Some(ServerEvent::Requested)));
    }

    async fn write(&mut self, bytes: &'static [u8]) {
        self.commands.send(ServerCommand::Write(bytes)).unwrap();
        self.run_clock();
        let event = self.next_event("the server writes what it was handed").await;
        assert!(matches!(event, Some(ServerEvent::Written)));
    }

    async fn next_event(&mut self, expectation: &str) -> Option<ServerEvent> {
        tokio::time::timeout(SERVER_STEP, self.events.recv())
            .await
            .expect(expectation)
    }

    /// A paused clock auto-advances to the next timer whenever the runtime parks, so pausing while
    /// bytes the server has already written are still in flight jumps straight to the read
    /// deadline and fails the client for a body it was about to receive. The clock therefore runs
    /// from the moment the server acts until the test deliberately advances it again.
    async fn advance_to(&mut self, target: tokio::time::Instant) {
        if std::mem::take(&mut self.clock_running) {
            tokio::time::pause();
        }
        tokio::time::advance(target.saturating_duration_since(tokio::time::Instant::now())).await;
    }

    fn run_clock(&mut self) {
        if !std::mem::replace(&mut self.clock_running, true) {
            tokio::time::resume();
        }
    }

    fn next_response(&self) {
        self.commands.send(ServerCommand::NextResponse).unwrap();
    }
}

impl Drop for ControlledServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    listener: tokio::net::TcpListener,
    mut commands: mpsc::UnboundedReceiver<ServerCommand>,
    events: mpsc::UnboundedSender<ServerEvent>,
) {
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        events.send(ServerEvent::Requested).unwrap();
        while let ServerCommand::Write(bytes) = commands.recv().await.unwrap() {
            socket.write_all(bytes).await.unwrap();
            events.send(ServerEvent::Written).unwrap();
        }
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) {
    let mut buffer = [0; 1024];
    let mut request = Vec::new();
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = socket.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0, "request ended before its headers");
        request.extend_from_slice(&buffer[..read]);
    }
}
